#!/usr/bin/env bash
# Deploy agentcontrol-daemon to a server over SSH.
# Usage: ./deploy.sh user@server [-p 2222 ...]
#
# Prerequisite: binary built for the target arch
#   cargo build --release -p agentcontrol-daemon
#   (or cross: cargo build --release --target x86_64-unknown-linux-gnu)
# Server side: systemd --user (loginctl linger for persistence).
# Without systemd, start the daemon by hand or adapt the supervision —
# see packaging/INSTALL.md §1 "Without systemd".
set -euo pipefail

HOST=${1:?usage: deploy.sh user@server [ssh args...]}
shift || true

BIN=target/release/agentcontrol-daemon
[ -f "$BIN" ] || { echo "build first: cargo build --release -p agentcontrol-daemon"; exit 1; }

ssh "$@" "$HOST" 'mkdir -p ~/.local/bin ~/.agentcontrol ~/.config/systemd/user'
scp "$@" "$BIN" "$HOST:~/.local/bin/agentcontrol-daemon"
scp "$@" packaging/agentcontrol-daemon.service "$HOST:~/.config/systemd/user/"
# Never clobber an existing config.toml — redeploys must keep the
# server's agents/orchestrator settings. Two-phase install: transfer
# to a temp file first (a cut transfer never leaves a partial
# config.toml), then publish with `ln` — an atomic create-or-fail, so
# a config.toml that already exists or appears mid-deploy is never
# overwritten. The example is still pushed under its own name for
# post-upgrade diffs.
DEPLOY_TMP='~/.agentcontrol/.config.toml.deploy'
scp "$@" packaging/config.example.toml "$HOST:$DEPLOY_TMP"
rc=0
ssh "$@" "$HOST" "ln $DEPLOY_TMP ~/.agentcontrol/config.toml" || rc=$?
if [ "$rc" -eq 0 ]; then
  echo "config.toml installed from example on $HOST"
elif ssh "$@" "$HOST" 'test -e ~/.agentcontrol/config.toml'; then
  echo "config.toml already present on $HOST — left untouched"
else
  ssh "$@" "$HOST" "rm -f $DEPLOY_TMP" || true
  echo "cannot install config.toml on $HOST — aborting" >&2
  exit 1
fi
ssh "$@" "$HOST" "rm -f $DEPLOY_TMP" || true
scp "$@" packaging/config.example.toml "$HOST:~/.agentcontrol/config.example.toml"
ssh "$@" "$HOST" '
  systemctl --user daemon-reload
  systemctl --user enable --now agentcontrol-daemon
  loginctl enable-linger "$USER" 2>/dev/null || true   # daemon active without an open session
  systemctl --user status agentcontrol-daemon --no-pager | head -5
'
echo "OK — daemon active, socket 127.0.0.1:7331 (tunnel via ssh -L)"
