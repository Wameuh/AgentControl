---
name: acctl
description: Drive the AgentControl daemon — sessions (new/spawn/msg/send/rm), files, PR registry, raw RPC for what acctl doesn't expose (group, rename, file)
argument-hint: "<acctl command or raw RPC>"
allowed-tools:
  - exec
---

# acctl — driving the AgentControl daemon

`acctl` is the daemon's CLI: create/list/delete sessions, send messages
(user or agent→agent), read/write files in a session's workspace, manage
the PR registry.

## Connection

- `AGENTCONTROL_TCP=host:port` → TCP socket (here the container daemon
  listens on `127.0.0.1:7331`)
- otherwise Unix socket `~/.agentcontrol/daemon.sock`
- `AGENTCONTROL_SESSION` = current session id (injected when running
  inside an AgentControl session — useful as `from` for `spawn`/`msg`)
- stderr noise: release binaries ≤ 0.10.4 may print a harmless
  `GLIBC_2.39 not found` warning (tied to the CI runner's glibc — fixed
  with static musl since #266) → append `2>/dev/null` to pipes if the
  warning shows up.

## Commands

| Command | Role |
|---|---|
| `acctl status` | daemon version, session count, self_update |
| `acctl agents` | available agent profiles |
| `acctl list` | sessions (state, agent, title, `[#N]` badges, sub-agents indented `↳`) |
| `acctl new <agent> <cwd> [prompt...] [--title T] [--mode M] [--model X]` | creates a **root** session (no `spawned_by`, no group) |
| `acctl spawn <from> [task...] [--agent A] [--cwd C] [--title T] [--mode M] [--model X] [--group G]` | creates a **sub-session** of `from`: `spawned_by=from`, nested in the sidebar; omitted fields inherited from the parent; explicit `--group` allowed; `task` delivered as a traced PeerMessage |
| `acctl send <session> [--steer|--force] <text...>` | user message → session; `--steer` injects mid-turn, `--force` jumps the queue; a sleeping session wakes automatically |
| `acctl msg <from> <to> [--steer] <text...>` | agent→agent message (PeerMessage journaled on both sides) |
| `acctl events <session> [before_seq] [limit]` | formatted event journal |
| `acctl tail <session> [after_seq]` | follows the journal live |
| `acctl search <session> <query...>` | full-text search in the journal (~20k events/scan) |
| `acctl find <session> <query...>` | files by name in the workspace |
| `acctl create <session> <path> [--dir]` | creates file/dir (exclusive: fails if it exists) |
| `acctl write|wr <session> <path> [file|-]` | writes a file (stdin if `-` or omitted) |
| `acctl download|dl <session> <path> [dest]` | fetches a file/directory back |
| `acctl blob <session> <id> [offset] [limit]` | reads a journal blob |
| `acctl resume <session> [native_id]` | wakes/respawns a session |
| `acctl attach <agent> <cwd> <native_id> [title]` | attaches a native agent session |
| `acctl fork <session>` | forks the native session → new session, same group, `_fork` title |
| `acctl sleep <session>` | puts to sleep (kills the process, keeps the journal — next send wakes) |
| `acctl cancel <session>` | cancels the current turn |
| `acctl kill <session>` | kills the agent without deleting the session |
| `acctl clear <session>` | clears the journal (`/clear`) |
| `acctl rm <session>` | **deletes the session permanently** |
| `acctl mode <session> <mode>` / `acctl model <s> <m>` / `acctl setcfg <s> <id> <val>` | agent runtime settings |
| `acctl cwd [--session S] <path\|->` | declares the session's effective cwd (e.g. your worktree) — the PTY terminal opens there; `-`/`clear` resets to the session cwd |
| `acctl respond <session> <request_id> [option_id | --answer JSON]` | answers a pending question/permission |
| `acctl daemon-update [version]` | daemon self-update (default: acctl's own version) |
| `acctl pr ls|add|note|rm|sync` | PR ↔ session registry (see the global PR-registry rule) |

## Groups

- **Create grouped**: `acctl spawn <parent> --group "a/b" ...` (sub-session)
  or nothing for `new` (root). A spawn without `--group` inherits the
  parent's group.
- **Re-group an existing session**: `acctl` doesn't expose
  `set_session_group` → raw RPC (below). `group: null` removes from the group.
- Names with `/` render as a tree (sub-groups) in the sidebar.

## Raw RPC (what acctl doesn't expose)

NDJSON protocol over the socket: `{"id":1,"req":"<snake_case>",...}` →
`{"id":1,"ok":true,"result":...}`. TCP helper:

```bash
ac_rpc() { python3 - "$@" <<'EOF'
import json, socket, sys
s = socket.create_connection(("127.0.0.1", 7331), timeout=10)
f = s.makefile("rw")
for line in sys.argv[1:]:
    f.write(line + "\n"); f.flush()
    print(f.readline().strip())
EOF
}
ac_rpc '{"id":1,"req":"set_session_group","session":"s-xxx","group":"a/b"}'
```

Useful requests outside acctl:

| req | effect |
|---|---|
| `set_session_group {session, group\|null}` | group/ungroup |
| `rename_session {session, title}` | rename |
| `session_info {session}` | full metadata (available modes/models/commands) |
| `list_sessions` | full JSON metadata (group, spawned_by, context_used…) |
| `queue_list / queue_edit / queue_drop / queue_promote {session, ...}` | pending message queue |
| `list_dir {session, path}` / `delete_path {session, path}` | files |
| `list_native_sessions {session}` | resumable native sessions of the agent |
| `term_open/term_input/term_resize/term_close` | PTY terminal (client usage) |
| `session_set_cwd {session, cwd}` | effective cwd for term_open (`cwd: null` clears) |

Full reference: `ReqKind` in `crates/proto/src/lib.rs` (~l. 430).

## Guardrails

- **Never** stop/restart the daemon itself (`systemctl`,
  `supervisorctl`, process kill) — it orchestrates every agent session,
  including yours (AGENTS.md rule).
- `rm` is final; prefer `sleep` to free a session you may resume.
- `spawn` requires a `from` = existing session id; `kill` on a
  `dead`/`starting` session is harmless.
- Client visibility: any session created here appears in the sidebar on
  the next refresh — nested under the parent if `spawned_by` is set
  (`spawn`), root otherwise (`new`).
