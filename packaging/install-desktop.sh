#!/bin/sh
# Installe l'icône + entrée .desktop pour que la barre des tâches
# affiche la gemme (le DE résout l'icône via l'app_id, pas via set_icon).
# Usage : packaging/install-desktop.sh [chemin-du-binaire]
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${1:-$HOME/.local/bin/agentcontrol}
APPID=dev.agentcontrol.app
mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications" \
  "$HOME/.local/share/icons/hicolor/512x512/apps" \
  "$HOME/.local/share/icons/hicolor/128x128/apps"
cp "$ROOT/client/src-tauri/target/release/agentcontrol" "$BIN" 2>/dev/null \
  || cp "$ROOT/client/src-tauri/target/debug/agentcontrol" "$BIN"
cp "$ROOT/client/src-tauri/icons/icon.png" \
   "$HOME/.local/share/icons/hicolor/512x512/apps/$APPID.png"
cp "$ROOT/client/src-tauri/icons/128x128.png" \
   "$HOME/.local/share/icons/hicolor/128x128/apps/$APPID.png"
cat > "$HOME/.local/share/applications/$APPID.desktop" <<EOD
[Desktop Entry]
Type=Application
Name=AgentControl
Exec=$BIN
Icon=$APPID
Terminal=false
Categories=Development;
StartupWMClass=$APPID
EOD
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
echo "OK — icône + .desktop installés (app_id=$APPID, binaire=$BIN)"
