# Review queue — local reviews managed by the daemon

The `review-queue` internal component is a **generic local review
queue**: any agent session can ask the daemon to review local work
(one or several repo checkouts, a branch or a diff) and dedicated
reviewer sessions execute the reviews. It is independent of the
`/mr review` pipeline (which is bound to GitHub PRs) — the two
coexist and never share sessions.

Everything is opt-in and tunable — see
[Configuration](#configuration). Request surface: a single CLI
command callable from inside any agent session.

## Request / result protocol

### Requesting a review

```sh
acctl review request --repo /path/to/checkout@branch@base \
    [--repo /other@branch …] [--instructions "what to focus on"]
```

- `--repo` is `path[@branch[@base]]`, repeatable (max 16). `branch`
  empty = the checkout's current branch; `base` empty = the
  reviewer's default base. `--spec '<json>'` accepts the full
  `[{path, branch?, base?}]` form instead.
- The requester session is `$AGENTCONTROL_SESSION` (injected by the
  daemon into agent sessions) — passed automatically. Without it the
  review still runs but no result is steered back.
- Response: `{request, thread, round, state:"queued"}`.

Under the hood: `ReqKind::ReviewQueueRequest {session, repos,
instructions}` (proto — additive extension).

### What the daemon does

1. **Thread** = the sorted `[repo_path, branch]` pairs. One reviewer
   **session per thread**, reused across rounds — round 1 spawns it
   (group `Reviews` by default, title `RQ-<tid>-r<n>`, origin
   `component:review-queue`), round ≥2 reuses it: `/compact` is sent
   first (bounded by `compact_timeout_secs`, skipped when the
   agent's `available_commands` does not announce `compact` — an
   empty list counts as "not announced", unlike `session/compact`
   which treats empty as unknown), then
   the round prompt — `NEW REVIEW ROUND <id>: disregard state from
   earlier rounds; the packet at <abs path> is the only authoritative
   input. Read <abs path> and follow it exactly.`
   Ordering note: a reused session is `asleep` between rounds, so the
   prompt may be sent while the `/compact` turn has not visibly
   started — ordering is guaranteed by the session's own prompt
   queue (`/compact` was posted first); `compact_timeout_secs` only
   bounds the wait when the session stays busy.
2. **Packet by reference**: the full review packet (repos snapshot:
   path, branch, base, head sha, diff instructions, output protocol)
   is written to `<state_dir>/runs/<request_id>/prompt.md`; the
   session only receives `Read <abs path>/prompt.md and follow it
   exactly.` — the packet never travels inline.
3. **Deterministic completion**: the reviewer writes, in the run
   dir, `review.md` + `findings.json` (`{"findings":[{severity,
   file, line, message}]}`) + `complete.json`
   (`{"verdict":"approved|changes_requested|comment","summary"}` —
   written LAST, it is the completion signal). The queue validates
   the schema — "the agent stopped" alone never completes a review.
   The reviewed checkouts are fingerprinted at dispatch (effective
   HEAD + content hash of `status --porcelain`, `diff HEAD` and
   untracked files) and re-checked at completion: a change flags
   the review `tainted`.
4. **Result**: the requester receives a steer envelope — verdict,
   summary, findings count, paths to `review.md`/`prompt.md`.
   Failures (`failed`, `timeout`, `acquire_timeout`) are notified
   the same way.
5. **After completion** the reviewer session is put to **sleep and
   kept** — never deleted while its thread may get another round.
   `review_timeout_secs` → cancel + sleep + `timeout`;
   `acquire_timeout_secs` queued without a slot → `acquire_timeout`.

### Status / config / reaper

```sh
acctl review status            # threads, request states, recent
acctl review config            # effective config (get)
acctl review set <key> <val>…  # patch, persisted to config.toml
acctl review on|off            # persisted enable/disable
acctl review reap              # force a reaper pass now
```

RPC equivalents: `ReviewQueueStatus`, `ReviewQueueConfig{patch}`,
`ReviewQueueReap`. The component `review-queue` also exposes the
same surface via `component_set_params` (client "Configurer" form).

### Reaper

`reaper_enabled` (default true) deletes a reviewer session when:

- **every MR/PR of its thread is merged/closed upstream** — the
  reaper lists changes per repo (branch-matched) via the resolved
  forge transport; one `open`/`draft` or one inconclusive repo
  blocks deletion, **or**
- its idle time exceeds `idle_ttl_secs` (default 5 days, `0` disables
  TTL reaping).

It **never** touches a thread with an active review or a live turn.
Forge resolution per repo: `reaper_forge = auto` detects kind/host
from the `origin` remote (github/gitlab declared forges only);
`github`/`gitlab` force the kind (self-hosted via
`reaper_github_host`/`reaper_gitlab_host`). Tokens come from the
env (`GITHUB_TOKEN`/`GH_TOKEN`, `GITLAB_TOKEN`) — same transports as
the rest of the forge stack. `reaper_interval_secs` bounds forge
polling per thread.

## Configuration

Section `[review_queue]` of `config.toml` (component
`review-queue`, hot-editable — persisted edits survive restarts):

| key | default | meaning |
| --- | --- | --- |
| `enabled` | `false` | master switch — off = requests stay queued |
| `max_parallel` | `2` | reviews running at once (slot semaphore) |
| `poll_secs` | `30` | queue loop period |
| `agent` | `""` | reviewer agent profile — empty = first available |
| `mode` / `model` / `thought_level` | `""` | spawn overrides |
| `group` | `"Reviews"` | sidebar group (empty = none) |
| `title_prefix` | `"RQ-"` | reviewer session title prefix |
| `review_timeout_secs` | `5400` | cancel + `timeout` beyond |
| `acquire_timeout_secs` | `3600` | `acquire_timeout` beyond |
| `compact_timeout_secs` | `180` | bounded `/compact` wait, then proceed |
| `idle_ttl_secs` | `432000` | idle TTL for the reaper (`0` = off) |
| `reaper_enabled` | `true` | reaper on/off |
| `reaper_forge` | `"auto"` | `auto`\|`github`\|`gitlab` |
| `reaper_github_host` / `reaper_gitlab_host` | `""` | host overrides (GHE/self-hosted) |
| `reaper_interval_secs` | `300` | min delay between forge checks/thread |
| `state_dir` | `""` | state + `runs/` — empty = `<root>/review-queue` |
| `prompt` | built-in | packet template (`{REQUEST}` `{ROUND}` `{RUN_DIR}` `{REPOS}` `{INSTRUCTIONS}`) |

State (`review-queue-state.json` + `runs/<id>/`) lives under
`state_dir` and survives daemon restarts — threads keep their
reviewer session across reboots.

## For agent authors

- Request a review: `acctl review request --repo <path>@<branch>`.
- The result arrives as a **steer message** in your session — wait
  for it (or poll `acctl review status`) instead of blocking.
- The reviewer never modifies the reviewed repos; a mutated checkout
  flags the review `tainted`.
- Output protocol for custom reviewer prompts (`prompt` template):
  the reviewer MUST write `review.md`, `findings.json` and
  `complete.json` (in this order, `complete.json` last) into the
  run dir — that is the completion contract.
