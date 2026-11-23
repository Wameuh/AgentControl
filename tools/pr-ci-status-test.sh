#!/usr/bin/env bash
# Test de tools/pr-ci-status — la condition d'attente CI des PR.
# Un faux `gh` (stub) applique le vrai programme --jq à des fixtures
# JSON : on vérifie les verdicts terminaux (DONE/FAIL/REBASE-REQUIRED,
# exit 0) et PENDING (exit 1), pas le polling ac-wait lui-même.
# Usage : bash tools/pr-ci-status-test.sh   — requiert bash, jq.
set -u
cd "$(dirname "$0")/.."
ROOT=$PWD
TOOL="$ROOT/tools/pr-ci-status"

command -v jq >/dev/null || { echo "jq requis"; exit 1; }

fails=0
# t "<desc>" "<condition shell>" — la condition s'évalue dans ce shell
# (peut référencer $rc, $TMP…).
t() { if eval "$2" >/dev/null 2>&1; then echo "  PASS $1"; else echo "  FAIL $1"; fails=$((fails+1)); fi; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# --- stub gh ----------------------------------------------------------------
# Applique le --jq reçu à la fixture JSON du scénario (STUB_DIR/{pr,runs}.json).
mkdir -p "$TMP/bin" "$TMP/case"
cat >"$TMP/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -u
mode="${1:-}"; shift || true
jqprog=""
while [ $# -gt 0 ]; do
  case "$1" in
    --jq) jqprog=$2; shift 2 ;;
    *)    shift ;;
  esac
done
case "$mode" in
  pr)  src="$STUB_DIR/pr.json" ;;
  api) src="$STUB_DIR/runs.json" ;;
  *)   echo "stub gh: mode inconnu '$mode'" >&2; exit 1 ;;
esac
[ -f "$src" ] || { echo "stub gh: fixture absente $src" >&2; exit 1; }
jq -r "$jqprog" "$src"
EOF
chmod +x "$TMP/bin/gh"
export PATH="$TMP/bin:$PATH"
export STUB_DIR="$TMP/case"

# --- fixtures ----------------------------------------------------------------

mkpr() { # $1=state $2=draft $3=mergeable $4=mstate $5=sha
  cat >"$STUB_DIR/pr.json" <<EOF
{"state":"$1","isDraft":$2,"mergeable":"$3","mergeStateStatus":"$4","headRefOid":"$5"}
EOF
}

runjson() { # écrit runs.json depuis des specs "status|conclusion|name" sur stdin
  {
    printf '{"check_runs":['
    first=1
    while IFS='|' read -r st co nm; do
      [ -n "$nm" ] || continue
      [ $first = 0 ] && printf ','
      first=0
      if [ "$co" = "-" ]; then co=null; else co="\"$co\""; fi
      printf '{"name":"%s","status":"%s","conclusion":%s}' "$nm" "$st" "$co"
    done
    printf ']}'
  } >"$STUB_DIR/runs.json"
}

# 7 checks requis verts (daemon-linux ×2, client ×2, client-android,
# client-macos, rust) + 2 advisory exclus (js, audit skipped)
green_runs() {
  cat <<'EOF'
completed|success|daemon-linux (ubuntu-latest, x86_64)
completed|success|daemon-linux (ubuntu-24.04-arm, aarch64)
completed|success|client (ubuntu-latest)
completed|success|client (windows-latest)
completed|success|client-android
completed|success|client-macos
completed|success|rust
completed|skipped|js
completed|skipped|audit
EOF
}

# pending_runs : 6 verts + rust en cours
pending_runs() {
  cat <<'EOF'
completed|success|daemon-linux (ubuntu-latest, x86_64)
completed|success|daemon-linux (ubuntu-24.04-arm, aarch64)
completed|success|client (ubuntu-latest)
completed|success|client (windows-latest)
completed|success|client-android
completed|success|client-macos
in_progress|-|rust
EOF
}

SHA=98be5ea6b4400756f6c330be1fa79107c93e0232

echo "== scénarios terminaux (exit 0 = le waiter notifie) =="

# Les scénarios mergeable sortent avant l'appel check-runs : runs.json
# vide suffit (le stub exige juste que la fixture existe).
runjson </dev/null

mkpr OPEN false CONFLICTING DIRTY "$SHA"
"$TOOL" 42 >"$TMP/out" 2>&1; rc=$?
t "CONFLICTING → REBASE-REQUIRED exit 0" '[ $rc = 0 ] && grep -q "REBASE-REQUIRED" "$TMP/out"'

mkpr OPEN false MERGEABLE DIRTY "$SHA"
runjson </dev/null
"$TOOL" 42 >"$TMP/out" 2>&1; rc=$?
t "mergeStateStatus=DIRTY → REBASE-REQUIRED exit 0" '[ $rc = 0 ] && grep -q "REBASE-REQUIRED" "$TMP/out"'

mkpr MERGED false UNKNOWN UNKNOWN "$SHA"
"$TOOL" 42 >"$TMP/out" 2>&1; rc=$?
t "PR mergée → DONE exit 0" '[ $rc = 0 ] && grep -q "^DONE" "$TMP/out"'

mkpr CLOSED false MERGEABLE CLEAN "$SHA"
"$TOOL" 42 >"$TMP/out" 2>&1; rc=$?
t "PR fermée → FAIL exit 0" '[ $rc = 0 ] && grep -q "^FAIL" "$TMP/out"'

mkpr OPEN true MERGEABLE CLEAN "$SHA"
"$TOOL" 42 >"$TMP/out" 2>&1; rc=$?
t "PR repassée draft → FAIL exit 0" '[ $rc = 0 ] && grep -q "draft" "$TMP/out"'

green_runs | runjson
mkpr OPEN false MERGEABLE CLEAN "$SHA"
"$TOOL" 42 >"$TMP/out" 2>&1; rc=$?
t "7 checks verts → DONE exit 0" '[ $rc = 0 ] && grep -q "^DONE" "$TMP/out"'

{ green_runs | sed '0,/success/s//failure/'; } | runjson
"$TOOL" 42 >"$TMP/out" 2>&1; rc=$?
t "un check requis rouge → FAIL exit 0" '[ $rc = 0 ] && grep -q "^FAIL" "$TMP/out"'

echo "== scénarios pending (exit 1 = le waiter continue) =="

pending_runs | runjson
mkpr OPEN false MERGEABLE CLEAN "$SHA"
"$TOOL" 42 >"$TMP/out" 2>&1; rc=$?
t "check en cours → PENDING exit 1" '[ $rc = 1 ] && grep -q "^PENDING" "$TMP/out"'

mkpr OPEN false MERGEABLE BEHIND "$SHA"
pending_runs | runjson
"$TOOL" 42 >"$TMP/out" 2>&1; rc=$?
t "BEHIND seul → PENDING (non bloquant) exit 1" '[ $rc = 1 ] && grep -q "base a avancé" "$TMP/out"'

mkpr OPEN false UNKNOWN UNKNOWN "$SHA"
pending_runs | runjson
"$TOOL" 42 >"$TMP/out" 2>&1; rc=$?
t "mergeable UNKNOWN → PENDING exit 1" '[ $rc = 1 ] && grep -q "^PENDING" "$TMP/out"'

printf 'completed|success|rust\n' | runjson
mkpr OPEN false MERGEABLE CLEAN "$SHA"
"$TOOL" 42 >"$TMP/out" 2>&1; rc=$?
t "moins de 7 runs → PENDING exit 1" '[ $rc = 1 ] && grep -q "^PENDING" "$TMP/out"'

# gh en échec → pending (retry), jamais de verdict inventé
cat >"$TMP/bin/gh" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
chmod +x "$TMP/bin/gh"
"$TOOL" 42 >"$TMP/out" 2>&1; rc=$?
t "gh en échec → PENDING exit 1" '[ $rc = 1 ] && grep -q "^PENDING" "$TMP/out"'

echo
if [ "$fails" = 0 ]; then echo "tous les tests passent"; else echo "$fails échec(s)"; exit 1; fi
