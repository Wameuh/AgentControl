---
name: ac-wait
description: Delegate a wait to a background waiter that polls at low noise and pings the session via acctl send when the condition lands — the agent yields the turn instead of looping
argument-hint: "[opts] -- <cmd...> | --until-pid|--until-file|--until-port|--until-pid-file | --list|--cancel|--log"
allowed-tools:
  - exec
---

# ac-wait — delegated waiters

`ac-wait` spawns a detached poller (setsid+nohup) that watches a condition
and sends the result to your session via `acctl send` — the message arrives
as a normal message (the session is woken if asleep). You yield immediately
after the spawn instead of burning turns looping.

Script: `tools/ac-wait` in the AgentControl repo, installed at
`~/.local/bin/ac-wait` (symlink). Deps: bash + `acctl` only
(`AGENTCONTROL_TCP` honored → works from a remote host too).

## WHEN to use it

Golden rule: **if you're about to run `X --watch` or a
`while ! cond; sleep` loop in a tool call → ac-wait instead.**

- wait for a PR's CI checks (`gh pr checks <N>` in a loop)
- wait for a PR to merge (`gh pr view <N> --json state`)
- wait for a lock/flock to release, for a backlog item to go done
- wait for a long build, a process to die, a port/daemon to come up
- wait for a remote condition over ssh (`-- ssh host test ...`)
- sleep N minutes (`-- sleep 600` → ping on wake)

## PROTOCOL

1. **Spawn** — returns a wait-id immediately:

   ```bash
   ac-wait --name "checks PR #123" -- gh pr checks 123 --required
   # → w-1730000000-12345
   ```

2. **Continue** — do something else or end your turn
   ("waiting for the ac-wait ping"). To actually sleep:
   `acctl sleep "$AGENTCONTROL_SESSION"` — the waiter's ping wakes the
   session automatically.

3. **Ping** — `[ac-wait] <name> : OK|TIMEOUT after <duration>` arrives
   queued + the last output lines of the command. Resume where you left
   off.

The condition is re-evaluated every `--interval` s (default 30) until
exit 0; `--timeout` (default 2 h) → TIMEOUT message. The message is
bounded (~2 KB, tail of the last outputs).

## Examples

```bash
ac-wait --name "CI #123" --interval 60 -- gh pr checks 123 --required
ac-wait --name "merge #123" -- bash -c 'gh pr view 123 --json state -q .state | grep -q MERGED'
#  ^ tip: the condition = "the command exits 0" — pipe/filter freely
#    BUT inside bash -c: a bare `|` in the calling line splits the command
#    (the waiter would only get the first member → immediate ping).
ac-wait --until-file /tmp/build.done --name "build done"
ac-wait --until-pid 4207 --name "batch died"              # pid already dead → instant OK
ac-wait --until-port 127.0.0.1:8080 --name "daemon up"
ac-wait --until-pid-file /run/my.pid --name "my daemon stopped"
ac-wait --name "remote deploy" -- ssh prod 'systemctl is-active --quiet app'
ac-wait --name "pause" -- sleep 900                       # simple timer
ac-wait --list                    # live waiters (id, session, age, condition)
ac-wait --cancel w-1730000000-12345   # kills + notifies "cancelled"
ac-wait --log w-1730000000-12345      # tail of the log (polls, notif)
```

`--session SID` targets another session (default `$AGENTCONTROL_SESSION`).

## Pitfalls

- **Orchestrator nudge inhibited**: while a waiter lives, the orchestrator
  won't nudge the session (the daemon keeps an `acwait_pending` guard —
  stale waiter/dead pid/timeout exceeded → the nudge resumes its safety-net
  role). So count on the nudge only as a last resort: the waiter must have
  a reasonable `--timeout` for the session to eventually come back.

- **Always a timeout**: a waiter ≠ infinite watch. Tune
  `--timeout`/`--interval` to the scenario (CI: 60-120 s poll).
- **Idempotent, cheap condition**: it runs in a loop — no side effects,
  no heavy/destructive command. A `--watch` that never exits 0 →
  guaranteed timeout (e.g. `gh pr checks --watch`: prefer
  `-- gh pr checks N` without --watch, or `--required` which exits
  non-0 while pending).
- **One waiter per condition**: don't chain/duplicate waiters on the
  same target — cancel the old one or wait for its ping.
- **Delivery**: the ping goes out via `acctl send --steer` → it injects
  **mid-turn** (steer is built for that: real wake, not queued behind a
  long turn); sleeping session → auto-wake.
  The waiter doesn't use `--force` (queue head is reserved for user
  emergencies).
- **Secrets**: the local log only carries rc + output tail, but `meta`
  contains the command and the ping message shows name|cmd — pass
  `--name` and avoid plaintext secrets in `<cmd>`.
- The waiter survives the end of your turn (detached process), not a
  machine reboot.

## Cross-agent note

`ac-wait` is a PATH binary independent of the agent (devin, cursor, …) —
any AgentControl session can use it. The SKILL.md lives in the repo for
devin sessions; for another agent, point the script's path
(`~/.local/bin/ac-wait --help`) in its context.
