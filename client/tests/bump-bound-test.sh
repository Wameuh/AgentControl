#!/bin/sh
# Test de la borne de scan de version.yml — bump_version.py --bound.
#   1) Fixture synthétique : bump tagué → commit (major) → bump non
#      tagué → revert `(reset` → fix. La borne doit être le revert :
#      le (major) du cycle annulé ne doit PAS entrer dans la plage.
#   2) Fixture minimale : sans commit candidat, repli = dernier tag
#      semver.
#   3) Historique réel (si présent) : tête d'avant le re-bump erroné
#      de #334 — la borne doit être le revert #435, pas le tag
#      v0.10.3 (qui ré-appliquait le (major) de #422).
# Usage : sh tests/bump-bound-test.sh   — requiert git, python3.
set -u
cd "$(dirname "$0")/../.."
ROOT=$PWD
TOOL="$ROOT/client/tools/bump_version.py"

fails=0
ok() { if [ "$1" = 0 ]; then echo "  PASS $2"; else echo "  FAIL $2"; fails=$((fails+1)); fi; }

comm() { echo x >> f; git add f; git commit -qm "$1"; git rev-parse HEAD; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# --- fixture principale -------------------------------------------------
git init -q "$TMP/repo" && cd "$TMP/repo"
git config user.email t@t.invalid && git config user.name t
echo base > f && git add f && git commit -qm "chore: bump version to 0.9.0"
TAGGED=$(git rev-parse HEAD) && git tag v0.9.0
comm "feat: cassure protocole (major)" >/dev/null
UNTAGGED=$(comm "chore: bump version to 1.0.0")
REVERT=$(comm "chore: revert bump 1.0.0 → 0.9.0 (reset)")
comm "fix: apres le reset" >/dev/null

# borne à la tête = le revert (marqueur `(reset`, tag absent)
B=$(python3 "$TOOL" --bound HEAD)
[ "$B" = "$REVERT" ]; ok $? "borne = commit revert (reset) non tagué"
# la plage exclut le (major) du cycle annulé
if git log --format=%B "$B..HEAD" | grep -q '(major)'; then
  ok 1 "plage borne..HEAD sans (major)"
else
  ok 0 "plage borne..HEAD sans (major)"
fi
# niveau détecté sur la plage = patch (seuls des fix post-reset)
LVL=$(git log --format=%B "$B..HEAD" | python3 "$TOOL" --detect-level)
[ "$LVL" = "patch" ]; ok $? "detect-level sur la plage = patch"

# à la tête du bump non tagué (avant le revert) : bump sauté, borne =
# le bump tagué v0.9.0 — le (major) compte alors (cycle pas annulé)
B2=$(python3 "$TOOL" --bound "$UNTAGGED")
[ "$B2" = "$TAGGED" ]; ok $? "bump non tagué sauté → borne = bump tagué"

# un `(reset` ultérieur redevient la borne même sans revert
RB=$(comm "chore: bump version to 0.10.4 (reset)")
B3=$(python3 "$TOOL" --bound HEAD)
[ "$B3" = "$RB" ]; ok $? "borne = bump (reset) non tagué"

# --- fixture repli : aucun candidat → dernier tag semver --------------
git init -q "$TMP/nobump" && cd "$TMP/nobump"
git config user.email t@t.invalid && git config user.name t
echo a > f && git add f && git commit -qm "feat: initial"
git tag v2.0.0
comm "fix: suite" >/dev/null
B4=$(python3 "$TOOL" --bound HEAD)
[ "$B4" = "v2.0.0" ]; ok $? "repli = dernier tag semver sans candidat"

# --- historique réel ----------------------------------------------------
cd "$ROOT"
# Checkout CI = depth 1 : complet si besoin pour l'historique + tags.
if [ "$(git rev-parse --is-shallow-repository 2>/dev/null)" = "true" ]; then
  git fetch --unshallow -q origin 2>/dev/null \
    || git fetch --depth=5000 -q origin 2>/dev/null || true
fi
git rev-parse -q --verify refs/tags/v0.10.3 >/dev/null 2>&1 \
  || git fetch --tags -q origin 2>/dev/null || true

INCIDENT=1b2a160   # tête de la PR release avant le re-bump erroné
REVERT=91ea0d3     # chore: revert bump 1.0.0 → 0.10.4 (reset) (#435)
if git cat-file -e "$INCIDENT^{commit}" 2>/dev/null \
   && git cat-file -e "$REVERT^{commit}" 2>/dev/null; then
  BR=$(python3 "$TOOL" --bound "$INCIDENT")
  [ "$BR" = "$(git rev-parse "$REVERT")" ]
  ok $? "historique réel : borne à l'incident = revert #435"
  if git log --format=%B "$BR..$INCIDENT" | grep -q '(major)'; then
    ok 1 "historique réel : plage sans (major) de #422"
  else
    ok 0 "historique réel : plage sans (major) de #422"
  fi
  LVR=$(git log --format=%B "$BR..$INCIDENT" \
        | python3 "$TOOL" --detect-level)
  [ "$LVR" = "patch" ]; ok $? "historique réel : niveau = patch (pas de re-bump)"
else
  echo "  SKIP historique réel (commits $INCIDENT/$REVERT absents)"
fi
# sanity : la borne courante de next_release est non vide
BN=$(python3 "$TOOL" --bound HEAD)
[ -n "$BN" ]; ok $? "borne à HEAD non vide ($BN)"

echo ""
if [ "$fails" -eq 0 ]; then echo "Tous les tests passent."; else echo "$fails échec(s)."; fi
exit "$fails"
