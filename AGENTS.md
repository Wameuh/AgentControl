# AgentControl — working rules

## Workflow: `next_release` → `main`

Two protected branches (**no direct push**, PR required,
CI checks: `daemon-linux` ×2, `client` ×2, `client-macos`,
`client-android` — 6 runs, 5 of them required by the ruleset,
`client-macos` still to be included):

- **`next_release`** = integration branch. **Every feature/fix PR
  targets `next_release`** (`gh pr create --base next_release`)
  and accumulates there.
- **`main`** = release branch. Only fed by merging
  `next_release` when the user decides to release.

Workflow: create a branch from `next_release` → push →
`gh pr create --base next_release --draft` → finish the work →
`/mr review` **on the draft** → handle the review (fix → push →
re-run, still in draft) → review settled → `gh pr ready`
(≥30 s after the last push) → wait for green CI checks → merge.
**Merge**: to `main`, always the user. To `next_release`,
**the agent may merge** via `gh pr merge <N>` when the latest auto
review carries a **`Verdict: APPROVED` title** posted after the push
of the current head (freshness verified via the repo's PushEvents,
`activity` feed fallback — events can lag by hours) —
otherwise the `.devin/` hook blocks (`git merge`, API, GraphQL,
`--admin`, `main`: always forbidden). Tripwire: an auto-posted
`Verdict: APPROVED` comment would pass — the rule remains to only
merge on a real review. PR already merged?
`git checkout next_release && git pull` to resync.
CI builds all artifacts on PRs (APK suffixed `-<sha>`).

**Worktrees**: work in a **dedicated git worktree per task**
(`git worktree add ~/worktrees/ac-<feat> -b <br> origin/next_release`)
— the main checkout stays clean and several tasks can progress in
parallel without stepping on each other. Clean up at the end:
`git worktree remove ~/worktrees/ac-<feat>`. **The main checkout
is locked**: the `.devin/hooks/guard-main-worktree.py` hook
blocks file tools (`write`/`edit`/…) on any path resolving into it
and mutating git commands targeting it (`commit`, `add`,
`checkout`/`switch` off `next_release`, `stash`, `clean`, `rebase`,
`update-ref`…); a `Stop` hook refuses to end while it's dirty
or off `next_release`, and `SessionStart` reports the state.
`fetch` / `pull` / `reset` / `restore` / `checkout -- <paths>` stay
allowed (resync and back-to-clean). Accepted tripwire: shell writes
(`>`, `sed -i`) aren't intercepted — the `Stop` hook is the safety net.

**Drafts**: a PR stays **draft through the whole work + review
cycle** — pipelines only run **after** the review (`gh pr ready` =
final green light: `ready_for_review` event → CI checks). **The
review is requested on the draft**: `/mr review` launches the session
even without ready (the trigger doesn't gate on `isDraft`) → iterate
(fix → push → re-run `/mr review`) for as many rounds as needed while
staying draft → **one full CI run per PR**, not one per round.
Any change on a non-draft PR → switch it back to draft **before**
touching it: `gh pr ready --undo`, then `gh pr ready` when done.
**CI doesn't run on drafts** (all jobs are skipped). **Leave ~30 s
between the last push and `gh pr ready`** — otherwise the draft
`synchronize` dispatch (all skipped) may materialize after the ready
run and cancel it (same concurrency group) → head with no CI
(happened live, merge blocked).
⚠️ `gh pr checks <N> --watch` exits green on skipped or
cancelled runs → gate on the head's **check runs**, not on run
metadata (`workflow_run.event` never reports `ready_for_review`,
and `createdAt` is a race):

```bash
SHA=$(gh pr view <N> --json headRefOid -q .headRefOid)
for i in $(seq 1 20); do
  COUNT=$(gh api --paginate "repos/{owner}/{repo}/commits/$SHA/check-runs?per_page=100" --jq \
    '.check_runs[] | select(.name | test("^(daemon-linux|client)( |-|$)"))
      | select(.conclusion != "skipped" and .conclusion != "cancelled") | .name' | wc -l)
  [ "$COUNT" -gt 0 ] && break; sleep 15
done
if [ "${COUNT:-0}" -eq 0 ]; then
  echo "aucun check run réel — head à réparer (gh pr ready --undo &&
        sleep 30 && gh pr ready), ou relancer depuis la racine du clone"
else
  gh pr checks <N> --watch
fi
```

Run from the clone root (`{owner}/{repo}` = repo of the cwd);
the name filter must track `build.yml`'s required jobs. Without
the failure branch, an all-skipped head would come out green after
~5 min — the exact case the gate detects.

**Rebase before review**: if `origin/next_release` moved since the
branch — `git merge-base --is-ancestor origin/next_release HEAD` false
**after `git fetch`**, or `gh pr view <N> --json mergeable` →
`CONFLICTING` (`UNKNOWN` = async GitHub computation → retry, not "ok") —
rebase/merge **before** launching the review. The verdict is anchored
to the head SHA: any later push invalidates it → the verdict is lost
and one more round is mandatory (happened: #35 approved on a stale
base → rebase → re-review). Lost ≠ ignored: if the review already
posted findings, they still stand — **handle the feedback at the
same time as the rebase** (fix + rebase in the same push), then
re-run the review. An up-to-date base also avoids having a diff
reviewed that the merge would rewrite. If `next_release` moves between
the APPROVED and the merge, the verdict stays valid but GitHub may
refuse the merge on conflict → rebase + re-review.

**Auto review**: when the work looks done **(PR still draft)**,
**note `TS=$(date -u +%FT%TZ)`**, comment `/mr review`
(`gh pr comment <N> --body "/mr review"`),
then **poll the comments** until the review — the automation
first posts an ack (`Launching a review…`) and the review may
come out under **the user account OR the bot** → don't filter the
author: match the marker **posted after `$TS`** (`--paginate`
required — recent comments are on the last page;
`Review|Verdict` since the format varies: `## Review` / `## Verdict: …`):
`gh api --paginate 'repos/{owner}/{repo}/issues/<N>/comments?per_page=100' --jq '.[] | select(.body | test("^[*#]+ ?(Review|Verdict)")) | select(.created_at > "'"$TS"'") | .body'`.
As soon as it's posted, address it — no fixed wait
(give up after ~10 min if nothing).
Changes to make → fix and push **staying draft**,
then re-run `/mr review`; `gh pr ready` only once the review
is settled (see **Drafts**). NB: a review
launched right after a push may read a stale diff → anchor the
reading on `headRefOid` — but `headRefOid`/`isDraft` can
themselves lag ~1 min post-push → cross-check with the branch tip
(`gh api repos/{owner}/{repo}/branches/<br>`) or wait ~60 s.
The command is posted **alone** in its comment: the trigger
matches `contains` on the body → any quote of `/mr review` in
another comment relaunches a session (to quote it:
`/mr&#32;review`).

## Releases

**No `v*` tag or release without an explicit user request.**
Pushing commits and bumping the version, yes — tagging/publishing, no.

The **release PR is auto-created** by `auto-release-pr.yml`: on every
push to `next_release`, a `next_release` → `main` draft PR is
opened if absent — to release you just switch it **ready**
(going non-draft is the trigger, no label required).
Creation goes through
**`RELEASE_PAT`** (fallback `GITHUB_TOKEN`): the repo setting
"Allow GitHub Actions to create and approve pull requests" forbids
`createPullRequest` to `GITHUB_TOKEN`; without `Pull requests: RW` on
the PAT the job fails red (happened: run 40210287826, attempts before
the scope was added).

**Release notes in English**: `release_notes.py` categorizes the
squash subjects (often French) but takes its labels from
`client/tools/release_notes_en.txt` — **every `feat`/`fix`/`perf`
PR adds a line `<N>: <english user note>`** (otherwise the raw
subject is reused and the generator flags it in the workflow logs).
Lines from already-released PRs are inert — can be purged each
cycle.

Release = `next_release` → `main` PR switched **ready** →
`version.yml`'s `release-bump` job computes `last tag + level`,
commit `chore: bump version` pushed to `next_release` (the PR
updates, CI checks run on the bumped head) → at merge (by the
user), `release-on-merge.yml` tags `v<version>` (read from
`client/package.json`) and dispatches `build.yml` on the tag (a tag
push via GITHUB_TOKEN doesn't trigger workflows, hence the
dispatch). Idempotent: existing tag → skip, package.json already at
NEXT → no re-bump. While the PR stays draft, nothing bumps.

**Release candidate**: when the release PR goes ready, `release-bump`
also pushes a `v<NEXT>-rc.<k>` tag (incremental k, one rc per head
state — a post-ready synchronize produces rc.k+1) → the tag push via
PAT triggers `build.yml` which publishes a **GitHub prerelease**
(never marked `latest`). The rc build **injects the `-rc.k` suffix
into version files** (not committed — the tag points to a clean
X.Y.Z commit) → the binary identifies as `X.Y.Z-rc.k`, and the
final `X.Y.Z` is offered as an update afterwards (semver: rc <
stable). Windows and macOS are excluded from injection (MSI/NSIS and
CFBundleVersion require numeric) — consequence: a Windows/macOS rc
artifact identifies as `X.Y.Z` → no auto-update path to rc.k+1 nor to
the final, manual reinstall. Linux bundling is limited to `deb`+`appimage`
(`client/src-tauri/tauri.linux.conf.json`): the rpm isn't distributed
and its validator refuses `-` in `Version` (semver `-rc.K` —
RPM convention = `~`, upstream fix tauri-apps/tauri#16336 open). The
.deb `X.Y.Z-rc.K` is accepted by dpkg and `update_client` installs via
`pkexec dpkg -i` (no apt repo → Debian ordering doesn't apply). To
install the rc from
the app: opt-in rc channel in settings → the probe switches from
`releases/latest` (which excludes prereleases) to the `/releases`
list + max semver. At merge, `release-on-merge` cleans up the cycle's
`v<NEXT>-rc.*` releases+tags — and a release PR closed without merge
triggers the same cleanup (`rc-cleanup` in version.yml).

**Reconcile**: every nr→main merge is a squash → the `main` commit
is never an ancestor of nr commits: without reconcile the next
release would re-conflict on every retouched zone (happened: #23, #29),
and the release PR would list ALL of nr's history (#39: 25 commits
going back to #11, injected by GitHub into the squash message —
`COMMIT_MESSAGES` setting). The `reconcile-next-release` job (push to
`main`) re-establishes the link, in two modes: **identical trees**
(nominal case post-release — nr has no content outside `main`) →
**reset** `next_release` = `main` tip with `--force-with-lease`: the
past cycle's commits leave nr → the next release PR only lists the
current cycle, merge-base = `main` tip; **different trees** (feature
merged inside the reconcile window, conflict resolutions on `main`,
direct hotfix mid-cycle)
→ classic **merge** (real recursive merge, hotfix propagated, conflict
= red job fail-closed, never a destructive reset).
⚠️ After a reset, nr is rewritten: local clones →
`git fetch && git reset --hard origin/next_release` (not `pull`);
an open PR based on the old nr → `git fetch && git rebase --onto
origin/next_release <old-tip>` — otherwise its commit list
shows the old nr history (the diff stays correct, but the old
`(sync)`/`(major)` subjects would re-enter the squash body → possible
over-bump on the next cycle).
⚠️ `release-bump` and `reconcile-next-release` push to
`next_release` via the **`RELEASE_PAT`** secret, which
`auto-release-pr` also uses to create the release PR. Fine-grained PAT
for the repo, required scopes: **Contents RW** (both pushes),
**Workflows RW** (reconcile: the merge imports `main` squashes touching
`.github/workflows`, a PAT without that scope is refused regardless of
ruleset bypass), **Pull requests RW** (auto-release-pr:
`createPullRequest`):
`github-actions[bot]` can't be a ruleset bypass actor
(first-party app → 422), and a `GITHUB_TOKEN` push wouldn't trigger
checks on the pushed head (anti-recursion). The `next_release`
ruleset has the **`Repository admin`** role as bypass actor
(Settings → Rules → Rulesets). Without `RELEASE_PAT`: the
`release-bump` guard fires as soon as `next` is known (the rc tag
still pushes, even with the bump skipped) → fail-fast "missing
RELEASE_PAT secret" on any non-draft run; `reconcile` stays
skip-clean with no push.
Fine-grained PATs expire → renew the secret at expiry
(otherwise opaque failure at checkout).

## After each push: check CI

Mandatory, don't forget:

```bash
gh run list --limit 3 --json databaseId,conclusion,name
gh run view <id> --log-failed   # if failure
```

The `.github/workflows/build.yml` workflow builds: daemon x86_64+aarch64,
Tauri client Ubuntu+Windows+macOS (arm64 dmg), Android APK, and
publishes a release on `v*` tags.

## Automatic versioning (commit keywords)

PR → **`next_release`** (not `main` directly). CI checks
(`daemon-linux` ×2, `client` ×2, `client-android`, `client-macos` —
5 required by the ruleset, `client-macos` to be included). CI builds
artifacts on PRs (APK and dmg suffixed `-<sha>`).

**No per-PR bump** — the version only moves at release. The level
is decided by **parenthesized keywords in commit subjects**
scanned over the cycle. Scan bound = last `chore: bump version`
**whose `vX.Y.Z` tag exists** on `next_release`, otherwise (post-reset:
nr no longer has a bump commit, its history = `main`'s) **last
semver tag**, which is then an ancestor of nr — self-maintained bound
in both cases:

- `(major)` → **major** (exceptional, major release decision);
  `(sync)` → **minor**; nothing → **patch**
- `(sync)` = **the only minor trigger**: blocking client↔daemon
  protocol incompatibility/dependency — it's the x.y bump that
  triggers auto-update (`minorOf` ≠ comparison → peer popup/update).
  Any feature without protocol breakage = patch, even a big one.
- `(minor)` = **deprecated, no longer used**: ignored, treated as
  patch — `detect_level` logs "note : (minor) déprécié" when it
  meets it. Use `(sync)` for protocol breakage, `(major)`
  exceptionally, nothing otherwise.
- Write the **parenthesized** keyword in the subject of the commit
  that motivates it: `feat: nouveau champ proto (sync)` — a bare
  word (`a major bug`) must not count. **At squash, the element must
  be kept** — GitHub includes subjects in the squash body by default:
  don't edit it out of the message.
- To **quote** a keyword in prose without triggering it (docs, a
  message talking about the rule), write it **without parentheses**
  ("sync") — `` `(sync)` `` matches even inside backticks, including
  in commit bodies (scanned at squash).
- The rule lives in `client/tools/bump_version.py` (`detect_level`,
  `--detect-level` mode) — source of truth called by `release-bump`
  (version.yml) and `release_notes.py`; don't duplicate it elsewhere.
- Releases number from the **last semver tag**, not from
  `package.json` — the file version can drift between releases,
  it's realigned at the release bump.

Manual off-PR: `python3 client/tools/bump_version.py [major|minor|patch|x.y.z]`.

## Before each push (PR branch)

- **Frontend**: `cd client && npm run build` (tsc + vite).
- **Rust client (src-tauri)**: no webkit on the Pi → typecheck via
  the Windows target:
  `cd client/src-tauri && PATH="/tmp/zig-shim:$PATH" cargo check --target x86_64-pc-windows-gnu`
  (`windres` stub in /tmp/zig-shim required).
- **Daemon**: `cargo build --release -p agentcontrol-daemon` (build
  only). **Forbidden to touch the live daemon**: no
  restart/stop/redeploy by an agent (`systemctl`, `supervisorctl`,
  kill, binary replacement) — `agentcontrol-daemon` orchestrates the
  agent sessions (including ours): restarting it kills all running
  agents (sessions → interrupted). Redeployment is a user-only
  action.
- **Removing a crate/binary**: think of the CI workflow
  (`-p <crate>` in build.yml) and `CRATES` in `tools/bump_version.py`
  (the PR's bump runs the **base**'s script → editing `CRATES`
  only helps post-merge bumps; removed crate = fail-closed red job,
  non-blocking).

## Android

- `gen/android` NOT committed — generated by `npx tauri android init --ci`
  in the `client-android` job (the generated manifest already includes
  `INTERNET`).
- SSH: no ssh binary → `Transport::SshNative` transport (russh) forced
  on Android, opt-in on desktop (`native: true` in the cfg).
  `direct-tcpip` replaces `ssh -L`; `ssh_native::ssh_exec` replaces
  `ssh host`.
- App-generated ed25519 key: `<base>/agentcontrol_ed25519` — on Android
  `client_core::init_paths(app_data_dir)` since HOME is absent.
- `ureq`, `tauri-plugin-shell`: desktop-only (target cfg in src-tauri/
  Cargo.toml); `check_update`/`update_client` return Err on Android.
- CI APK: aarch64 debug-signed (artifact + release). `--target` to add
  other ABIs.

## Code conventions

- **Quality and evolvability first**: all development is done with
  code quality and easy evolution in mind — favor clear abstractions,
  clean decoupling and readable code over a quick solution that will
  become hard to modify.
- **Files ≤ 1000 lines**: avoid files exceeding 1000 lines; if a
  file approaches or exceeds that size, consider splitting it into
  coherent modules/components.
- **Tests**: no unit tests required — favor integration tests that
  validate real flows (daemon ↔ client, ACP calls, SSH, …).

## Files protocol (viewer/editor)

- `read_file{path, offset, limit}`: with `limit` → paged response
  `{path, offset, size, data(base64), mtime_ms}` (raw bytes — the
  client decodes, not the daemon; supports files of any size and
  acvideo video streaming). Without `limit` → legacy response
  `{content, truncated}` UTF-8 512 KiB.
- `write_file{path, offset, delete_len, data(base64), if_mtime_ms?}`:
  splice bounded to the workspace (`resolve_in_cwd_create` — parent
  canonicalized, absent leaf OK, `..`/symlink escaping refused);
  `delete_len` clamped; `if_mtime_ms` ≠ disk mtime → `conflit:` error
  (the client offers `Reload`/`Overwrite` — `Recharger`/`Écraser` in
  the fr locale); atomic `.part`+`rename`
  write in `spawn_blocking`, unix mode preserved; `data` cap 16 MiB
  encoded. Response `{path, size, mtime_ms}`.
- `create_path{path, dir}`: EXCLUSIVE creation of an empty file or
  directory (explorer right-click "new") — same `resolve_in_cwd_create`
  bound, root refused, target already present (any form, symlink
  followed) → "already exists"; O_EXCL = no overwrite even in a race.
  Response `{path, dir}`. CLI: `acctl create <session> <path> [--dir]`.
- The client sends `offset:0, delete_len:loaded` (buffer = disk
  prefix → save without having loaded everything); CRLF and BOM
  rewritten; encodings: utf-8, windows-1252, utf-16 le/be — decoding
  via streamed `TextDecoder`, homemade encoding (`core/encoding.ts`).
- `acctl write <session> <path> [src|-]` = full replacement
  (`delete_len: u64::MAX`, no mtime guard).

## Known pitfalls

- `anyhow::bail!` in a `Result<_, String>` function → E0308.
- `ureq`: `native-tls` feature (not rustls/ring → no C compiler
  required for the Windows check); `libssl-dev`/`pkg-config` needed
  on Ubuntu (CI). NB: russh **requires** ring (C) — the Windows check
  passes via the zig shims `/tmp/zig-shim/x86_64-w64-mingw32-gcc{,++}`
  (`zig cc -target x86_64-windows-gnu`, filter cc-rs's `--target=`).
- `flex-shrink:0` required on items of a scrollable flex-column.
- Tauri 2: `set_var` is `unsafe`; `get_webview_window` requires
  `use tauri::Manager`; `Image::new_owned` takes raw RGBA.
- `cursor-agent acp`: **hidden** subcommand (absent from --help),
  no arguments — no {mode}/{model} placeholder possible, the mode/
  model requested at creation is applied via set_config_option
  post-spawn.
- Codex: no native ACP → `@agentclientprotocol/codex-acp` adapter
  (Node; zed-industries/codex-acp archived). **Absolute** `CODEX_PATH`
  required to the standalone codex binary: otherwise the adapter takes
  its bundled `@openai/codex` npm (optional dep missing on arm64), and
  via npx `codex` in PATH is the npm wrapper, not the real CLI.
- Claude: `@agentclientprotocol/claude-agent-acp` bundles the Claude
  Agent SDK → `claude` CLI unneeded. Auth: ANTHROPIC_API_KEY or
  CLAUDE_CODE_OAUTH_TOKEN (`claude setup-token`, headless OK).
- Minimal systemd PATH: `ensure_agent_path` (main.rs) injects the
  binary's dir + ~/.local/bin + nvm (`$NVM_BIN`, else latest
  `~/.nvm/versions/node/*/bin`) — required for npm-global agents
  (`#!/usr/bin/env node` shebang).
- **Interrupted drag → stuck "grabbing" cursor** (WebKitGTK/Wayland,
  toolkit bug with no web-side remedy): NEVER detach an element that
  can be a drag source (list rebuild, `.remove()`) while a drag is in
  flight — cf. `dragging` counter + deferred rebuild in `sidebar.ts`.
  `startDragging()` only on `e.detail===1` (a 2nd mousedown during
  toggleMaximize kills the WM grab the same way). `<a>`/`<img>`
  disarmed via `-webkit-user-drag:none` (base.css); keep `draggable`
  only on real DnD sources.
