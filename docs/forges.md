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
rate-limit, `gh` missing/unauthenticated) → retries in backoff over
~15 min then **give up** — detection stays best-effort: a URL
reappearing later in output restarts the cycle (and journal backfill
at restart replays outputs).

Safeguards: queue deduplicated by reference key (cap 256 — a flood of
distinct URLs overworks neither the forge nor memory); a reference
already in the registry never goes back to verification (synchronous
detection takes over: possible "strong" reattribution); entry created
in the meantime (`pr add`, other queue) → verification skipped at
processing time.

Consequence on a daemon **without working `gh`**: GitHub URLs cannot be
verified → no auto-recording (retries run out); `acctl pr add` remains
the fallback — a deliberate choice: an entry this daemon could never
re-read at sync time would have no value anyway.

## Configuration

Daemon `config.toml` (`$AGENTCONTROL_HOME/config.toml`):

```toml
[[forges]]
type = "gitlab"                    # "gitlab" | "gerrit"
hosts = ["gitlab.example.com"]     # host[:port][/prefix] without scheme
token_env = "GITLAB_TOKEN"         # optional — NAME of an env var
                                   # holding the token (never the
                                   # token itself in the config)

[[forges]]
type = "gerrit"
hosts = ["gerrit.corp:8080", "example.com/gerrit"]  # port and path
                                                    # prefix OK
```

- `github.com` and `gitlab.com` are implicit — no need to declare them.
- The same host may serve several forges: the URL path discriminates.
- The effective set is exposed via the `forge_list` RPC.

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

- **GitHub**: `gh pr view|list` (unchanged).
- **GitLab**: `GET /api/v4/projects/<p%2Fenc>/merge_requests/<iid>` —
  anonymous on public projects; for a private instance, declare
  `token_env` and export the variable in `acctl`'s env
  (`PRIVATE-TOKEN`). A 401/403 or an outage leaves the status unchanged
  with a warning.
- **Gerrit**: `GET /changes/<ref>` anonymous — works on public instances
  (verified on `gerrit-review.googlesource.com`). `status` NEW → open
  (+ `work_in_progress` → draft), MERGED → merged, ABANDONED → closed.
- **Host scope**: collection only queries declared hosts (`forge_list`
  — gitlab.com implicit for gitlab, github.com for github); a registry
  entry on an undeclared host is noted "undeclared host — status
  unchanged", never queried.

Each sync pass also tracks **seen** and **unreachable** entries: the
registry's `sync_fails` is reset to zero on the first live read and
incremented otherwise (dead or undeclared host, missing repo or
reference — for GitHub the failure only counts if `gh auth status`
answers: a daemon without working gh penalizes nothing). Past **3
consecutive failures** the entry becomes **"unresolvable"**: dedicated
state and dimmed line in the Repos tab, out of the counter and session
badges (status restored as soon as a sync re-reads it — a host coming
back unmasks the entry).

Merging a review stays a **GitHub-only permission**: the PR merge hook
(`/mr review`, CI gates) does not extend to GitLab or Gerrit — the
registry there is advisory (visibility, attribution, status).
