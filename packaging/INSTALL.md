# AgentControl — Installation

Archive: `agentcontrol-server-<arch>-linux.tar.gz`
Contents: `agentcontrol-daemon` · `acctl` · `acp-claude` · `ac-wait` · `packaging/` (systemd unit, example config)

## 1. Server (Ubuntu — where the agents run)

```bash
tar xzf agentcontrol-server-*.tar.gz
mkdir -p ~/.local/bin ~/.agentcontrol ~/.config/systemd/user
cp agentcontrol-daemon acctl acp-claude ac-wait ~/.local/bin/
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

Local test: `~/.local/bin/acctl status` → `{"agents":["devin","cursor"],...}`

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

- `+ New agent` → agent, `cwd` on the server, mode/model.
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

### Typical case: backlog-orchestrator (legacy Python → internal)

`backlog-orchestrator` is **no longer a catalog recipe nor a managed
`process`**: the name is reserved for an *internal* daemon component —
the orchestration loop (spawn/nudge/release/cleanup) now runs inside
`agentcontrol-daemon`. A legacy deployment still runs
`~/.local/bin/backlog-orchestrator.py` under
`~/.supervisor/conf.d/backlog-orchestrator.conf` and holds the shared
flock `~/.agentbacklog/orchestrator.lock`. Migrating matters: the
internal component is the only implementation that journals the
presentation-only `[Backlog] item …` worker post (what `first_post`
serves on a worker session) — the Python script can't emit it.

The two implementations never orchestrate together: the flock
serializes (one holder at a time) and both share
`~/.agentbacklog/orchestrator-state.json` (worker counter, nudge
cooldowns, notes items) — the native takes over seamlessly and reposts
each running worker's current wip once (the Python state carries no
`posted_wip` marker).

**Switchover** — enable the internal component **persistently**, then
remove the supervisord program:

```bash
# 1. Enable the internal component (persisted to config.toml AND
#    applied now — the loop starts, sees the flock held by the legacy
#    script and reports phase "blocked" until it exits)
acctl orch set enabled true
acctl orch status                     # → phase: blocked, lock_holder: legacy,
                                      #   item_post: unavailable

#    Runtime-only variants (revert on daemon restart): `acctl orch on`
#    or `acctl comp start backlog-orchestrator`. Config-only variant:
#    `[backlog_orchestrator] enabled = true` in config.toml + daemon
#    restart.

# 2. Remove the legacy supervisord program — the Python script stops
#    and releases the flock; the native takes it at its next tick
#    (≤ poll_secs, 60 s default)
rm ~/.supervisor/conf.d/backlog-orchestrator.conf
supervisorctl reread && supervisorctl update
supervisorctl status                  # backlog-orchestrator gone

# 3. Check the takeover — active phase, lock held by the daemon itself
acctl orch status                     # → phase: active, lock_holder: self,
                                      #   item_post: available
acctl comp ls
```

`[backlog_orchestrator] plan_review = true` additionally opts the
orchestrator into the plan→review→implementation pipeline
(`docs/components-integration.md` §2.11) — default `false` keeps the
legacy direct-worker dispatch; a deployment without the pipeline code
silently ignores the flag.

Pause/resume is unchanged from the legacy script: `touch
~/.agentbacklog/orchestrator.pause` parks the loop (`acctl orch status`
→ `phase: paused`), removing the file resumes it.

Optionally delete the now-dead script
`~/.local/bin/backlog-orchestrator.py` (kept for rollback, nothing
references it any more).

⚠️ Don't merely *stop* the program — remove the conf. A legacy that
comes back later (restored conf, container rebuild) silently re-steals
the flock and the `[Backlog] item` posts stop again: `acctl orch
status` then reports `phase: blocked`, `lock_holder: legacy`,
`item_post: unavailable`.

**Rollback**: `acctl orch set enabled false` (or `orch off` for a
runtime-only stop), restore the supervisord conf, `supervisorctl
reread && supervisorctl update` — same flock, no overlap.

⚠️ The orchestration lives inside the daemon: it pauses while the
daemon is down (workers keep running; spawn/nudge/rm resume on
restart). Same exposure as the supervisord program dying with the
container — just not resurrected by `autorestart` while the daemon is
absent.

## 6. Orchestrator (optional — `backlog-orchestrator`)

The internal `backlog-orchestrator` component drives a shared task
queue: it claims pending items, spawns one worker session per item,
nudges stalled ones, runs the plan→review pipeline and serves PR
reviews. It is **opt-in** (`[backlog_orchestrator] enabled = true`,
or `acctl orch set enabled true` for a persisted toggle) — the daemon
starts fine without it and none of the prerequisites below is needed
otherwise.

Prerequisites:

- **agentbacklog** — the queue CLI the orchestrator shells out to
  (`agentbacklog --list/--pop/…`). Resolved either on the daemon's
  PATH (`uv tool install git+https://github.com/Wameuh/AgentBacklog`)
  or as a managed `tool` component — the catalog recipe does both
  (`component_install agentbacklog`, also offered by the Backlog
  sidebar tab). Without it the component reports
  `waiting: agentbacklog missing` — an explicit phase, not a crash.
- **An available agent profile** — `agent` empty (default) resolves
  to the first `available` `[agents.*]` profile at dispatch
  (auto-discovered ACP adapters count);
  `worker_agent`/`planner_agent`/`reviewer_agent`/
  `pr_reviewer_agent` override it per role. No resolvable profile →
  a clear `no available agent profile` error in `acctl orch status`.
  `mode`/`model`/`thought_level` empty (defaults) let the profile
  decide — the previous named defaults were calibrated for Devin.
- **A forge CLI/REST matching `[[forges]]`** — for the PR-review
  pipeline and the forge calls of worker prompts. GitHub: `gh` on
  the PATH + `gh auth login`. Other forges declared in `[[forges]]`
  (gitlab/gerrit) use their own CLI/credentials there. Not needed
  for pure queue dispatch — only the review pipeline is forge-bound.
- **Workspace** — `workspace` defaults to `git rev-parse
  --show-toplevel` run in the daemon's cwd, else the daemon's cwd
  itself: declare it (and `repo_workspaces` repo→cwd routes for
  repo-tagged items) when the daemon doesn't run inside the work
  repo. `repo_workspaces` is empty by default — single implicit
  workspace.
- **State directory** — `state_dir` (default `~/.agentbacklog`)
  holds `orchestrator.lock` (shared flock — every orchestrator of
  the same queue must point at the same directory),
  `orchestrator.pause`, `orchestrator-state.json`, `plans/`,
  `metrics.jsonl` and the `pr-reviews/` review worktrees.
- **ac-wait** — bash script shipped at the tarball root (same
  install step as `acctl`, §1): worker/reviewer prompts delegate
  any wait over ~1 min to it. Contract: `ac-wait -- <cmd>` polls
  `<cmd>` until exit 0 then pings the session via
  `acctl send --steer` (`--name`, `--interval`, `--timeout`,
  `--until-pid|file|port` variants, `--list/--cancel/--log`); waiter
  state lives under `${XDG_STATE_HOME:-~/.local/state}/ac-wait/` —
  the daemon scans that dir to count parked sessions, it is the
  tool's own XDG convention and stays independent of `state_dir`.
  Needs `acctl` on the spawned agent's PATH.
- **Branch conventions** — worker/reviewer prompts assume the repo's
  own integration workflow (feature branch → PR on the integration
  branch → automated review → merge). The convention itself lives
  in the repo's `AGENTS.md`; a repo without one falls back to the
  generic git flow described by the prompt.

Optional knobs: `plan_review` (plan→review→implementation pipeline —
needs agentbacklog ≥ 0.7), `pr_review` (native `/mr review` serving),
`worker_pool`, `max_workers`… — all hot-settable via
`acctl comp set backlog-orchestrator <key> <value>` or the component's
config form.
