# Reference deployment — this repository's own AgentControl setup

> **This document describes the author's own deployment** — the infra
> specifics of the AgentControl repository itself: its dev container,
> supervisord services, `next_release` → `main` branch workflow,
> workspace map, and the history of its own orchestrator migration.
> It is an **example of a real deployment, not a requirement**: none of
> the programs, paths or conventions below is needed to use
> AgentControl on another repo. The generic component architecture is
> in `docs/components-integration.md`; install docs in
> `packaging/INSTALL.md`; the release pipeline is documented as an
> optional example in `docs/ci-example.md`.

---

## 1. Ecosystem landscape (this host)

The ecosystem grew organically. Today only the MCP server is integrated
into the daemon; everything else lives alongside, supervised by other
means (dev container supervisord, systemd --user, crontab, nothing at
all).

### 1.1 What the daemon already manages (internal, compiled in)

| Element | Role | Where |
| --- | --- | --- |
| ACP agent sessions | spawn/monitor agent processes, events.jsonl journal + blobs, auto-resume post-crash | `crates/daemon/src/session/`, `server/sessions.rs` |
| `agentcontrol` MCP server | agent↔agent tools (list/send/create/read/sleep) injected via `mcpServers` of session/new\|load; hidden `mcp-serve` subcommand of the same binary, spawned by each agent | `crates/daemon/src/mcp.rs`, injection `server/mod.rs::mcp_servers` |
| PR ↔ session registry | prs.json, per-session watcher (`gh pr create` detection), `pr_list/track/untrack/sync` RPCs (`crates/forgesync` collection) | `crates/daemon/src/prs.rs`, `server/prs.rs` |
| Web UI | loopback HTTP listener + token (`bind_http`), serves the SPA (embed or disk), WS bridge to the NDJSON proto, /rpc /media /download | `crates/daemon/src/web/` |
| Janitor | retention: journal compaction, LRU blob purge, idle session purge | `server/mod.rs::janitor` |
| PTY terminals | one shell per connection, lifecycle bounded to the conn | `server/term.rs` |
| Self-update | `UpdateDaemon`: release download + binary replacement + exit (relaunched by supervisor) | `selfupdate.rs` |
| `acctl` | daemon client CLI (same NDJSON proto) | `crates/acctl` |

### 1.2 What lives alongside (external — on this deployment)

| Program | Role | Current supervision | State/log |
| --- | --- | --- | --- |
| `backlog-orchestrator.py` | **retired on the dev container (#140)** — former 60 s poller (`Backlog-W*` spawns, nudges, orphan releases) superseded by the `internal` `backlog-orchestrator` component, which adopted live workers and the shared `orchestrator-state.json` at switchover; the Python state carries no `posted_wip`, so the native reposts each adopted worker's current wip once as the presentational `[Backlog] item …` `user_message` `origin` (never delivered) | supervisord conf kept **stopped + `autostart=false`** for rollback — removed once the pipeline canary passes | `~/.agentbacklog/orchestrator-{state.json,log,stdout.log,stderr.log}` (frozen at stop); flock `orchestrator.lock` now held by the daemon itself |
| `agentbacklog` (CLI) | FIFO feature queue in SQLite — atomic transactions, **no daemon** | `uv tool` in `~/.local/bin` (repo `github.com/Wameuh/AgentBacklog`) | `~/.agentbacklog/backlog.db` |
| `pr-db` | local sqlite PR registry — **legacy**, replaced by the built-in registry (feat #59) | `~/.local/bin` | `~/.agentcontrol/prs.db` |
| `graphify`, `graphify-mcp` | external uv tools | `~/.local/bin` | — |
| `acp-mock-agent` | fake test ACP agent — only used by `client/tests/first-post-daemon-e2e.py` (`AC_MOCK`/PATH, `/bin/true` fallback); **not in the release tarball** (build.yml ships daemon + acctl + packaging only), legacy installs may still carry it | manual/legacy binary | — |
| `supervisord` | **PID 1 of the dev container**: sshd + cron + agentcontrol-daemon (the backlog orchestrator moved inside the daemon — #140) | conf `/etc/supervisor/conf.d/` + `~/.supervisor/conf.d/` | — |
| `cron`/`crontab` | scheduled jobs | supervised (`cron-spool` volume) | — |
| Review automation | `/mr review` → review session | served by the native orchestrator (`pr_review`) on this deployment | — |
| systemd `agentcontrol-daemon.service` | daemon supervisor in prod (`Restart=always`) | `packaging/` | — |

### 1.3 Managed vs external — the observation

- The daemon already knows how to manage **processes** (agent sessions):
  spawn via `tokio::process`, exit monitoring, on-demand restart,
  injected env. The mechanism exists — it's ACP-specialized.
- Satellite services (orchestrator, future MCPs/scripts) each rebuild
  their own supervision, logs, state and CLI — and are **invisible in
  the client**: you can't see they're running, nor that they're linked
  to sessions.
- The vision: every program in the ecosystem becomes a **declared
  component** — the daemon displays it, offers its configuration, and
  (depending on type) drives its lifecycle; the client provides a
  diagram view of programs + attached sessions.

---

## 2. Component decisions on this deployment

Deployment-specific rows of `components-integration.md` §2.8 — the
entries that exist because of *this* host's setup rather than the
product's built-ins:

| Component | Kind | Why |
| --- | --- | --- |
| `backlog-orchestrator.py` (legacy) | retired | superseded by the `internal` component — dev container migrated in #140, supervisord conf disabled pending removal (`INSTALL.md` §5) |
| pr-db | external/legacy | end of transition (feat #59) — declarable as `external` if still present |
| supervisord / sshd / cron | `external` | container infra — never managed (the daemon doesn't touch its own supervisor) |
| `agentbacklog` | `tool` | `github.com/Wameuh/AgentBacklog` is its canonical upstream — repointable via `[catalog.agentbacklog] repo`/`install_spec` |

## 3. Workspace routing on this deployment

`[backlog_orchestrator] repo_workspaces` maps the backlog `repo` tags
to checkouts on this host (see the generic mechanism in
`components-integration.md` §2.9):

```toml
[backlog_orchestrator.repo_workspaces]
AgentControl = "/workspace"
# + per-hackathon workspaces as needed
```

Items tagged `AgentControl` are worked in `/workspace` (this repo);
untagged items fall back to the default `workspace`. The `AGENTS.md`
workflow (`next_release` → `main`, worktrees under `~/worktrees/`,
review/merge rules) is the rules contract read by every spawned
session.

## 4. Rollout notes — supervisord → internal orchestrator

The switchover staged `enabled = true` + `plan_review = true` in
`config.toml` (#140): the internal component ran legacy dispatch until
the first deployed daemon build carrying the pipeline code, where the
flag took effect at boot — caveat: any `acctl orch set`/`comp set` on a
pre-pipeline daemon rewrites the section from the old schema and
silently drops the staged key.

The migration procedure itself (persistent enable, supervisord conf
removal, shared flock handover, rollback) is in `packaging/INSTALL.md`
§5 — it is kept there for any deployment that still runs the legacy
Python script, and marked as reference-deployment material.

## 5. Implementation history — AgentBacklog items

The component system was broken down into AgentBacklog items
(`docs/internal/coupling-audit.md` covers the later decoupling series #316-322).
Order: A (daemon core) → B (editing+catalog) → {C, D, E, F, G}.

| # | Backlog item | Contents | Depends on |
| --- | --- | --- | --- |
| A | **#24** | daemon — component model + supervisor + read RPCs (`ListComponents/ComponentInfo/Start/Stop/Restart/Logs`, components.toml, logs, healthcheck, capability, **`created_by` attribution field**) | — |
| B | **#25** | daemon — config editing + catalog + install (`ComponentUpsert/Remove/Catalog/Install`, GitHub fetch, agentbacklog + orchestrator recipes) | #24 |
| C | **#27** | acctl — `comp` commands (ls/info/start/stop/restart/logs/catalog/install) | #25 |
| D | **#26** | client — animated SVG diagram view (program+session nodes via `created_by`, states, flows) | #24 |
| E | **#28** | client — component management panel (list, actions, logs, spec editing, catalog/integration tab) | #25 |
| F | **#29** | orchestrator — supervisord → managed component migration (spec, migration doc, conf removal after switch) | #25 |
| G | **#30** | `tool` kind — detection/version + passive catalog nodes (agentbacklog in the diagram) | #25 |

### Pickup criteria (this repo's workflow)

Each item was self-contained: dedicated worktree based on
`origin/next_release`, full spec in its backlog description, draft PR →
`/mr review` → ready → required checks green (5 required by the ruleset
out of the workflow's 6 runs, `client-macos` pending inclusion — gating
on the head SHA's check-runs, per `AGENTS.md`). The `description`
field carries: affected files, exact types/reqs schema, verifications
(`cargo build --release -p agentcontrol-daemon` / `npm run build`).
