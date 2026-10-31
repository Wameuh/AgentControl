# AgentControl — Installation

Archive: `agentcontrol-server-<arch>-linux.tar.gz`
Contents: `agentcontrol-daemon` · `acctl` · `acp-mock-agent` · `packaging/` (systemd unit, example config)

## 1. Server (Ubuntu — where the agents run)

```bash
tar xzf agentcontrol-server-*.tar.gz
mkdir -p ~/.local/bin ~/.agentcontrol ~/.config/systemd/user
cp agentcontrol-daemon acctl acp-mock-agent ~/.local/bin/
cp packaging/agentcontrol-daemon.service ~/.config/systemd/user/
cp packaging/config.example.toml ~/.agentcontrol/config.toml

systemctl --user daemon-reload
systemctl --user enable --now agentcontrol-daemon
loginctl enable-linger $USER        # daemon stays up with no session open
systemctl --user status agentcontrol-daemon
```

The daemon listens on:
- `127.0.0.1:7331` (loopback → reachable only via SSH tunnel)
- `~/.agentcontrol/daemon.sock` (local)

Local test: `~/.local/bin/acctl status` → `{"agents":["devin","mock"],...}`

### Without systemd (testing)

```bash
AGENTCONTROL_HOME=~/.agentcontrol ~/.local/bin/agentcontrol-daemon
```

### Agent prerequisites

- `devin`: installed + `devin auth login` on the server (`devin acp` command).
- Other ACP agents: edit `[agents.*]` in `~/.agentcontrol/config.toml`.

## 2. Client (your machine)

- **Ubuntu**: install the `.deb`/binary from the `agentcontrol-client-linux`
  artifact (or `cd client && npm i && npx tauri build` from the repo).
- **Windows**: install `.msi`/`.exe` from the `agentcontrol-client-windows` artifact.
- **macOS**: arm64 `.dmg` from the `agentcontrol-client-macos` artifact —
  unsigned, right-click → **Open** on first launch (see README).

### Connection

"Connect" dialog → **SSH**:
- Host: `user@server` — `~/.ssh/config` aliases are suggested;
  an SSH key is required (`BatchMode`, no password prompt →
  `ssh-copy-id user@server` if needed).
- Remote daemon port: `7331` (default).

The client opens `ssh -N -L <local>:127.0.0.1:7331 <host>` automatically.

### Quick tunnel check (CLI)

```bash
ssh -N -L 7331:127.0.0.1:7331 user@server &
AGENTCONTROL_TCP=127.0.0.1:7331 acctl status   # → {"sessions":0,...}
```

## 3. Usage

- `+ New session` → agent, `cwd` on the server, mode/model.
- Close the app → **the agent keeps running** on the server. Reopen →
  full history (paged scrollback), `waiting_input` sessions to resume,
  permissions/questions still answerable.
- `/` in the message field → autocomplete for the agent's slash commands.

## 4. Retention (config.toml)

```toml
[retention]
journal_max_mb = 64    # events.jsonl compaction
journal_keep_mb = 32
blob_max_mb = 512      # tool call output quota (LRU)
purge_days = 0         # purge idle sessions (0=off)
interval_secs = 600
```

## 5. Managed components — migrating a supervisord service

The daemon itself supervises satellite programs: `kind=process`
(spawn/stop/restart, healthcheck, paged logs), `tool`, `external` —
full model in `docs/components-integration.md`. Specs live in
`~/.agentcontrol/components.toml`, a file **owned by the daemon**: edit
it via RPC (`component_upsert`/`component_remove`), never by opening it
live. Annotated example: `packaging/components.example.toml`.

### Typical case: backlog-orchestrator

Currently a service under `~/.supervisor/conf.d/backlog-orchestrator.conf`
(script `~/.local/bin/backlog-orchestrator.py`, 60 s poll, **singleton
enforced by flock** `~/.agentbacklog/orchestrator.lock`).

**Switchover** — write the spec, remove the supervisord conf, **start the
component**. Neither `component_install` nor `component_upsert` launches
the process: `autostart` only applies at daemon boot — without an
explicit `component_start`, the orchestrator polls nothing until the next
restart. The flock makes the transition window safe: as long as the old
instance runs, any second instance exits cleanly.

```bash
# 1. Install the component from the embedded catalog (declarative
#    recipe: requires the script present, lays down the autostart +
#    restart=on-failure + healthcheck=process spec — state: stopped)
acctl comp install backlog-orchestrator   # acctl ≥ item #27

#    Without `acctl comp` (earlier version) — raw NDJSON on the socket:
python3 - <<'EOF'
import json, os, socket
s = socket.socket(socket.AF_UNIX)
s.connect(os.path.expanduser("~/.agentcontrol/daemon.sock"))
f = s.makefile("rw")
for req in ({"id":1,"req":"component_catalog"},
            {"id":2,"req":"component_install","name":"backlog-orchestrator"}):
    f.write(json.dumps(req)+"\n"); f.flush(); print(f.readline().strip())
EOF

# 2. Remove the legacy supervision — the supervisord instance stops
#    and releases the flock
rm ~/.supervisor/conf.d/backlog-orchestrator.conf
supervisorctl reread && supervisorctl update
supervisorctl status                      # backlog-orchestrator gone

# 3. Start the component — the flock is free, the managed one takes over
acctl comp start backlog-orchestrator     # or the RPC below
python3 - <<'EOF'
import json, os, socket
s = socket.socket(socket.AF_UNIX)
s.connect(os.path.expanduser("~/.agentcontrol/daemon.sock"))
f = s.makefile("rw")
f.write(json.dumps({"id":1,"req":"component_start","name":"backlog-orchestrator"})+"\n")
f.flush(); print(f.readline().strip())
EOF

# 4. Check on the daemon side: running, logs flowing
acctl comp ls                             # or component_info/list_components RPC
acctl comp logs backlog-orchestrator
```

"Continuous coverage" variant: start the component **before** removing
the supervisord conf. The managed instance fails on the flock and
`restart=on-failure` retries it (backoff ≤ 60 s) until it gets the lock —
no coverage gap, at the cost of "lock" retries in the log and a non-zero
`restarts` counter.

**Rollback**: `component_remove` (stop + spec removed), restore the
supervisord conf, `supervisorctl reread && supervisorctl update` — no
duplicate there either thanks to the flock.

⚠️ A managed `process` shares the daemon's fate: SIGTERM when the daemon
stops, respawn at boot if `autostart` — for a service that must survive
a daemon restart, declare `kind=external` instead (observation only).

### Versioning the script (recommended)

The `backlog-orchestrator` catalog recipe assumes the script is already
present at `~/.local/bin/` — it isn't versioned anywhere. Dropping it in
**Wameuh/AgentBacklog** or an AgentControl `contrib/` would provide a
canonical downloadable source (`run`/`write_file` recipe instead of
`require_file` + spec-only). Until then, the recipe deliberately stays
**spec-only when the script is detected**: nothing to install, just the
spec to lay down.
