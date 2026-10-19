#!/usr/bin/env bash
# Déploie agentcontrol-daemon sur un serveur via SSH.
# Usage: ./deploy.sh user@serveur [-p 2222 ...]
#
# Prérequis : binaire compilé pour l'arch cible
#   cargo build --release -p agentcontrol-daemon
#   (ou cross: cargo build --release --target x86_64-unknown-linux-gnu)
set -euo pipefail

HOST=${1:?usage: deploy.sh user@serveur [ssh args...]}
shift || true

BIN=target/release/agentcontrol-daemon
[ -f "$BIN" ] || { echo "compilez d'abord : cargo build --release -p agentcontrol-daemon"; exit 1; }

ssh "$@" "$HOST" 'mkdir -p ~/.local/bin ~/.agentcontrol ~/.config/systemd/user'
scp "$@" "$BIN" "$HOST:~/.local/bin/agentcontrol-daemon"
scp "$@" packaging/agentcontrol-daemon.service "$HOST:~/.config/systemd/user/"
scp "$@" packaging/config.example.toml "$HOST:~/.agentcontrol/config.toml" 2>/dev/null || true
ssh "$@" "$HOST" '
  systemctl --user daemon-reload
  systemctl --user enable --now agentcontrol-daemon
  loginctl enable-linger "$USER" 2>/dev/null || true   # démon actif sans session ouverte
  systemctl --user status agentcontrol-daemon --no-pager | head -5
'
echo "OK — démon actif, socket 127.0.0.1:7331 (tunnel via ssh -L)"
