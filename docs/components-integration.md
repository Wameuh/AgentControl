# Integrating components into the daemon — architecture

> Design document: the daemon-managed "component" model, proto
> surface, client surfaces and diagram view — the **generic**
> architecture, valid on any install. What is specific to this
> repository's own deployment (its satellite programs, workspace map,
> migration history) lives in `docs/reference-deployment.md`.

---

## 1. Why a component model

The daemon already knows how to manage **processes** (agent sessions):
spawn via `tokio::process`, exit monitoring, on-demand restart,
injected env. The mechanism exists — it's ACP-specialized.

Satellite services around it (queue orchestrators, review watchers,
MCP servers, scripts…) each rebuild their own supervision, logs,
state and CLI — and are **invisible in the client**: you can't see
they're running, nor that they're linked to sessions.

The model: every program in the ecosystem becomes a **declared
component** — the daemon displays it, offers its configuration, and
(depending on type) drives its lifecycle; the client provides a
diagram view of programs + attached sessions. A concrete inventory of
the components deployed on this repository's own setup is kept in
`docs/reference-deployment.md`.

---

## 2. Target architecture

### 2.1 Component model: four `kind`s

```toml
# <root>/components.toml — file owned by the daemon
# (config.toml stays hand-edited; components.toml is atomically
#  rewritten by the editing RPCs)

[[components]]
name = "agentreviewer"
kind = "process"                 # internal | process | external | tool
command = ["~/.local/bin/agent-reviewer", "--watch"]
cwd = "~"
autostart = true
restart = "on-failure"           # always | on-failure | never
restart_sec = 5                  # base backoff (exponential ×2, 60 s cap)
healthcheck = { kind = "process" }  # none | process | tcp | http | command
description = "Auto-review watcher driving this daemon"

[components.env]
AGENTCONTROL_TCP = "127.0.0.1:7331"

[[components]]
name = "agentbacklog"
kind = "tool"                    # stateless CLI: no process
command = ["agentbacklog", "--version"]
description = "FIFO feature queue (SQLite ~/.agentbacklog/backlog.db)"
```

| `kind` | Meaning | Lifecycle | Examples |
| --- | --- | --- | --- |
| `internal` | feature compiled into the daemon | no process; state derived from config (web active if `bind_http`, a2a if enabled…) | mcp-serve, prs, web, janitor, backlog-orchestrator, agents |
| `process` | long-running program managed by the daemon | spawn/stop/restart/healthcheck/logs | agentreviewer, future MCPs/scripts |
| `external` | declared, managed elsewhere | observed via healthcheck only — the daemon never touches its lifecycle | supervisord, sshd, cron, review-bot, systemd unit |
| `tool` | stateless CLI/install | presence + version (`command --version` probe); no long process — `installed`/`absent` states | agentbacklog, acctl, graphify |

`internal` components are **declared in code** (not in components.toml)
and appear in `ListComponents` like the others — so the diagram shows
the whole ecosystem at once.

`tool`s have **no lifecycle** — `ComponentStart/Stop/Restart` are
refused. Their `state` is an observation: `installed` when argv0 is
resolvable (daemon PATH or executable path), `absent` otherwise — not a
failure, just "not there". The periodic probe (30 s) runs `command`
(typically `… --version`, 3 s timeout) and publishes the first line as
`status.version` (≤200 c) — the `installed` state itself is re-probed
on demand, fresh even before the first pass. Health and version are
only retained for an `installed` tool: `absent` invalidates both caches
(the state is self-sufficient — no residual "health: fail" on
reinstall), just like a spec upsert, until the next pass.

**Managed-tool version** (`process`/`external` too): a spec may declare
`version_argv = ["agent-reviewer", "--version"]` — the argv probed on
the same 30 s cadence, lifecycle-independent (an installed version is
displayed even for a stopped process). For a `tool`, `version_argv`
overrides `command` as the probe. `min_version = "0.3"` declares a
floor: a probed version below it is reported `version_outdated` and the
health verdict becomes `"outdated"` — an explicit degraded signal
("update the tool"), not a silent failure. The catalog stamps both
fields on `suggested_spec` (agentreviewer: `version_argv` +
`min_version = "0.3"`, the first multi-repo release); a catalog-backed
component whose persisted spec predates the fields inherits them from
the homonymous entry at read time. Probe failure/absent → no version,
no error.

### 2.2 Lifecycle of a managed `process`

- **Spawn**: `tokio::process::Command` at daemon boot if `autostart`,
  else on demand (`ComponentStart`). `setsid` isn't needed: the process
  dies with the daemon, like agents.
- **Automatically injected env** (like `agent_parts` for a2a):
  `AGENTCONTROL_SOCK`, `AGENTCONTROL_TCP`, `AGENTCONTROL_HOME`,
  `AGENTCONTROL_COMPONENT=<name>` → a component discovers the daemon
  with no config, on any install. `AGENTCONTROL_COMPONENT` also serves
  session attribution (§2.3.1).
- **Restart**: `on-failure`/`always` with exponential backoff
  (`restart_sec` base, ×2, 60 s cap, reset after 5 stable min);
  `restarts` counter + `last_exit` exposed.
- Periodic **healthcheck** (30 s): `process` = pid alive; `tcp`/`http` =
  probe; `command` = small executed check — a `command` spec is thus
  **periodic** execution (not on-demand) at the same trust level as
  `NewSession` (§3). State: `running | degraded | stopped | failed |
  starting`.
- **Logs**: stdout/stderr redirected to
  `<root>/components/<name>.log`, bounded rotation (1 MiB ×2).
  `ComponentLogs` does paged tail.
- **Daemon shutdown**: SIGTERM to managed processes — same fate as
  agents (interrupted on daemon restart, relaunched at boot if
  autostart), with a gentler signal than the agents' `kill_on_drop`.
  A component that must **not** be interrupted by a daemon restart →
  `kind = "external"`.
- **Orphans** (daemon SIGKILLed): pidfile
  `<root>/components/<name>.pid`. At boot: dead pid → clean; live pid →
  V1 kill + respawn (deterministic — avoids an orphan process keeping
  fds/state). `adopt` option possible later.
- **Guard**: refuse a `process` spec whose command points to the daemon
  binary itself, `supervisord` or `systemctl` — the component system
  manages neither its own supervisor nor itself.

### 2.3 Proto surface

New `ReqKind`s (NDJSON, like the rest — backward-compatible addition:
an old client ignores them; an old daemon answers `parse: unknown
variant` with the `id` correctly extracted, so the call fails cleanly):

| ReqKind | Response |
| --- | --- |
| `ListComponents` | `[{name, kind, state, pid?, autostart, health, uptime_s?, restarts, last_exit?, version?, version_min?, version_outdated?, description?, source?}]` |
| `ComponentInfo {name}` | full spec + detailed state |
| `ComponentStart/Stop/Restart {name}` | `{name, state}` |
| `ComponentLogs {name, offset?, limit?}` | `{data(b64)|lines, size, truncated}` (paged tail) |
| `ComponentUpsert {name, spec}` | atomically rewrites components.toml — bounded user surface: on a catalog-covered name, the internal spec (kind/command/env/cwd/restart*/healthcheck/queries/version_argv/min_version) is sealed, only `autostart`/`description` and declared params are mutable; creating a catalog name refused (→ `ComponentInstall`) |
| `ComponentRemove {name}` | same (+ stop if process alive) |
| `ComponentCatalog {refresh?}` | catalog entries (§2.5) — declared `params`/`render` included |
| `ComponentInstall {name, params?}` | runs the recipe + upserts the spec (params validated first) |
| `ComponentSetParams {name, params}` | merge + validation, re-renders config, restarts if alive |

Capability: `daemon_status.components = true` → the client gates its
buttons like `terminal`/`first_post`. The commit adding the ReqKinds
carries the `(sync)` keyword (new client↔daemon surface).

Push: no daemon-wide event channel (current `Subscribe` is per-session)
— the client **polls** (`ListComponents` + `ListSessions`, ~3 s while
the view is open), like the existing connections watchdog (10 s). A
`component_status_changed` push will come in V2 if polling proves
insufficient.

#### 2.3.1 Component ↔ session attribution — the diagram's missing link

The diagram view attaches sessions to their parent program; for that
the daemon must know **which component** created a session. Today
`spawned_by` only covers the session→session case (`PeerSpawn` requires
`from` = a live session and sets `spawned_by = Some(from)`;
`NewSession` sets `None`): a `process` component creating workers via
`acctl` would produce sessions attached to an invisible pivot session
or to the root — **never to the component node** (the
`AGENTCONTROL_COMPONENT` env lives component-side, the daemon never
sees it again on the RPC).

Mechanism: a **`created_by`** field on creation requests (`NewSession`,
marginal `PeerSpawn` — `AttachSession` likewise if needed: attaching a
native session under a component node), persisted in `SessionMeta` and
exposed by `ListSessions`/`SessionInfo` — values `component:<name>` |
`service:<name>` | `user` (default). The component knows its name via
`AGENTCONTROL_COMPONENT` (§2.2) and re-transmits it — e.g. the
orchestrator `acctl new … --as backlog-orchestrator` produces
`Backlog-W*` attached to the orchestrator node in the diagram.
Cheap daemon-side validation (spec #24): only accept `component:<name>`
if `<name>` appears in components.toml — the field stays self-declared
(same trust as `NewSession`), but a component can't attribute itself a
nonexistent name.

This is **the same channel as item #21's external provenance** (spawn
by external services, in progress): both work items describe the same
need — #24's spec field is the one #26 consumes for edges; if #21 has
already settled on a shape (`origin`) by the time #24 lands, it wins
and the diagram aligns — otherwise #24's choice (`created_by`) fixes
the name consumed by #26. Integrate into the model from #24 to avoid a
second proto evolution (a second `(sync)` cycle).

### 2.4 acctl

`acctl comp <sub>` — mirrors the RPCs for terminal use (agents, scripts,
the orchestrator itself):

```
acctl comp ls [--json]            # name, kind, state, pid, uptime, restarts
acctl comp info <name>            # spec + state
acctl comp start|stop|restart <name>
acctl comp logs <name> [--tail N]
acctl comp catalog [--json] [--refresh]  # + upstream versions
acctl comp install <name> [--set K=V …] # catalog recipe → managed spec
acctl comp set <name> <key> <value>     # declared param (cf §2.5.1)
acctl comp add <name> --kind K --cmd "<argv>" [--env K=V …] [--autostart]
acctl comp edit <name> [--desc D] [--autostart|--no-autostart] [add flags]
                                  # partial edit — catalog component:
                                  # only desc/autostart (params via comp set);
                                  # custom: all add fields + --restart/--rsec
acctl comp rm <name>
```

### 2.5 Catalog & installation — the "integration options"

Goal: **on another install**, offer to integrate a known component in a
few clicks — its metadata (description, version, doc) fetched from its
upstream source.

- **Embedded catalog** in the daemon (const table or packaged JSON):
  `name, description, repo ("<owner>/<repo>"), homepage,
  suggested_spec (kind/command/env/healthcheck prefilled), recipe`.
- **Upstream fetch** (`ComponentCatalog refresh: true`):
  `ureq` (already a daemon dep, rustls TLS) →
  `api.github.com/repos/<repo>/releases/latest` (tag + body); fallback
  `releases` → `tags` → HEAD. Result cached a few minutes.
- **Declarative recipes** audited in code (never an arbitrary command
  sent by the client): typed steps `run {argv}` /
  `write_file {path, content}` / `upsert_spec {}`. `run` argv may carry
  `{{key}}` placeholders — each is a declared catalog `param` resolved
  at install time (provided > `[catalog.<name>]` config > declared
  default); the consumed keys land in the spec's `params` so the
  installed component keeps its provenance. Examples:
  - `agentbacklog`: `uv tool install {{install_spec}}` — `install_spec`
    defaults to `git+https://github.com/<canonical repo>` but accepts
    an index package name, a `git+<url>` or a local path → the source
    is a parameter, never a URL sealed in the argv + `tool` spec.
  - `agentreviewer`: same `uv tool install {{install_spec}}` shape
    (packaged upstream: pyproject + console script) + rendered
    `config.toml` + `process` spec (`agent-reviewer --watch`, in-process
    loop — it receives the injected `AGENTCONTROL_SOCK`/`AGENTCONTROL_TCP`
    and creates its reviews as `origin=component:agentreviewer`
    sessions, "AgentReviewer" group; SIGTERM from `component stop`
    reaches the real process, no external loop orphan).
- **Catalog overrides** (`[catalog.<name>]` in `config.toml`):
  `repo` (upstream probed for `latest_version`, GitHub `owner/name`
  format), `homepage` (doc link shown by the client — derived from
  `repo` when only the repo is repointed) and `params` (per-key
  defaults for the entry's declared params). Point a recipe at a
  fork/mirror without rebuilding: `[catalog.agentbacklog] repo =
  "acme/AgentBacklog"` or `params.install_spec =
  "git+https://git.acme.corp/ab"`. Unknown entries/params warn at
  boot and are ignored — no silent failure.
- `backlog-orchestrator` (**superseded — no catalog entry**): the
  name is reserved for the *internal* component — the orchestration
  loop was ported into the daemon itself; `component_install`/
  `component_upsert` on it fails `reserved component name`. This
  repository's own legacy-Python→internal migration is documented in
  `docs/reference-deployment.md` and `packaging/INSTALL.md` §5.
- **Detection**: the catalog marks each entry `installed` (command
  found on the daemon PATH / file present) + version if probeable → the
  client shows "installed v0.2 — integrate?" vs "available".
- Future components = one catalog entry, no new client code.

#### 2.5.1 Parameter surface — the only user config

A component installed from the catalog exposes **only** its declared
parameters: the internal spec (`command`, `kind`, `restart`,
`healthcheck`) remains the catalog entry's property and isn't user
configuration.

- **`ParamSpec`** declared per entry (`component_catalog` →
  `entry.params`): `{key, label, description?, required, kind,
  default?, secret?, validation?, choices?}` with
  `kind ∈ string | path | int | duration | bool | enum | list | json`
  (`validation` = regex for string/path, `choices` for enum).
  `json` = free-form JSON value — syntactically validated (`null`
  forbidden everywhere, inexpressible in TOML), stored in compact
  canonical form, rendered as recursive TOML literal (object → inline
  table, sorted keys) — the type for structured parameters (lists of
  tables).
- **Install**: `ComponentInstall {name, params?}` — collected values
  (client dialog, `acctl comp install --set K=V`) are validated
  **before** the recipe steps; the materialized spec inherits
  `catalog` (entry name), `params_schema`, `params`, `render` — it's
  self-contained from then on.
- **Edit**: `ComponentSetParams {name, params}` / `acctl comp set
  <name> <key> <value>` — merge, validation by kind + regex, config
  re-render and **restart if the process runs**. `""` removes the key;
  an unknown key is refused with the list of declared ones.
- **`config_required`** (`status`): `required` params with no effective
  value (neither set nor defaulted) — while the list isn't empty spawn
  is refused (autostart disarmed, `comp start` → explicit error) and
  rendering frozen; `comp ls` shows "configuration required: repo,
  repo_dir".
- **Rendering** (`spec.render = {path, template}`): TOML template with
  `{{key}}` placeholders — the literal is typed (string/path/enum/
  duration → escaped `"…"`, `~` expanded; bare int; bool; list →
  `["…"]`) and the line is **omitted** when the parameter stays
  valueless (component default). Bounded write under HOME, atomic,
  0600 if a `secret` is declared.
- **Secrets**: `params` returned masked (`"••••••"`) in RPC responses —
  components.toml keeps cleartext since the rendered file contains it
  anyway.
- **`params` in response = effective values** (defaults ∪ set values),
  not the stored diff.

`agentreviewer` example: `repo*` + `repo_dir*` required;
`worktree_root` (`~/.local/state/agentreviewer/worktrees`), `repos`
(json — list of `{repo, repo_dir, worktree_root?, trigger?, group?,
max_parallel?, agent?}` objects rendered `repos = [ { … } ]`,
extra repos of multi-repo agent-reviewer ≥ 0.3), `trigger`
(`/mr review`), `group` (`AgentReviewer`), `max_parallel` (1),
`review_timeout` (3h), `keep_sessions`, `mode` (enum daemon|systemd,
empty = auto), `daemon`, `agent.command` (`devin`), `agent.profile`,
`agent.model` — rendered into `~/.config/agentreviewer/config.toml`.

#### 2.5.2 Local pre-push review — agent surface

Upstream ≥ 0.3 exposes a generic local review CLI:
`agent-reviewer [--config cfg] review --path <worktree> --base <ref>
[--out FILE] [--timeout SEC] [--detach]` — the `<base>...HEAD` diff is
already materialized (no worktree created), the full review goes to
stdout and the exit code encodes the verdict: `0` APPROVED, `4`
CONDITIONAL, `5` REJECTED, `3` no verdict, `2` usage/infra.
Backend: daemon session (`new_session`, "AgentReviewer" group, verdict
re-read via `events`, `kill_session` on timeout, `keep_sessions`
honored) when an endpoint is reachable, foreground agent otherwise — no
PR nor forge comment. Use case: a backlog worker has its diff reviewed
before pushing.

AgentControl-side surface — settled choice: **documenting the CLI path
in the built-in worker prompt** (`server/orch.rs::PROMPT`, overridable
via `[backlog_orchestrator] prompt`). The worker calls the CLI like an
ordinary tool; in `daemon` mode the upstream review session shows up in
the "AgentReviewer" group like PR reviews. The alternatives were
dismissed: an audited `queries` is a fixed synchronous argv (no
parameters, short timeout) — a review is long and parameterized; a
dedicated component RPC/action would add proto surface for a need
covered by the already-installed binary (the component stays
self-contained, zero extra coupling — AgentReviewer keeps its generic
interface).

### 2.6 Client UI — "Components" panel

Full `#chat` overlay view (`settings-view` pattern) opened from a
titlebar/menu ⋮ button:

- **List** of the current connection's components: name, kind badge,
  animated state, uptime, restarts; contextual actions
  start/stop/restart + log viewer (auto-refresh tail).
- **Editing**: two surfaces depending on component provenance.
  - *Catalog component* (`item.catalog`): the form only exposes the
    **declared params** (`spec.params_schema` — kinds
    string/path/int/duration/bool/enum/list, `secret` masked) +
    `autostart`/`description`; the internal spec (command, env, restart,
    healthcheck, kind) is the catalog entry's property — shown
    **read-only** in an "internals" panel for debugging, and sealed
    daemon-side (`upsert_edit`). Values go through the dedicated params
    RPC; `status.config_required` lists missing required keys
    ("configuration required" banner).
  - *Custom component* (no catalog entry, e.g. hand-written spec): full
    legacy spec form — documented choice: outside the catalog there are
    no declared params, the raw spec stays the only coherent surface.
- **Catalog tab**: cards of known entries (name, description, fetched
  upstream version, "installed?"), "Integrate" button →
  `ComponentInstall` then form prefilled from the `suggested_spec`.
- Capability-gated on `components: true`.

### 2.7 Animated diagram view

The "living" piece: **graph of programs and sessions**.

- **Nodes**: the daemon at the center; components in a ring (icon per
  kind); sessions grouped around their parent — the component node when
  `created_by` = `component:<name>` (§2.3.1: `Backlog-W*` workers under
  the orchestrator), the parent session when `spawned_by`, the daemon
  otherwise.
- **Edges**: daemon→managed component (`manages`), component→daemon
  (RPC — the orchestrator talks to the socket), component→session
  (`created_by`), daemon→session (`spawned`), session→session
  (`spawned_by`/`peer_message`).
- **Animated states**: pulse on `working` sessions, colored dot per
  state (idle/working/waiting/dead/degraded/stopped), animated dashed
  edges on active flows. `tool`s are **passive nodes**: dedicated icon,
  `installed` = subtle outline, `absent` = node and edge dashed, nearly
  faded out (observation, not failure), probed version in tooltip +
  info card. Tooltips: title, state, uptime; session click → opens
  chat; component click → §2.6 panel.
- **Technique**: **vanilla TS SVG** (the client has no graph dep — no
  external lib). Deterministic radial layout (daemon center, components
  on a circle, sessions on arcs around their parent) — no
  force-directed: stable, readable, sufficient (< ~100 nodes). CSS
  animations (`stroke-dashoffset`, `transform`) — no canvas.
- **Data**: `ListComponents` + `ListSessions` + `PrList` polled ~3 s
  only while the view is open.
- Integration: new `map-view` overlay next to `settings-view` (same
  open/close mechanics + `nav.push`), opened via the ⋮ menu and a
  titlebar button.

### 2.8 Choosing a `kind` — settled choices

The table below uses the built-in components as the worked example —
generic selection logic: `internal` for what is already compiled in,
`process` for daemons that should live and die with this daemon,
`external` for infra that outlives it, `tool` for stateless CLIs.

| Component | Decision | Why |
| --- | --- | --- |
| mcp-serve | `internal` | already a binary subcommand — the diagram counts instances per session |
| PR registry | `internal` | already in the daemon |
| web UI | `internal` | `bind_http` feature — state = active/inactive |
| janitor | `internal` | internal task |
| backlog-orchestrator | **`internal`** | the managed-`process` plan was superseded: the loop was ported into the daemon (better: direct session calls, `AGENTCONTROL_*` env unnecessary, no process to lose) |
| agents | `internal` | agent profiles (`[agents.*]`) are daemon config, not a process — the component only exposes their editable surface: one `list` param per profile (argv CSV), persisted as the full `[agents.<id>]` section of config.toml and hot-reloaded (`component_set_params` / `acctl comp set agents <id> <argv-csv>`) |
| agentbacklog | `tool` | stateless CLI — passive node + version + catalog install |
| acctl | `tool` | same |
| a host supervisor (supervisord/systemd/cron) | `external` | container/host infra — never managed (the daemon doesn't touch its own supervisor) |
| agentreviewer | **managed `process`** | watcher of the same daemon it drives — `AGENTCONTROL_*` env injected, reviews as grouped sessions (a watcher of another daemon/host would stay `external`) |
| the daemon itself | root node, not a component | the system doesn't supervise itself |

### 2.9 "Manager" — per-workspace backlog stewardship

When the `agentbacklog` component is declared **and** the native
orchestrator is active (`enabled`), the daemon exposes
`orchestrator.workspaces` — the **repo → cwd** routing
(`[backlog_orchestrator] repo_workspaces`, empty by default — a
deployment maps its own tags, e.g. `{"MyProject": "/srv/checkout"}`)
— and the Backlog view offers a Manager per entry:
"Create" button → `manager_spawn{repo}` RPC → `Manager · <repo>`
session (`origin: feature:backlog-manager`, built-in `MANAGER_PROMPT`
overridable via `manager_prompt`). The Manager is stewardship —
turning requests into structured items (`--add --repo` `--depends-on`,
ordered breakdown), answering status questions, adjusting the queue —
never a worker: no `--pop`/`--done`, and its title escapes the
`title_prefix` (on top of the workers filter's `origin` exclusion →
never nudge/rm/PR guard). Idempotent spawn (`manager_lock` +
origin+title+live-state dedup) — an existing Manager is returned as-is;
`rename_session` breaks the dedup (accepted: the title is the
contract).

The same `repo_workspaces` map routes **workers**: when the installed
`agentbacklog` supports `--claim` (≥ 0.7), each tick the orchestrator
picks the next poppable item itself (priority DESC, id — the `--pop`
order), spawns the worker with `cwd = repo_workspaces[repo]`
(`workspace` for untagged items) and claims the item for that session —
a worker can no longer pick an item belonging to another workspace.
Untagged items keep the historical self-`--pop` fallback on old CLI
versions. A tagged item whose tag has **no** mapping stays pending
(never dispatched into the wrong checkout); the Backlog view warns via
`orchestrator.routed_tags`, and the orchestrator status lists the
waiting tags. Worker prompts are templated per item — `{WS}`, `{REPO}`,
`{ID}`, `{ITEM}` — so the routed rules files (`{RULES}` —
`rules_files`, default `AGENTS.md`/`CLAUDE.md`) and PR instructions
always come from the routed workspace. Plan-review roles (planner,
reviewer, implementer) and failover replacements inherit the same
routed cwd.
`max_workers`/`max_working` stay global caps — routing changes the
cwd, not the pool accounting.

### 2.10 Backlog metrics — sampled activity series

The native orchestrator samples the queue at the end of each
successful tick (`server/metrics.rs`, flag
`[backlog_orchestrator] metrics = false` to cut the sampler without
cutting the RPC): one JSONL sample appended to
`<state_dir>/metrics.jsonl` (`state_dir` default `~/.agentbacklog`) —
`{ts, pending, poppable, wip, workers_alive, by_repo}`.
`workers_alive` counts live sessions with
`origin: component:backlog-orchestrator`. Retention is bounded (30 d
and ~100 k samples, atomic `.part`+`rename` compaction, periodic or
size-triggered). Orchestrator off/paused/blocked → no samples: the
series lives with the orch — there is no standalone collector.

`backlog_metrics{window}` (`acctl orch metrics [--window 24h|7d|30d|all]`,
default `7d`) serves the series plus on-the-fly aggregates computed
from the item timestamps (`agentbacklog --list --all`):
mean/median lead time (created→done) and queue wait (created→started),
items done/day, peak workers. A missing/corrupt journal yields an
empty clean series; an unavailable agentbacklog still serves the
series (`items_available:false`, item aggregates `null`). Additive
request — no protocol breakage.

### 2.11 Plan→review→implementation pipeline — `plan_review` opt-in

`[backlog_orchestrator] plan_review = true` (default **false**, legacy
dispatch unchanged) switches the native orchestrator to a gated
pipeline, still on the same tick/persist/lock substrate (item #139,
`server/orch_plan.rs`):

- **Roles** — three session prefixes instead of one: `Backlog-P<N>`
  planners (`planner_mode`, default `plan`), `Backlog-R<N>` ephemeral
  reviewers (`reviewer_mode`, default `smart` — one review per session,
  deleted afterwards) and the usual `Backlog-W<N>` workers which now
  only take `no_plan=1` items and the patchnotes item (created with
  `--no-plan`), in `implement_mode` (default `bypass`). Role modes are
  **ACP session modes** (`session/set_mode`, applied post-handshake):
  ids that aren't agent CLI/env values (`plan`/`smart`/`ask` — devin's
  `DEVIN_PERMISSION_MODE` only knows normal/accept-edits/dangerous/
  autonomous) are omitted from the spawn env rather than killing the
  process at boot, and on respawn.
- **Flow** — `pending → wip → plan_review → reviewing → wip → done`
  (agentbacklog ≥ 0.7: `--claim`, `--submit-plan`, `--claim-review`,
  `--unclaim-review`, `--verdict`, `--reassign`). The item is claimed
  **by the orchestrator** after spawn (no more self-`--pop`) — a failed
  claim deletes the fresh session. The planner's submission is read
  from its internal journal — `PLAN_FILE: <path>` marker or a pending
  exit-plan `PermissionRequest` — never via fork/exec.
- **Verdicts** — a `wip` whose `review_round` advanced is a fresh
  verdict (journal marker is only a fallback when the reviewer forgot
  `--verdict`): relayed to the planner via PeerMessage if the reviewer
  didn't; `denied`/`conditional` → revision loop bounded by
  `max_review_rounds` (default 5 — beyond: item parked `plan_review`
  "stalled — human review required", planner session removed; human
  un-stall = `agentbacklog --reassign <id> --agent-id <other> --force`
  — the daemon drops the stall when the owner changes); `approved` →
  `/compact` +
  implementation prompt + `set_mode(implement_mode)` on the **same**
  planner session, or a fresh implementer + `--reassign` when the
  planner is gone (`denied` + gone planner → release).
- **Priorities & guards** — reviewers dispatch before planners;
  whenever a `plan_review` item waits unclaimed, planner/worker spawns
  require ≥2 free slots (a reviewer slot stays reserved). Reviewer
  timeout → one nudge, then `--unclaim-review` + 5 min respawn
  cooldown; dead reviewer → same unclaim. Dead planner with its item
  still `wip` → release (same orphan rule as legacy). A stale
  `PermissionRequest`/`QuestionRequest` (≠ exit-plan) is auto-answered
  after `nudge_cooldown_secs` so a blocked planner can't freeze the
  loop; 3 plan-phase nudges without submission → rm + release. The CLI
  capability is probed once per tick (`--claim-review` in `--help`) —
  a too-old agentbacklog parks the component in `waiting`, not in a
  spawn-fail loop.
- **Adoption** — pre-existing `wip` items and `Backlog-W*` sessions
  keep the old protocol untouched (they finish as before, no
  retro-planning); the release-PR guard counts `plan_review`/`reviewing`
  as in-flight work, and `status.workers` now covers all three roles
  (`planners`/`reviewers` counters added).

### 2.12 Worker pool per profile — `worker_pool` (#282)

`[backlog_orchestrator] worker_pool` replaces the flat
`worker_agent` + `max_workers` worker surface with a structured team —
TOML array-of-tables, one entry per profile:

```toml
[[backlog_orchestrator.worker_pool]]
profile = "devin"   # [agents.*] id — empty = worker_agent → agent
count   = 2         # slots of this profile (0 = inert entry)
model   = "default" # empty = global `model`
effort  = "high"    # thought_level post-spawn — empty = global
mode    = "agent"   # empty = global `mode`
```

- **Capacity** — the sum of `count` replaces `max_workers` for worker
  slots (`max_working` stays the independent active cap; `max_workers`
  is ignored for workers once the pool is set). Empty/absent pool =
  implicit single entry `worker_agent` + `max_workers` + global
  values — legacy configs unchanged.
- **Spawn order** — entries fill in declaration order: the first entry
  whose slots aren't all occupied by live `Backlog-W*` sessions of its
  profile wins (no round-robin); same-tick spawns count via the
  `extra` profile list. A worker whose profile vanished from the pool
  occupies no entry slot.
- **Set syntax** — `acctl comp set backlog-orchestrator worker_pool …`
  accepts the JSON array, its string form, or the shorthand
  `devin:2,claude=1` (`profile=count` also accepted); `[]`/`{}`/empty
  clears back to legacy. Values outside a profile's announced
  model/mode/effort vocabularies warn at spawn, never refuse.
- **Failover** — rate-limit drift compares `meta.agent` against ALL
  pool profiles (count > 0): a worker legitimately on entry 2 doesn't
  drift; a drifted worker is replaced on the FIRST pool entry with its
  model/effort. Planner/reviewer/PR-reviewer roles gained the same
  per-role `*_model`/`*_effort` fields (empty = global fallback).
- **UI** — the component form groups params into sections
  (`ParamSpec.group`): Worker team / Planning & plan reviews / PR
  reviews / General. `worker_pool` renders as an editable table —
  profile dropdown + slots + model/effort/mode inputs whose datalists
  come from that row's profile caps (`list_agents`), plus a live
  « N slots · max M active » total.

---

## 3. Risks & constraints

1. **The daemon orchestrates the agents** — restarting it kills the
   sessions. A managed `process` suffers the same fate (SIGTERM on
   stop, respawn at boot if autostart). That's **consistent** but must
   be documented: a service that must survive a daemon restart →
   `external`. Development/testing of daemon features happens on a
   throwaway `AGENTCONTROL_HOME` — never on the live daemon.
2. **Proto compat**: new `ReqKind`s = safe extension (old client
   ignores; old daemon → clean `parse:` with extracted id). The client
   gates on `daemon_status.components`. `(sync)` keyword on the
   proto+daemon PR (client↔daemon incompat = auto-update).
3. **Security**: the daemon binds loopback only — unix socket 0600 and
   both `bind_http`/`bind_tcp` enforced loopback at startup (a daemon
   refusing to start on a non-loopback address — `bind_tcp` alone has an
   explicit `allow_non_loopback_tcp` escape, warned at every boot with
   TCP conns still authenticated by `daemon.token`) — component RPC
   trust amounts to "whoever talks to the socket drives the agents"
   (`NewSession` can already spawn processes; `ComponentInstall` only
   runs **embedded declarative recipes**, never arbitrary shell sent by
   the client). Guards: refuse specs targeting daemon/supervisord/
   systemd; components.toml inherits root permissions (0600 advised,
   like daemon.sock).
4. **Persistence**: `components.toml` rewritten via `.part`+atomic
   rename (`write_file` pattern). `config.toml` is **not** rewritten by
   the daemon — two files, two responsibilities.
5. **Windows**: the daemon is Linux-only (Unix socket) — the model
   doesn't forbid it but Windows `process` management is out of scope.
6. **Load**: one more supervisor in the daemon event-loop — bounded:
   ~30 s healthchecks, rare spawns. No new Rust dep required (`toml`,
   `ureq` already present).
7. **Orphans/split-brain**: an external component keeping state
   (orchestrator: its ledger + flock) must stay **singleton** — a
   shared flock serializes duplicate supervisors; when migrating from
   another supervisor, remove its conf so two supervisors can't
   relaunch the program (flock = second instance exits cleanly, safe).
