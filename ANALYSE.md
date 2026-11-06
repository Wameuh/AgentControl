# AgentControl — Analysis & design questions

> Working document: requirements analysis, architecture options,
> technical mechanisms and open questions with recommendations.
> Date: 2026-10-18 — **V1 implemented** (see §10 Implementation status)

---

## 1. Requirements restated

- **Client application** (Ubuntu + Windows) that drives **AI agents** running
  on a **remote Ubuntu server over SSH**.
- Agents run **detached**: closing the app doesn't kill them, they keep
  going on the server.
- The client can **reconnect** to a live session, review **the full
  history** (paged scrollback, not all in RAM), and **expand/collapse**
  blocks (CLI commands, outputs, diffs…).
- Hard constraint: **light on RAM**, smooth.

Roughly: "a light heavyweight client" + "a tmux-style session multiplexer,
but for agents" + "a replayable event journal".

---

## 2. Key observation: what already exists

Before picking a tech, know that modern agent CLIs already expose
machine-consumable interfaces:

| Mechanism | What it does | Value for us |
| --- | --- | --- |
| **ACP** (`devin acp`, Claude Code, Gemini CLI…) | The agent speaks **JSON-RPC over stdio**: sessions, message streaming, structured `tool_call`s, permission requests | **The** protocol designed to plug a GUI onto an agent (what Zed/JetBrains do). "Expandable" blocks (tool calls, terminal output) are native to the protocol. |
| `devin -p` / `--print` | One-shot non-interactive mode | Enables a very simple MVP: one process per message. |
| `devin -c` / `-r <id>`, `devin list --format json` | Resume and list sessions persisted **on disk server-side** | The "real" history already lives server-side; the client doesn't need to be the source of truth. |
| `--export` (**ATIF** format) | Exports the conversation as JSON after each turn | Replay/display history without scraping the terminal. |
| Permission modes (`accept-edits`, `smart`, `bypass`, `autonomous`+sandbox) | Control what the agent can do without asking | Crucial for a **detached** agent: nobody is there to approve → mode choice must be handled explicitly. |
| `devin --cloud` | Already does "remote agent that survives shutdown" | …but on Cognition's cloud, not on **your** server. It's the functional benchmark, not the solution. |

**Consequence**: no need to write a terminal emulator or parse raw output
if we build on ACP or structured exports. The real difficulty shifts to:
**keeping the agent process alive when the client disconnects, and being
able to re-attach cleanly.**

---

## 3. The central problem: "detach"

ACP speaks over stdio → it's bound **to a process and a connection**. If
the client leaves, the process dies (SIGHUP / socket closed). Three
strategies:

### Option A — Session daemon/server ✅ **chosen**

A small daemon runs permanently on the server (`agentcontrol-daemon`).
It:

- spawns agents (`devin acp`, `claude`, shell…) as **child processes**,
- holds the ACP/stdio connection to them,
- journals every event to a **per-session append-only log**
  (`~/.agentcontrol/sessions/<id>/events.jsonl` + blobs for large outputs),
- exposes an API to the client: `list_sessions`, `get_events(id, from_seq, limit)`,
  `subscribe(id, last_seen_seq)` → live stream, `send_message`, `interrupt`,
  `approve_permission`…

The client is just a **view**: it can leave, come back, ask for the last
N events then subscribe to live updates with a sequence number → **zero
gap in history**, clean reconnection.

This is the "tmux/dtach for agents" pattern, or "multiplexed ACP server".

- Client↔daemon transport:
  - **via SSH tunnel** (`ssh -L`, or direct channel in the client's SSH lib):
    zero open ports, auth = existing SSH. *Recommended for personal use.*
  - Direct WebSocket+TLS: only needed for multi-machine clients without SSH
    or a web app.

### Option B — tmux/dtach + scraping

Run the agent in `tmux new-session -d`, capture output via
`tmux pipe-pane` to a file or `capture-pane`.

- ✅ Works with **any CLI**, even without a machine mode.
- ❌ History = raw terminal text: expand/collapse, tool call statuses,
  diffs → requires parsing ANSI, fragile. No clean interactive answers
  to permission prompts.
- Fine as a **fallback** for non-ACP agents, not as the main architecture.

### Option C — One-shot process per message (ultra-simple MVP)

`devin -p "msg" --export events.json` and `devin -r <id> -p "next msg"`.

- ✅ Survives anything (state on disk), zero daemon.
- ❌ No real-time streaming (must poll the export file), process startup
  latency on every message, no clean interruption.
- Can serve as a **throwaway v0** to validate the UI, but not the target.

**Decision: Option A**, keeping B as a future "generic agent" mode.

---

## 4. Client technology

| Choice | RAM / weight | For | Against |
| --- | --- | --- | --- |
| **Tauri** (Rust + system webview) | ~30–80 MB | Cross-platform Ubuntu/Windows, modern web UI (virtual list, markdown, diff), Rust backend for SSH+protocol | System webview = rendering slightly varies by OS |
| Electron | 150–300+ MB | Huge ecosystem | **Contradicts the RAM requirements** |
| Qt (C++/PySide) | ~50–100 MB | Native, solid | Heavier dev, PySide license |
| **TUI** (Ratatui/Textual/Bubbletea) | ~10–30 MB | Lightest, runs even over SSH | No real GUI, UX learning curves (expand, scrollback, mouse) |
| **Web app served by the daemon** | ~0 install-side | Client = browser, reachable from anywhere | Needs proper TLS/auth if exposed; less "desktop" |

**Decision: Tauri.** Rich UI (virtualized list, markdown, collapsible
blocks, diffs), single binary, lightweight, and the SSH/protocol part
lives in the Rust backend (`russh`/`ssh2`, `tokio` crates).
TUI option possible as a v2 "lite".
The web option gets interesting if you want access from a phone.

---

## 5. Data model: event sourcing

It's **the** mechanism that makes everything else simple (scrollback,
expand, reconnection, replay).

Each session = an append-only journal server-side:

```jsonl
{"seq": 1, "ts": "…", "type": "user_message", "text": "…"}
{"seq": 2, "ts": "…", "type": "agent_text", "text": "…", "streaming": true}
{"seq": 3, "ts": "…", "type": "tool_call", "id": "tc1", "kind": "exec",
  "title": "cargo test", "status": "running"}
{"seq": 4, "ts": "…", "type": "tool_output", "call_id": "tc1",
  "blob": "blobs/tc1.out", "size": 184230, "exit_code": 0}
{"seq": 5, "ts": "…", "type": "permission_request", "call_id": "tc2", …}
```

- **Large outputs** → separate blob files (`blobs/<id>`), the event keeps
  only a summary + pointer → *expand = fetch the blob on demand*.
- Monotonic `seq` → the client says "give me everything after seq 4821".
- In ACP, this mapping is nearly direct: `session/update` notifications →
  events.

The client maintains a **window**: the last ~200 events in RAM; scrolling
up triggers `get_events(before_seq)` in pages. Never the whole history
in memory → RAM requirement held even on 100,000-line sessions.

---

## 6. Questions to settle

### Q1 — Which agents must we support?

- **a) Just one** (e.g. Devin CLI via `devin acp`) → simpler, optimized UX.
- **b) Several ACP agents** (Devin, Claude Code, Gemini, Codex…) → same
  protocol, low marginal effort, high value.
- **c) Generic shell** (just a detached terminal) → useful bonus via tmux.
- **Reco: b)** — ACP unifies; plan the adapter from the start but
  implement only one (Devin) in the MVP.
- ✅ **Decision: b) multi-agent ACP.** Consequence: we need an "agent
  profile" abstraction layer for heterogeneous modes/commands — see §6.1.

### Q2 — Where does the daemon live and how does the client connect?

- **a) SSH tunnel only** — the client carries an SSH connection (key from
  `~/.ssh` or agent), opens a channel to the daemon socket. Zero network
  config, security = SSH. **Reco for v1.**
- b) TCP port + TLS + token — required for web/multi-device, add later.
- c) The client *deploys and starts* the daemon over SSH when absent
  (auto-bootstrap) → **highly recommended**: `scp` the binary +
  `systemd --user` or `nohup`.
- ✅ **Decision: a** (+ c auto-bootstrap). b stays possible in v2 — the
  daemon↔client protocol must therefore be transport-agnostic (same
  message format over SSH channel or WebSocket).

### Q3 — How to handle permissions of a detached agent?

Nobody in front of the screen when the agent asks "may I run `rm`?":

- **a)** The client relays the request → notification + the user answers
  when back (the agent waits, that's fine, it doesn't die).
- **b)** `accept-edits`/`smart`/`bypass` mode set at launch based on the
  trust level chosen **per session**.
- **c)** Auto-approval rules daemon-side (command allowlist).
- **Reco: a + b** — mode chosen at session creation; remaining requests
  are pushed to the client and **persist** in the journal (visible and
  answerable after reconnection).
- ✅ **Decision: a + b, with mode change mid-session.** Natively
  supported: ACP exposes `session/set_mode`, and CLIs have their own
  slash commands (`/mode`, `/plan`, `/bypass`…). The daemon must expose
  `set_mode(session, mode)` and the client offer a **per-session** mode
  selector fed by the modes *declared by the agent* (not a hardcoded
  list). See §6.1.

### Q4 — Interface format: GUI, TUI or web?

See §4. ✅ **Decision: Tauri GUI.**

### Q5 — One server or several?

- a) One configured server — v1.
- b) Multi-server in a sidebar — design the data model (`server_id`
  everywhere) now, implement later. **Reco: b in design, a in code.**
- ✅ **Decision: a in v1, b not locked out** — `server_id` in all
  schemas/config from the start.

### Q6 — Notifications when the app is closed?

An agent finishing at 3am…

- a) Nothing — state visible on reopen (badge "done/waiting").
- b) Local notification if app open in background.
- c) Webhook (ntfy, mail, Matrix…) triggered by the daemon.
- **Reco: a then c** (ntfy.sh = 20 lines daemon-side).
- ✅ **Decision: a then c** — status badges v1, ntfy webhook in v1.x.

### Q7 — Should we show diffs / modified files?

- a) No, just chat + tool calls.
- b) Diff view per edit tool call (ACP provides `diff` in updates).
- c) Remote workspace file explorer + git status.
- **Reco: b** native via ACP; c in v2.
- ✅ **Decision: b** — diff per edit tool call.

### Q8 — Security: what can the agent touch on the server?

- a) Dedicated `agent` user on the server (not your main user).
- b) Confined workspaces: one root dir per session, `bwrap`/sandbox.
- c) Container/VM per session (heavy).
- **Reco: a + b** — Devin CLI already has a `--sandbox --permission-mode
  autonomous` mode (bubblewrap) usable on Linux.
- ✅ **Decision: a + b, with bypass option** — dedicated `agent` user +
  confined workspaces; `bypass`/`dangerous` stays selectable per session
  for those who accept the risk.

### Q9 — History: source of truth?

- a) The daemon's journal (events.jsonl) — full control over the format.
- b) The agent's native store (`devin -r`) — zero duplication but
  unguaranteed format + depends on the CLI.
- **Reco: a as display truth, b as resume truth**: the journal feeds the
  UI, `devin -r <id>` makes the agent continue.
- ✅ **Decision: a** — the daemon journal is the display source of truth.
  The agent's native session IDs are stored in metadata to enable
  `session/load` / `-r` on resume.

### Q10 — What history granularity?

- Keep **raw** (every chunk) or **compacted** (one message = one event)?
- **Reco: compacted** in the journal + raw ACP archived for debug — the
  journal stays small, scrollable, and the raw serves diagnostics.
- ✅ **Decision: configurable** — `journal.detail = compact | raw` in
  daemon config, default `compact`. `raw` mode keeps every streaming
  chunk (useful for debug, ~10× larger).

### Q11 — Multi-user?

- Single user single machine: v1. Multi-user changes everything (auth,
  socket isolation, quotas) → **out of scope v1**, but don't lock it out.
- ✅ **Decision: out of scope, not locked out** — the daemon runs under a
  Unix user; the Unix socket naturally gets the user's permissions →
  multi-user isolation would come "for free" from one instance per user.

### Q12 — Recovery after crash / server reboot?

- a) systemd `--user` with `Restart=always` for the daemon; agents
  restart via `-r` (session resume) or are marked "interrupted".
- b) Nothing special, sessions marked orphaned, manual resume.
- **Reco: a for the daemon, b for agents** (resuming an agent mid-action
  is risky anyway).
- ✅ **Decision: a for the daemon** — `systemd --user` `Restart=always`
  unit installed by auto-bootstrap. Agents: marked "interrupted" on
  restart, manual resume via `session/load` / `-r`.

### 6.1 Agent adapter: handling mode/command heterogeneity

Every ACP agent has its own modes, slash commands and conventions (Devin:
`/mode`, `/plan`, `/ask`, `/model`, Shift+Tab; Claude Code: other names,
other cycle…). This can't be hardcoded → the daemon maintains an
**agent descriptor** filled at connection time:

```rust
struct AgentProfile {
    id: String,                 // "devin", "claude-code", …
    display_name: String,
    spawn_cmd: Vec<String>,     // ["devin", "acp", "--model", "{model}"]
    resume: ResumeStrategy,     // SessionLoad | SpawnWithFlag("--resume", id)
    capabilities: Capabilities, // discovered via ACP `initialize`
}
```

The essentials are **dynamically discovered** via the protocol, not
configured:

| UI need | ACP mechanism | Fallback |
| --- | --- | --- |
| Per-session mode selector | `session/set_mode` + `sessionInfo.modes` returned by the agent | Send the equivalent slash command (`/mode bypass`) as a message |
| Available commands list | `available_commands` in `session/update` notifications (Devin announces them: `/plan`, `/ask`, `/compact`…) | Static profile file per agent |
| Model change | `session/set_model` if supported | Slash command `/model opus` |
| Interruption | `session/cancel` | SIGINT to the process |
| Session resume | `session/load` (confirmed in `devin acp`) | Respawn with `-r <id>` |

→ The UI client is **agnostic**: it displays the modes/commands the
daemon feeds it. Adding a new agent = adding an `AgentProfile` (10 lines
of TOML/JSON), zero UI code.

### 6.2 When the agent asks a question mid-run

Two distinct cases, handled differently:

**a) Permission request** ("may I run `npm install`?")
→ ACP `session/request_permission` with options (`allow_once`,
`allow_always`, `reject`…). The daemon:
1. journals a `permission_request` event (persistent),
2. pushes it to the client if connected (notification + inline in chat),
3. **keeps it "pending" otherwise** → answerable after reconnection, the
   agent waits server-side without dying,
4. applies the daemon's auto-approval rules if configured (allowlist).

**b) Open question from the agent** (`ask_user_question`, "which branch
to target?") → Devin CLI uses the **standard ACP elicitation** (confirmed
in the changelog). The client must announce the `elicitation` capability
at `initialize`, and the daemon relays likewise: `question_request`
event with a response schema (multiple choice or free text), persistent
until answered.

Watch points:
- **Configurable timeout**: `question.timeout = never | 1h | …`; option
  "timeout → default answer" or "timeout → switch to bypass mode".
- A pending question **blocks the current turn**: the UI must make it
  very visible (red badge on the session, ntfy webhook notification in
  v1.x).
- Answer from another device / reconnect: the `question_answered` event
  is journaled with the answer → all clients see the resolution.

### 6.3 Storage: where and how heavy server-side

Everything lives under `~/.agentcontrol/` (dedicated `agent` user):

```
~/.agentcontrol/
├── daemon.sock                  # Unix socket (access via SSH tunnel)
├── config.toml
└── sessions/<session_id>/
    ├── meta.json                # title, agent, workspace, native_session_id
    ├── events.jsonl             # journal (display truth)
    └── blobs/<event_id>         # large outputs, one file per tool call
```

> Implemented: no SQLite after all — `meta.json` + an **in-memory**
> seq→offset index (~16 B/evt, rebuilt at startup) are enough: history
> reads are direct seeks even on multi-MB journals. Retention is applied
> by a janitor (see `[retention]` in config): journal compaction past a
> threshold, LRU blob purge, optional purge of idle sessions.

Orders of magnitude (intense dev session, ~8h):

| Data | Typical size | Notes |
| --- | --- | --- |
| events.jsonl | 1–5 MB, capped by `journal_max_mb` | messages + tool call metadata |
| blobs/ | capped by `blob_max_mb` (LRU) | dominated by build/test outputs |

**Lightness levers (implemented)**:
- Externalization: output > 32 KB → blob, the event keeps a 4 KB excerpt.
- Janitor: `events.jsonl` compaction > 64 MB → keeps 32 MB of tail;
  blobs > 512 MB/session → LRU purge; idle sessions → `purge_days` (off
  by default).
- In-memory seq→offset index: O(1) history, only ~16 B/evt.
- No copy of the agent's native history: we only store the ID.

→ Realistic budget: **< 600 MB per session** worst case (bounded by
quotas), typically a few dozen MB per month.

---

## 7. MVP → v1 breakdown

| Step | Contents | Validates |
| --- | --- | --- |
| **MVP0** | Minimal Rust daemon: spawn `devin acp`, JSONL journal, Unix socket. Client = CLI script `send`/`tail`. | detach + journal + reconnection |
| **MVP1** | Tauri client: session list, live chat, paged scrollback, collapsible tool call blocks | core UX |
| **v1** | Multi-agent ACP, relayed permissions, daemon SSH auto-bootstrap, ntfy notifications | the advertised product |
| **v2** | Diff/file view, multi-server, TUI mode, optional web | extensions |

---

## 8. Risks & watch points

1. ~~**ACP lifecycle**: `session/load`~~ — ✅ resolved: `devin acp`
   supports `session/load` and restores titles/modes/usage on resume
   (CLI changelog). The MVP0 spike stays useful to validate latency and
   event flow.
2. **Backpressure**: an agent can produce MBs of output (build logs) →
   truncate blobs, sample streaming, never push the whole stream to the
   client.
3. **SSH from Windows**: use a lib (`russh`) rather than `ssh.exe` —
   keys, Pageant/ssh-agent, keepalive, auto-reconnect.
4. **One writer per session**: if two clients subscribe and send,
   arbitrate daemon-side (or accept it — you're alone).
5. **Journal size**: rotation/compaction of events.jsonl, purge blobs
   > X days.

---

## 9. Executive summary

> **Rust daemon `agentcontrol-daemon` on the server** (spawns and owns
> ACP agents, journals everything in JSONL+blobs under
> `~/.agentcontrol/`) **+ Tauri client** connecting via **SSH tunnel**,
> displaying a paged window of the journal and subscribing to the live
> stream by sequence number. Multi-agent via **declarative profiles** +
> ACP discovery (modes, commands, elicitation). Permissions/questions
> **persistent**: answerable after reconnection, the agent waits
> server-side. It's the only design that simultaneously satisfies: real
> detach, lossless reconnection, full history without excessive RAM,
> native expandable blocks, and bounded server storage (< 200 MB/month,
> purgeable).

---

## 10. Implementation status (2026-10-19)

Delivered and **tested end-to-end** (no unit tests, real verification):

| Component | Status |
| --- | --- |
| `crates/daemon` | ✅ Unix socket + TCP loopback, ACP spawn, indexed JSONL journal (seq→offset) + blobs, subscribe/catch-up, persistent permissions/elicitation, `session/load`, replay dedupe, retention janitor (compaction/purge), systemd |
| `crates/client-core` | ✅ unix/tcp/`ssh -L` transports, calls + dedicated subscribe, 60 s timeout |
| `crates/mock-agent` | ✅ scenarios: chunks, tool call, perm, elicitation, blob > 32 KB, cancel |
| `crates/acctl` | ✅ full CLI (used for all e2e tests) |
| `client/` Tauri | ✅ complete code (backend commands + frontend TS, vite build OK **11 KB JS**) — not launched here (webkit2gtk absent) |
| `packaging/` | ✅ systemd `--user` unit + `deploy.sh` (scp + enable + linger) |

Verified in e2e: **real Devin agent** (`devin acp`) — handshake, modes,
slash commands surfaced, thoughts/answers streamed, `session/load` after
restart; **mock agent** — permissions, elicitation, blobs, cancel;
daemon ≈ **6.5 MB RSS**; 20,000-event journal read in **7 ms** mid-file;
compaction 3.2 MB → 1 MB; LRU blob purge; idle session purge
(`purge_days`).

Left for v1.x+: Tauri webview to test on dev machine, ntfy
notifications, daemon auto-bootstrap from client, auto-approval
allowlist, richer diff view, multi-server.
