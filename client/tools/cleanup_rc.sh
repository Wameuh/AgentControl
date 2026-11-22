#!/usr/bin/env bash
# cleanup_rc.sh [VERSION] — supprime releases + tags des rc du cycle de
# release, depuis la racine du repo (le script s'y replace lui-même).
#
# Une rc du cycle = tag v<B>-rc.N dont la base B ∈ ]PREV, V] avec
#   V    = version publiée (argument, sinon client/package.json)
#   PREV = plus haut tag stable vX.Y.Z strictement < V — le tag v<V>
#          lui-même, déjà poussé par release-on-merge, est exclu du
#          calcul pour ne pas borner l'intervalle à V.
# Robuste à un changement de niveau en cours de cycle (ex. patch →
# major : v0.10.4-rc.* puis v1.0.0-rc.* cohabitent) : toutes les rc du
# cycle partent, pas seulement v<V>-rc.*. Les rc plus anciennes que le
# stable précédent (cycles passés) et celles au-dessus de V sont
# conservées — le balayage reste borné au cycle courant.
#
# Chaque suppression tente `gh release delete` (release + tag), puis
# l'API git refs (tag orphelin sans release), puis `git push` en
# dernier recours — une rc déjà partie est ignorée (fail-safe : un
# échec de cleanup ne casse jamais la release).
#
# Env : GH_TOKEN requis pour gh ; GITHUB_REPOSITORY (fourni par le
#       runner) pour l'API ; DRY_RUN=1 pour lister sans supprimer.
set -uo pipefail
cd "$(dirname "$0")/../.."

VERSION="${1:-$(grep -oE '"version": "[^"]+"' client/package.json | head -1 | cut -d'"' -f4)}"
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "::error::version semver introuvable (argument ou client/package.json): '$VERSION'"
  exit 1
fi
REPO="${GITHUB_REPOSITORY:-Wameuh/AgentControl}"

git fetch --tags -q

# $1 > $2 en version X.Y.Z (sort -V compare les segments numériquement ;
# l'égalité est exclue explicitement).
version_gt() {
  [ "$1" != "$2" ] \
    && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n1)" = "$2" ]
}

# PREV = plus haut tag stable vX.Y.Z < V (par construction, v<V> est
# exclu : un tag v<V> déjà poussé ne devient pas la borne).
prev=""
while IFS= read -r v; do
  if [ "$v" != "$VERSION" ] && version_gt "$VERSION" "$v"; then prev=$v; fi
done < <(git tag -l 'v*' \
  | sed -n 's/^v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)$/\1/p' \
  | sort -V)

echo "version publiée: $VERSION ; stable précédent: ${prev:-<aucun>}"

# rc du cycle ? base ∈ ]prev, V]
cycle_rc() {
  local base=$1
  ! version_gt "$base" "$VERSION" || return 1
  [ -z "$prev" ] || version_gt "$base" "$prev" || return 1
  return 0
}

for t in $(git tag -l 'v*-rc.*' \
    | sed -n 's/^\(v[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*-rc\.[0-9][0-9]*\)$/\1/p'); do
  base=${t#v}; base=${base%%-rc.*}
  cycle_rc "$base" || { echo "keep $t"; continue; }
  if [ "${DRY_RUN:-}" = "1" ]; then echo "cleanup $t (dry-run)"; continue; fi
  echo "cleanup $t"
  gh release delete "$t" --yes --cleanup-tag 2>/dev/null \
    || gh api -X DELETE "repos/$REPO/git/refs/tags/$t" 2>/dev/null \
    || git push origin ":refs/tags/$t" || true
done
