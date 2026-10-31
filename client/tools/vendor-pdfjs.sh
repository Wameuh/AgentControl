#!/bin/sh
# Revendore les assets pdf.js depuis node_modules/pdfjs-dist vers
# public/pdfjs/ (cmaps, standard_fonts, iccs, wasm + fallbacks nowasm).
# À lancer après chaque bump de pdfjs-dist dans package.json — sinon les
# assets servis dérivent de la version du runtime.
set -eu
cd "$(dirname "$0")/.."
SRC=node_modules/pdfjs-dist
DST=public/pdfjs
[ -d "$SRC" ] || { echo "npm install d'abord" >&2; exit 1; }
rm -rf "$DST"
mkdir -p "$DST"
cp "$SRC/LICENSE" "$DST/"
for d in cmaps standard_fonts iccs wasm; do cp -r "$SRC/$d" "$DST/"; done
echo "pdfjs-dist $(node -p "require('$SRC/package.json').version") vendué → $DST"
