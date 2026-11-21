# Multi-forge review registry (GitHub, GitLab, Gerrit)

The daemon maintains a *review reference ↔ session* registry
(`prs.json` at its root): when an agent creates a PR/MR/change in a
session, the reference is automatically attributed to it and the badge
shows up in the sidebar (`acctl list`, `acctl pr ls`).

## Automatic detection

Each session's watcher scans tool call excerpts:

- **GitHub**: `github.com/<o>/<r>/pull/<N>` — always on.
- **GitLab**: `<host>/<group>/<project>/-/merge_requests/<N>` — `gitlab.com`
  implicit, self-hosted hosts via `[[forges]]`.
- **Gerrit**: `<host>/c/<project>/+/<N>`, `/r/` and `#/c/` variants,
  `/q/I<sha>` search URLs — declared hosts only (nothing tells a Gerrit
  path from another service without a declaration).
- **`Change-Id: I<40 hex>`** (commit trailer in a `git log`,
  `git show`, `git review`): recorded as an unresolved Gerrit change
  (`pr` = 0, badge `c#I<8>`); the number is resolved at the next
  `acctl pr sync` and the entry re-keyed, keeping the Change-Id.

Strong (re)attribution signal: `gh pr create`, `glab mr create`,
`git review`, push `refs/for/*` in the tool call title.

Tool outputs also reproduce the content of files being read
(docs, specs, `AGENTS.md`, tests) — two barriers keep their URLs
from creating phantom entries:

- a **placeholder denylist** at extraction: any repo path where every
  segment fits in a single character or is a generic label
  (`o/r`, `g/p`, `owner/repo`, `grp/sub/proj`, `a/b`, `foo/bar`,
  `example/test`…) is ignored before it can even become a reference;
- an **existence check on the forge** for every *new* reference: the
  entry is created only if the forge actually knows the repo AND the
  review — a plausible but made-up URL (`corp/tools`,
  `infra-team/deploy/tool`…) never gets in. A manual `pr add` stays
  possible (explicit intent, never verified), and GitLab/Gerrit only
  match declared hosts.

## Existence check (auto detection)

An unknown reference extracted from an output goes into a dedicated
**verification queue** (single worker, one forge call at a time,
~300 ms between calls) — the watcher never waits on the network:

- **GitHub**: `gh api repos/{o}/{r}/pulls/{n}` (`gh` auth required);
- **GitLab**: `GET <host>/api/v4/projects/<enc>/merge_requests/<iid>` —
  declared hosts only, `token_env` honored;
- **Gerrit**: `GET <host>/changes/<num|Change-Id>` anonymous — declared
  hosts only.

Verdicts: **exists** → the entry is recorded ("PR detected" event as
before); **nonexistent** (404/410, or 401/403 = never readable by this
daemon; undeclared host) → rejection + **bounded negative cache**
(1024 entries, 30 min TTL): re-occurrences of the same URL are ignored
without a new call, but a reference created *after* being quoted becomes
recordable again on expiry; **forge unreachable** (outage, 5xx,
rate-limit, transport missing or unauthenticated) → retries in backoff
over ~15 min then **give up** — detection stays best-effort: a URL
reappearing later in output restarts the cycle (and journal backfill
at restart replays outputs).

Safeguards: queue deduplicated by reference key (cap 256 — a flood of
distinct URLs overworks neither the forge nor memory); a reference
already in the registry never goes back to verification (synchronous
detection takes over: possible "strong" reattribution); entry created
in the meantime (`pr add`, other queue) → verification skipped at
processing time.

Consequence on a daemon **without any working transport** (no `gh`,
no `token_env` for GitHub REST): GitHub URLs cannot be verified → no
auto-recording (retries run out); `acctl pr add` remains the fallback
— a deliberate choice: an entry this daemon could never re-read at
sync time would have no value anyway. Declaring
`[[forges]] type="github" transport="rest" token_env="GH_TOKEN"`
removes the `gh` dependency entirely.

## Transports (`transport = "…"`)

Every forge op in the pipeline — orchestrator review (`/mr review`,
`acctl pr review`), registry sync, release-PR guards — goes through a
`forgesync::ForgeClient` resolved per (forge, host, repo). The
`transport` key of `[[forges]]` selects the implementation, per host
group:

| `transport` | meaning |
|---|---|
| `auto` (default) | the forge's CLI if installed **and** authenticated (`gh auth status` / `glab auth status`), else REST |
| `gh` | GitHub CLI — forced even if `gh` would fail the probe |
| `glab` | GitLab CLI (`glab api`) |
| `rest` | direct REST — GitHub REST (`token_env`, implicit `GH_TOKEN` fallback), GitLab REST v4 (`PRIVATE-TOKEN`), Gerrit REST (`/changes/`, anonymous read) |
| `mcp` | accepted, **resolves to `rest`** — see below |
| `git` | reserved — selected implicitly by `type = "git"` |

```toml
[[forges]]
type = "gitlab"
hosts = ["gitlab.example.com"]
transport = "rest"                 # never spawns gh/glab
token_env = "GITLAB_TOKEN"         # NAME of an env var holding the token
```

`[[forges]] type="git"` is the explicit **degraded mode**: a plain git
host with no forge API — no listing, no comments, no automatic review
trigger; `verify_ref` answers "unreachable" so detection and sync stay
inert by design instead of erroring.

**MCP spike — verdict.** A daemon-side MCP client for the GitLab MCP
server was evaluated for `gitlab_mcp`: the server exposes the same
surface as REST v4 (MRs, notes, pipelines) while adding a runtime
dependency (stdio spawn or SSE endpoint, handshake, tool schema
versioning) the pipeline gains nothing from — `post_comment` is one
`POST /notes` call, `list_comments` one `GET`. `transport = "mcp"`
therefore resolves to `rest`; the REST transport covers every op
without a second dependency, and the config key stays valid so a
future client can take the slot without a config break.

## Configuration

The canonical way to edit `[[forges]]` is the **`prs-registry` component
panel** (Composants → `prs-registry` → Configurer): a single `forges`
JSON param holding the declared entries —

```json
[{"type":"gitlab","hosts":["gitlab.example.com"],"token_env":"GITLAB_TOKEN"},
 {"type":"gerrit","hosts":["gerrit.corp:8080","example.com/gerrit"]}]
```

— or `acctl comp set prs-registry forges '<json>'`. The set is applied
**hot**: PR watchers of live sessions, `forge_list` and `pr_sync` see
the new hosts without a daemon restart, and the change is persisted as
`[[forges]]` in `config.toml` (atomic rewrite, same lock as the other
config writers). `token_env` stays an env var **name** — a value that
isn't a valid identifier is rejected (never a token in the config).

`config.toml` remains editable by hand — a `[[forges]]` added, modified
or removed there is picked up at the next `forge_list`/panel read, at
`pr_sync`, and by a 60 s rescan — but the panel/`comp set` is the
canonical path (it validates and normalizes hosts).

Daemon `config.toml` (`$AGENTCONTROL_HOME/config.toml`) — same entries,
hand-edited or panel-written:

```toml
[[forges]]
type = "gitlab"                    # "gitlab" | "gerrit" | "git"
hosts = ["gitlab.example.com"]     # host[:port][/prefix] without scheme
token_env = "GITLAB_TOKEN"         # optional — NAME of an env var
                                   # holding the token (never the
                                   # token itself in the config)
transport = "rest"                 # optional — auto|gh|rest|glab|mcp

[[forges]]
type = "gerrit"
hosts = ["gerrit.corp:8080", "example.com/gerrit"]  # port and path
                                                    # prefix OK
```

- `github.com` and `gitlab.com` are implicit — no need to declare them,
  and they are **not removable** via the panel (they are not `[[forges]]`
  entries — only declared hosts are editable).
- The same host may serve several forges: the URL path discriminates.
- The effective set is exposed via the `forge_list` RPC (implicit +
  declared hosts with their `token_env`).
- **Removing a declared host does not touch existing registry
  entries**: their references stay (attribution, badges, history) but
  stop being synced — an undeclared host is noted "undeclared host —
  status unchanged" at the next `pr_sync` and counts toward
  `sync_fails`. Re-declaring the host resumes sync.

## Repo resolution

The registry keys each reference by `forge + host + repo + number`
(`+ Change-Id` for unresolved Gerrit changes). There is no repo
allowlist: which repo a reference belongs to is decided by the URL
itself. For manual commands the **current clone keeps priority**:

- `acctl pr add` without `--repo` resolves the target from the `origin`
  remote of the cwd (host matched against the declared forge hosts — a
  Gerrit's ssh `:29418` port matches its declared web host);
- `acctl pr rm|note <N>` prefers the entry whose repo matches the
  clone's remote when several candidates share the number;
- a bare `Change-Id: I<sha>` trailer detected in a tool output is
  recorded with an empty repo and resolved at the next `pr_sync`.

Auto-detection never guesses a repo: a reference is only created from
a full forge URL found in a tool output (placeholder-looking repo paths
are refused, and every new reference is existence-checked on the forge
before entering the registry — see above).

## `acctl pr`

```bash
acctl pr ls [--all]                     # badges: #N github, !N gitlab,
                                        # c#N / c#I<8> gerrit
acctl pr add <N|I<cid>> [--session S] [--repo R]
           [--forge github|gitlab|gerrit] [--host H[:port]] [--note t]
acctl pr note <N|I<cid>> <text…> | acctl pr rm <N|I<cid>>
acctl pr sync                           # refreshes statuses
```

The same collection runs daemon-side via the `pr_sync` RPC (shared
`forgesync` crate): the client's "Repos" tab requests it on open then
every 60 s while the tab is active — daemon throttle ~45 s, one run at
a time (`{synced:false, running|throttled:true}` otherwise). In that
context `token_env` is read from the **daemon** env (from acctl's env
for `acctl pr sync`).

Without flags, `add`'s target is inferred from the `origin` remote
(hostname compared to configured hosts — a Gerrit's ssh `:29418` port
matches the declared web host); otherwise GitHub as before.
`rm`/`note` work differently: the number/Change-Id is **resolved in the
registry** — `acctl pr rm 42` finds a GitLab MR or a Gerrit change just
as well as a GitHub PR (full key forge+host+repo+number), `--repo`
(bare, `host:path` or URL), `--forge` and `--host` act as filters, and
an ambiguity lists the candidates.

## Status sync

Collection groups the registry by (forge, host, repo) and issues one
`list_changes` per group on the resolved transport — `gh pr list`,
`glab api`, or REST — falling back to `get_change` per entry missing
from the list:

- **GitHub**: `gh pr view|list` (default `auto`) or REST
  `repos/{o}/{r}/pulls` + check-runs (`token_env`/`GH_TOKEN`).
- **GitLab**: `GET /api/v4/projects/<p%2Fenc>/merge_requests` —
  anonymous on public projects; for a private instance, declare
  `token_env` and export the variable (`PRIVATE-TOKEN`). A 401/403 or
  an outage leaves the status unchanged with a warning.
- **Gerrit**: `GET /changes/<ref>` anonymous — works on public instances
  (verified on `gerrit-review.googlesource.com`). `status` NEW → open
  (+ `work_in_progress` → draft), MERGED → merged, ABANDONED → closed.
- **Plain git**: no transport — entries are never probed.
- **Host scope**: collection only queries declared hosts (`forge_list`
  — gitlab.com implicit for gitlab, github.com for github); a registry
  entry on an undeclared host is noted "undeclared host — status
  unchanged", never queried.

Each sync pass also tracks **seen** and **unreachable** entries: the
registry's `sync_fails` is reset to zero on the first live read and
incremented otherwise (dead or undeclared host, missing repo or
reference — a missing transport never counts: `available()` false
leaves the entry unchanged, a daemon without `gh` penalizes nothing).
Past **3 consecutive failures** the entry becomes **"unresolvable"**:
dedicated state and dimmed line in the Repos tab, out of the counter
and session badges (status restored as soon as a sync re-reads it — a
host coming back unmasks the entry).

## Orchestrator review (multi-forge)

The native review (`pr_review`, `/mr review` comment or
`acctl pr review <N>` / `agentcontrol_request_review`) resolves the
forge client of the target repo — the `origin` remote of the routed
workspace first, then the registry entry — and serves the same
pipeline on any declared forge: trigger comments are read via
`list_comments` (notes GitLab, messages Gerrit — system notes skipped,
membership resolved per transport), the head is fetched via the
forge's change ref (`pull/N/head`, `merge-requests/<iid>/head`,
`refs/changes/…`) with the SHA fallback, and ack/verdict/failure notes
are posted back through `post_comment`. A forge without comment caps
simply skips comment scans; `git` entries never reach the pipeline.
The verdict comment keeps the same `## Verdict:` marker whatever the
forge — merge hooks consuming it stay forge-side by design.

Merging a review stays a **GitHub-only permission**: the PR merge hook
(`/mr review`, CI gates) does not extend to GitLab or Gerrit — the
registry there is advisory (visibility, attribution, status).
