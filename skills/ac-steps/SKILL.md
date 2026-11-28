---
name: ac-steps
description: Publish the current task's step list to the AgentControl daemon (acctl steps set) — shown in the client's "Steps" panel and persisted per session
argument-hint: ""
allowed-tools:
  - exec
---

# ac-steps — task progress visible in the client

When you work on a task in an AgentControl session, publish a minimal
step list (name + status) to the daemon. The client shows a "Steps"
panel per session that updates live, and the list survives daemon
restarts (steps.json) and resume.

Deliberately minimal granularity: **just the step name and its
status** — no sub-tasks, no descriptions, no nesting.

## Statuses

`todo` | `in_progress` | `done` | `abandoned`

- **at most one `in_progress` step** at a time — when you move on, mark
  the previous one `done` in the same `set`.
- Mark `abandoned` rather than **deleting** an obsolete step — the
  history of what was attempted stays visible.
- Short names (a label, not a sentence — ~200 chars max, truncated
  beyond).

## Protocol

Your session is `$AGENTCONTROL_SESSION` (injected by the daemon — same
variable as `acctl send`). You don't need to pass it:

```bash
# At task start: publish the full list
acctl steps set "Analyze the code:todo,Implement:in_progress,Tests:todo,PR and review:todo"

# On every change: republish the WHOLE list (full replacement,
# idempotent — never incremental edits, no index drift)
acctl steps set "Analyze the code:done,Implement:done,Tests:in_progress,PR and review:todo"

# See the current state (debug)
acctl steps get
```

- The compact form is `name:status` separated by commas; the status is
  after the **last** `:` (a name may contain `:`).
  Accepted aliases: `wip`, `in-progress` → `in_progress`;
  `complete`, `finished` → `done`; `skip`, `skipped` → `abandoned`.
- One spec argument is the documented form. You may also pass several
  positional args — each becomes one entry (`"a:todo" "b:done"` = two
  steps) — but a step **name must never contain a comma**: it would be
  split into two entries (comma is the entry separator, there is no
  escaping).
- JSON form also accepted:
  `acctl steps set '[{"name":"Tests","status":"in_progress"}]'`
- Clear the list: `acctl steps set -` (or `[]`).
- The list is **bounded to ~100 steps** — beyond that the daemon
  refuses; merge or abandon completed steps.
- The daemon emits a `steps` event on every change → live client
  update; `session_info` carries `steps` to rebuild on open. `/clear`
  and rebinding to another native session clear the list (the cycle
  starts over).

## When to publish

- **At the start** of a non-trivial task: split into 3-8 steps that
  represent the plan (don't over-split — the panel must stay readable).
- **At every step transition** (todo → in_progress → done).
- **When the plan changes**: republish the whole list with revised
  steps (`abandoned` for dropped ones).
- No need to republish on every trivial action — only when a step's
  state changes.

## Compatibility / other providers

The protocol is `session_set_steps` (single RPC, full list) —
provider-independent. This skill lives in `.devin/skills/ac-steps/`
(standard SKILL.md format): Devin agents see it natively.
For Claude Code / Codex / Cursor, copy or link this directory into
their skills directory (`~/.claude/skills/`, `~/.codex/…`,
`~/.cursor/…`) — the content stays identical, only the path changes;
`acctl` and `$AGENTCONTROL_SESSION` are already available in every
session spawned by the daemon, whatever the provider.
