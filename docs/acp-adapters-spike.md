# Spike #278 — In-house ACP adapters: common base + per-agent feasibility/effort

> Exploratory analysis only — no implementation. Date: 2026-11-18.
> Cross-references: #277 (npm provisioning), #268 (claude model catalogue),
> #261 (auto-discovery registry).

## 1. What the daemon actually consumes (ACP client surface)

The daemon is an ACP **client** speaking JSON-RPC 2.0 over NDJSON on
stdio (`crates/daemon/src/acp.rs` — hand-rolled, no ACP crate). An in-house
adapter only needs to satisfy this surface, not the full Zed-facing one.

**Client → agent requests** (`session/agent.rs`, `session/config.rs`,
`session/prompt.rs`):

| Method | Required? | Notes |
| --- | --- | --- |
| `initialize` | yes | `protocolVersion: 1`, client caps: fs read/write **false**, terminal **false**, elicitation `{}`, `_meta.clientName` |
| `session/new` | yes | `{cwd, mcpServers, capabilities.compaction}` + optional `_meta` |
| `session/load` | yes (capability-gated) | `agentCapabilities.loadSession` decides resume vs fresh; failure degrades to `session/new` on the same process |
| `session/prompt` | yes | text content blocks; response `stopReason` ends the turn |
| `session/cancel` | yes | notification |
| `session/set_mode`, `session/set_model` | fallback path | used when no `configOptions` entry resolves |
| `session/set_config_option` | preferred | generic config mechanism (mode/model/effort…) |
| `session/list` | optional | native-session picker (resume elsewhere) |
| `session/rename` | optional, best-effort | `-32601` tolerated → marked unsupported |

**Agent → client**: `session/update` notifications (mapped in
`acp::map_update`) and requests `session/request_permission` +
`elicitation/*`. `fs/*` and `terminal/*` are **refused** with
`-32601` — a home adapter must not depend on client-side fs/terminal.

**`session/update` kinds actually rendered**: `agent_message_chunk`,
`agent_thought_chunk`, `user_message_chunk`, `tool_call`,
`tool_call_update`, `plan`, `available_commands_update`,
`config_option_update`, `current_mode_update`, `usage_update`,
`compaction_update`, `notice`. Everything else is ignored.

**Handshake response fields consumed**: `sessionId`, `modes`
(`currentModeId`, `availableModes[].id`), `models`
(`currentModelId`, `availableModels[].modelId|id`), `configOptions[]`,
`agentCapabilities.loadSession`. This is the entire contract.

## 2. Upstream `agentclientprotocol/claude-agent-acp` — anatomy

Repo: <https://github.com/agentclientprotocol/claude-agent-acp>
(Apache-2.0, npm `@agentclientprotocol/claude-agent-acp`, currently
v0.108.0). Fork lineage: Zed's `@zed-industries/claude-code-acp`.

### Size & shape

- `src/` non-test: **~54 500 LOC TypeScript**; `src/tests/`: ~100 000 LOC.
- The bulk is **v1** (`src/v1/acp-agent.ts` alone = **11 322 lines**) plus a
  newer **v2** agent (`src/v2/`, ~23 kLOC across 27 modules) — the adapter
  serves ACP v1 *and* v2 protocol versions.
- Shared modules worth noting: `permissions/` (modes, options,
  presentation, durable effects), `tool-calls/` (renderers, reporters,
  field trackers — this is where most fidelity work lives),
  `session-index/` (transcript scan of `~/.claude/projects/*/*.jsonl`,
  session list/rename/archive), `elicitation.ts`, `slash-commands.ts`,
  `mcp-oauth.ts`, `subagent-*`, `plans.ts`, `context-compaction*`,
  AIR/JetBrains extensions.

### Dependencies

`@agentclientprotocol/sdk` (ACP server side, TS) +
`@anthropic-ai/claude-agent-sdk` + `diff` + `zod`. **It does not talk to
the Anthropic API**: the SDK's `query()` spawns the `claude` binary in
SDK/headless mode and translates its stream. The adapter resolves the CLI
via `CLAUDE_CODE_EXECUTABLE` → bundled binary → the SDK's platform
package (`claude-cli.ts`).

### ACP surface implemented upstream

Far wider than ours: `session/new|load|resume|fork|list|close|delete|
rename|set_title|set_config_option`, `session/cancel`, v2 extras
(`session/replay`, `session/steering`, `session/async_task`, AIR
goal/extension `_meta` keys), `fs/*` and `terminal/*` **server-side**
capabilities (interactive terminals, file preview), MCP OAuth
auth-methods, subagent transcripts, permission *presentation* extensions,
plan mode, ultracode/effort config options, usage/cost reporting.

### Churn

~1 000+ commits since 2025-08 (~15 months), currently ~60–90
commits/month, pushed daily. The upstream tracks **every Claude Code
release**: new permission modes, new tool-call kinds, new SDK message
types, new slash commands. A large share of the diff is protocol/SDK
churn and rendering fidelity, not ACP surface.

## 3. Claude transport: stream-json (verified live)

`claude` 2.1.325 supports the SDK/headless transport directly:

```
claude -p --input-format stream-json --output-format stream-json --verbose
```

Verified on this machine (2026-11-18): bidirectional NDJSON works —
wrote a `{"type":"user","message":{...}}` line on stdin, received
`system/init` → `assistant` (text + tool_use blocks) → `result`
(`stop_reason`, `usage`, `modelUsage`, `total_cost_usd`).

Relevant flags (all present in `--help` of 2.1.325):

- `--resume <session-id>` / `--session-id <uuid>` / `--fork-session`
  → session/load mapping.
- `--permission-mode` = `acceptEdits|auto|bypassPermissions|manual|
  dontAsk|plan` → ACP modes.
- `--permission-prompts host|none` + `--permission-prompt-tool` →
  permission callbacks arrive as `control_request` /
  `control_response` messages (subtype `can_use_tool`) instead of
  blocking on a TTY.
- `--model`, `--settings`, `--mcp-config`, `--add-dir`,
  `--replay-user-messages`, `--prompt-suggestions`.

Observed message types: `system` (`init`, `hook_started`,
`hook_response`, …), `user`, `assistant` (content blocks:
text/thinking/tool_use/tool_result), `result` (terminal, with
`subtype: success|error_*`), `rate_limit_event`, `control_request` /
`control_response`, `prompt_suggestion`. The transcript persists to
`~/.claude/projects/<cwd-slug>/<session-uuid>.jsonl` (+ `subagents/`).

**Stability**: the format is *semi-documented* — it is the
`claude-agent-sdk` wire protocol, de-facto stable (the SDK and CLI ship
in lockstep from Anthropic), but not a published spec. Message fields
evolve between CLI releases; the upstream's churn rate is the best
available proxy for how often. Mitigation: the adapter stays thin —
map observed fields, ignore unknown ones, degrade gracefully.

**Auth**: untouched — the adapter spawns `claude`, which reads
`~/.claude` credentials itself. No token handling in our code.

**session/load replay**: upstream reads the transcript (`getSessionMessages`
→ the same `.jsonl` files) and **synthesizes ACP `session/update`
replays** for history. `--resume` alone restores context but emits no
history; our daemon expects replayed updates (it dedupes them, uses
them for meta effects). A Rust adapter can parse the JSONL transcript
directly — the format is line-delimited JSON, conceptually reusable
though not a published schema either.

## 4. Verdict Q3 — does `claude` have native ACP?

**No.** As of claude 2.1.325 (2026-11-18): no `--acp` flag, no `claude
acp` subcommand. Upstream request
[anthropics/claude-code#6686](https://github.com/anthropics/claude-code/issues/6686)
(open since 2025-05, still a feature request) and
[#48506](https://github.com/anthropics/claude-code/issues/48506) remain
unimplemented. The community answer is still "use the adapter".

**Codex**: also no native ACP (`codex --help` has none), **but** it has
`codex app-server` — an experimental JSON-RPC 2.0 stdio protocol with
`generate-ts` / `generate-json-schema` subcommands (i.e. a *real*,
schema-published protocol, much stabler than claude's stream-json).
That is what `@agentclientprotocol/codex-acp` (now maintained by the
agentclientprotocol org; the zed-industries repo is archived) translates.

## 5. Options compared

### a) Rust, in-repo (`crates/acp-*`)

A thin ACP **server** binary shipped with the daemon tarball, spawned by
the daemon like any other argv. Zero npm/node dependency; the profile's
`commands` becomes e.g. `["acp-claude"]` (resolved next to the daemon
binary or on PATH).

- Rust ACP server side exists: `agent-client-protocol` crate v5.x
  (traits `Agent`/`Client`, router, conductor — docs.rs). Alternatively
  ~150 lines of hand-rolled NDJSON JSON-RPC like `acp.rs` already does
  client-side; the wire format is trivial.
- The heavy lifting is the **backend translator**: spawn `claude` in
  stream-json mode, map `assistant`/`tool_use`/`control_request`/
  `result`/`init` onto ACP `session/update` + `request_permission`.
- `command_available("claude")` remains the only external requirement →
  resolves #277 for this profile outright.
- Cost: real Rust work + ongoing maintenance burden (see §8).

### b) Minimal TS bundle in the tarball

Port/bundle the upstream adapter. Keeps node as a dependency (or a
bun-compiled binary — upstream already ships standalone executables via
`bun build --compile`). Closer to upstream = easier to track its fixes,
but we inherit ~54 kLOC we don't need and still depend on the Claude
Agent SDK release cadence.

### c) Status quo: external npm + #277 provisioning

`npm i -g @agentclientprotocol/claude-agent-acp` provisioned by the
daemon (#277). Zero code, mature adapter, but: node/npm dependency on
user machines, install friction, version drift, and the profile stays
`available:false` until provisioning succeeds.

## 6. Generalization — the matrix

Only **claude** and **codex** lack native ACP; every other registry
entry already speaks ACP itself.

| Agent | Native transport | Adapter needed? | Backend protocol stability |
| --- | --- | --- | --- |
| devin | `devin acp` | no | — |
| cursor | `cursor-agent acp` (hidden subcommand) | no | — |
| gemini | `gemini --experimental-acp` | no | — |
| opencode | `opencode acp` | no | — |
| **claude** | `claude -p --input/-output-format stream-json` | **yes** | semi-documented SDK wire protocol; churn ~monthly with CLI releases |
| **codex** | `codex app-server` (stdio JSON-RPC, `--listen stdio://`) | **yes** | experimental but schema-generated (`generate-json-schema`, `generate-ts`); versioned with codex releases |
| aider | HTTP API / scripting, no agent loop protocol | yes (large) | — |
| goose | `goose acp` (stdio) | no — native ACP | — |
| amp | CLI only, no ACP | yes | no stable protocol |
| qwen-code | fork of gemini-cli → `--experimental-acp` likely works | probably no | inherits gemini's |

**Common skeleton**: an ACP-server core in Rust (`agent-client-protocol`
crate or a thin NDJSON JSON-RPC layer — the daemon already proves the
format) + one backend translator per CLI. Estimated split:

- Skeleton (initialize/session lifecycle/prompt/cancel/update routing/
  permission round-trip): ~60–70% of the work, written **once**.
- Claude backend (stream-json → ACP): the first and hardest —
  permissions via `control_request`, tool-call rendering, modes/models,
  `--resume` + transcript replay.
- Codex backend (`app-server` JSON-RPC → ACP): after the skeleton, mostly
  event mapping (`turn/*`, `item/*`, approvals). Arguably *easier* than
  claude because app-server is a real bidirectional protocol with a
  published schema rather than a print-mode event stream. Estimate
  ~50–60% reuse of the skeleton; backend-specific work ≈ half of
  claude's.

**Where adapters live** — recommendation: **separate binaries in the
release tarball** (`acp-claude`, `acp-codex` or one `acp-shim <backend>`),
spawned by the daemon as ordinary argv. Rationale:

- Deployment = "the daemon ships them"; `command_available` unchanged.
- Crash isolation: an adapter panic kills one session's agent process,
  not the daemon.
- Reusable by any ACP client (the binaries are standalone ACP servers).
- In-process virtual profiles would be less code-motion but couple agent
  lifecycles to daemon restarts and complicate the spawn/kill model.

**Registry impact** (`agents_auto.rs`): add an `internal`/`builtin`
marker to `AdapterSpec` — e.g. `commands: &[["acp-claude"]]` resolved
against the daemon's own directory first, `detect` stays on the `claude`
CLI, `install_hint` becomes "install Claude Code". `via` could stay
`auto` (it *is* auto-discovered) or gain `via:"builtin"` for UI
labelling. The existing `commands` fallback mechanism already supports
`[internal-acp, npm-adapter]` ordering — ship internal first, keep npm
as fallback during transition.

## 7. Effort estimates

Assumes one experienced Rust dev, ACP surface limited to §1.

| Scope | Estimate |
| --- | --- |
| **MVP claude** — initialize, session/new, session/prompt with streamed `agent_message_chunk`/`agent_thought_chunk`/basic `tool_call`, `session/cancel`, `session/request_permission` round-trip via control channel, modes (`--permission-mode`), `--model` | **~8–12 days** |
| + `session/load` (`--resume` + JSONL transcript replay → updates) | +3–5 days |
| + `session/list`, `session/set_config_option` (effort/model/mode configOptions), `usage_update`, `available_commands_update` (slash commands from init), `plan`, `compaction_update`, `session/rename` | +1–2 weeks |
| **Parity utile** — thinking blocks, rich tool-call kinds/diffs, subagent events, elicitation (`AskUserQuestion`), fast-mode/effort options, titles, edge cases (rate-limit, auth errors surfaced as ACP errors) | **+3–5 weeks** total vs MVP |
| Codex backend on the shared skeleton | **~1–2 weeks** MVP, +2–3 weeks parity |

For comparison: upstream is ~54 kLOC + ~100 kLOC tests for the *full*
surface (v1+v2, AIR extensions, terminals, fs, subagents, MCP OAuth).
Our §1 surface is roughly a third of that, and we skip v2/AIR entirely.

## 8. Risks

- **Protocol churn**: stream-json is semi-documented; Anthropic ships
  Claude Code ~weekly and the upstream absorbs the drift at ~60–90
  commits/month. A home adapter must either track releases (real
  maintenance cost) or pin a minimum CLI version and degrade. This is
  the dominant long-term cost — bigger than the initial build.
- **Built-for-nothing risk**: if Anthropic ships native ACP
  (#6686/#48506), the claude backend dies — but the skeleton survives
  for codex, and the npm adapter remains as fallback. Bounded loss.
- **Transcript replay**: `.jsonl` format is internal; upstream already
  depends on it via the SDK, so the risk is shared, not new.
- **Permission fidelity**: `can_use_tool` control messages carry rich
  options (allow/deny rules, destinations); a thin mapping loses some
  nuance vs upstream's `permissions/` module (presentation, durable
  effects). Acceptable for our UI, which renders a fixed option set.
- **Model catalogue (#268)**: the 3-model announcement comes from the
  SDK/account layer, not ACP. A home adapter reading `init`/settings
  could announce the full list — upside, not a blocker.

## 9. Recommendation

**Option (a), staged** — and only if #277 alone proves insufficient:

1. Keep c) as the default path now (zero risk, mature adapter).
2. If the npm dependency/friction justifies it, build the Rust skeleton
   + **claude backend MVP** (~2 weeks) as `acp-claude` shipped in the
   tarball, registered in `agents_auto` with npm adapters as fallback
   commands. This resolves #277 for claude and gives #268 a path to a
   complete model list.
3. Codex backend follows on the same skeleton (~1–2 weeks) if wanted —
   its app-server protocol is the *most* stable backend available.
4. Re-evaluate if Anthropic ships native ACP: detection becomes
   `command = ["claude", "acp"]`-style and the home adapter is dropped
   per-profile (skeleton may still serve codex).

Do **not** port upstream code (license allows it, but the user asked
for a reimplementation and 54 kLOC is unmaintainable here anyway). Do
**not** embed in the daemon process — separate binary, spawn-as-argv.
