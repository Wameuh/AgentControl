# AgentControl — maintainer workflow

> **Internal doc — maintainer scope.** This is the full working-rules
> file used by the maintainer's own automation (orchestrated agent
> sessions, release mechanics, deployment). Contributors: everything
> you need for a PR lives in [AGENTS.md](../AGENTS.md) and
> [CONTRIBUTING.md](../CONTRIBUTING.md).

---

# AgentControl — working rules

## Workflow: `next_release` → `main`

Two protected branches (**no direct push**, PR required,
CI checks: `daemon-linux` ×2, `client` ×2, `client-macos`,
`client-android` (build.yml) + `rust`, `js`, `audit` (test.yml)
— 9 runs, 6 of them required by the ruleset, `client-macos`,
`js` and `audit` still to be included):

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
**the agent may merge** via `gh pr merge <N> --squash --delete-branch`
(squash convention + head cleanup — belt-and-suspenders over the repo
setting `delete_branch_on_merge`) when the latest auto
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
Branch heads are deleted on merge (`--delete-branch` + repo setting):
a worktree whose remote branch vanished keeps working (the local
branch stays), but resync with `git fetch --prune` and drop stale
worktrees with `git worktree remove` — `git branch -D` on a
checked-out branch fails, remove the worktree first.

**Hook config** (`.devin/hooks/`): `guard-main-worktree.py` and
`block-merge.py` are **repo-local policy examples** — written for this
`next_release` → `main` workflow but genericized for reuse. The branch
they enforce resolves, first non-empty wins: env
`AGENTCONTROL_INTEGRATION_BRANCH` → repo
`git config agentcontrol.integration-branch` → the daemon's
`[backlog_orchestrator] integration_branch` (`config.toml`, #316) →
`next_release`. A resolved name absent from the repo's refs leaves the
hooks **inert** (`none`/`off` disables explicitly); `block-merge.py`'s
approving `Verdict:` labels come from `AGENTCONTROL_MERGE_VERDICTS`
(CSV, default `approved,mergeable`). Copying `.devin/hooks` to another
repo is safe — nothing triggers until a branch is configured (see each
script's CONFIGURATION docstring).

**Waiting = `ac-wait`, never in-turn**: any wait longer than ~1 min
(CI checks, `/mr review` verdict, merge watch, long builds)
is delegated to a background waiter, never
spent blocking a tool call. `ac-wait --name "<t>" --interval 60
-- <cmd>` returns a wait-id immediately, re-evaluates `<cmd>`
until exit 0, then pings the session via `acctl send` —
`[ac-wait] <t> : OK|TIMEOUT` arrives as a normal message (and
wakes an asleep session). Arm the waiter, **end your turn**
(optionally `acctl sleep "$AGENTCONTROL_SESSION"` to park for
real — a parked worker keeps its max_workers slot, #280). Never
run a blocking `gh pr checks --watch`, a `sleep`-retry loop or a
`while ! cond` poll in a tool call — that's a slot burned doing
nothing. Waiter commands must be cheap and read-only (`gh
api`/`gh pr view`, file tests, `grep`) — never builds or heavy
commands in a loop. Short one-shot checks (< ~1 min, e.g.
`sleep 30` before `gh pr ready`) may stay in-turn; `ac-wait --
sleep 30` works too if you'd rather yield the turn.

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
and `createdAt` is a race). Arm an **ac-wait waiter** on
`tools/pr-ci-status <N>` — it evaluates once per poll and covers
three failure modes a bare check-runs poll misses: the PR state
(merged / closed / back-to-draft — CI only runs on ready PRs),
`mergeable`/`mergeStateStatus` (a `next_release` merge can flip the
PR to `CONFLICTING` mid-wait → `REBASE-REQUIRED`, exit 0 →
immediate ping instead of an endless `PENDING`), and the check runs
of the **current** head (live `headRefOid` each poll, not the SHA
frozen at arm time — a rebase mid-wait would otherwise poll a dead
head forever). See **Waiting** — never a blocking `--watch` nor a
sleep-loop in-turn:

```bash
ac-wait --name "checks PR #<N>" --interval 60 --timeout 3600 \
    -- tools/pr-ci-status <N>
```

Verdicts — exit 0 notifies with the verdict line as tail, exit 1
keeps polling: `DONE` (PR merged, or all required checks green) →
merge; `FAIL` (red required check → `gh run view <id>
--log-failed`; PR closed; PR back to draft) → fix; `REBASE-REQUIRED`
→ see **Rebase before review** below; `PENDING` keeps polling —
`mergeStateStatus=BEHIND` alone stays `PENDING` (not blocking while
the ruleset's strict status-check policy is off, which is this
repo's setting). `TIMEOUT` with no verdict usually means the head
got **zero real runs** (the late `synchronize` dispatch cancelled
the ready run): repair with `gh pr ready --undo && sleep 30 && gh
pr ready`. Run from the clone root (`{owner}/{repo}` = repo of the
cwd); the name filter inside the script must track `build.yml`'s
and `test.yml`'s required jobs (`js`/`audit` stay advisory, outside
the filter). Without the failure branch, an all-skipped head would
come out green after ~5 min — the exact case the gate detects.
`bash tools/pr-ci-status-test.sh` exercises every verdict against a
stubbed `gh`.

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

The same applies when the conflict is discovered **during the CI
wait** — `tools/pr-ci-status` exits `REBASE-REQUIRED` instead of
polling a head that must change. Procedure: `gh pr ready --undo`
(any change on a non-draft PR goes back to draft first) → `git
fetch origin && git rebase origin/next_release` (resolve the
conflicts) → push (`--force-with-lease`) → re-run `/mr review` on the
draft — the verdict is anchored to the head SHA, the rebase
invalidated it, and any standing findings are handled in the same
push → review settled → `gh pr ready` (≥30 s after the last push) →
re-arm the `pr-ci-status` waiter.

The `/mr review` trigger can be served by the native orchestrator
itself (`pr_review=true` in `backlog_orchestrator` params —
`crates/daemon/src/server/orch_review.rs`): the daemon watches open
`next_release` PRs for a bare trigger comment, spawns a `PRReview-<N>`
session on a detached worktree at the exact head, and posts the
`## Verdict:` comment itself (ack `Launching a review — AgentControl
orchestrator`). Same comment-level protocol — workers change nothing.

The same review can also be **requested internally** (no GitHub
comment): `acctl pr review <N> [--repo R]` or the MCP tool
`agentcontrol_request_review {repo, pr}` — same queue, same guards
(`pr_review`, pool, `pr_rereview`, bounded retries, worktree at the
exact head), same `## Verdict:` comment posted by the daemon. An A2A
requester (`session` set) gets the ack plus the verdict delivered as
a message — no GitHub polling needed; the comment stays the official
merge proof.

**Review flags** (`backlog_orchestrator` params, all hot-settable via
`acctl comp set` and shown in the config UI): each review mechanism
has an independent on/off switch — `plan_review` (plan → review →
implementation pipeline, off = direct worker dispatch),
`pr_review` (native `/mr review` serving), `plan_rereview` (off =
a single plan-review round per item: a denied/conditional verdict on
round 1 stalls directly, as if `max_review_rounds=1`),
`pr_rereview` (off = one review per PR: triggers after a `done`
review are ignored, a `failed` one stays replayable) and
`reviewer_reuse` (off = a fresh `Backlog-R*` session per round,
deleted after each verdict — pre-persistence behavior).
**In-flight disable**: flipping `plan_review` off with items still
`plan_review`/`reviewing` unfreezes them — the daemon posts a
technical `approved` verdict and releases them to `pending`
(direct implementation by a worker), and removes the mapped
reviewer sessions. Flipping `pr_review` off settles running reviews:
reviewer session deleted, worktree removed, `review failed:
disabled` posted so polling workers aren't left waiting.

**WHEN — `review_trigger_mode`** (`backlog_orchestrator` param,
hot-settable, default `comment`): what launches a daemon PR review —
`comment` = a bare `pr_review_trigger` comment on an open change
(historical behavior, forge comment scan: PR comments GitHub, MR
notes GitLab REST, patchset messages Gerrit); `auto_open` = review
when the change first shows up open; `auto_push` = also re-review on
every head differing from the reviewed sha — **auto modes only fire
on trusted changes**: a same-repo source branch or an
`OWNER`/`MEMBER`/`COLLABORATOR` author (fail-closed: fork/unknown
authors are remembered `skipped_untrusted` and never re-evaluated
every tick — `auto_push` rechecks on a new head;
`auto_review_untrusted` relaxes the filter globally or per repo;
`comment` and internal requests are untouched — a human asking is
the validation); `manual` = only
`acctl pr review <N>` / `agentcontrol_request_review` — **no forge
scan at all**; `off` = nothing, internal requests refused too
(in-flight reviews still settle — flip `pr_review` off to abort
them). A forge without `comments` cap (plain `git`) can't serve
`comment` mode anyway — `manual` is its only meaningful mode.
**HOW**: `review_prompt` fully overrides the built-in reviewer
template (same mechanics as `prompt`/`manager_prompt` — placeholders
`{PR}` `{REPO}` `{SHA}` `{BASE}` `{WS}` `{TITLE}` `{BODY}` plus
`{TRIGGER}`, `{VERDICT_APPROVED}`, `{VERDICT_CHANGES}` and `{RULES}`
which expand to the repo's configured trigger, labels and rules
files); `verdict_approved` /
`verdict_changes` set the posted `## <label>` titles (defaults
`Verdict: APPROVED` / `Verdict: CHANGES_REQUESTED` — the reviewer
renders the canonical protocol word and the daemon maps it onto the
configured label, so `verdict_approved=LGTM` posts `## LGTM`).
**WHO** stays `pr_reviewer_agent`/`pr_reviewer_mode`/
`pr_reviewer_model`/`pr_reviewer_effort` — empty falls back to the
global `agent`/`model`/thought level (`agent_or`/`model_or`/
`effort_or`). **Per-repo overrides**:
`[backlog_orchestrator.review.<repo>]` in config.toml (or the
`review_overrides` JSON param) accepts `trigger_mode`,
`pr_review_trigger`, `review_prompt`, `verdict_approved`,
`verdict_changes`, `auto_review_untrusted`, `rules_files` — key = repo
path as it appears in
`pr_reviews`/the registry (`owner/name`, gerrit project), a short
name resolves too; each empty field inherits the global param —
mixed-forge workspaces can e.g. keep `comment` on GitHub and run
`manual` on a gerrit repo. ⚠️ **Merge policy**: custom verdict
labels must be recognized repo-side — the merge hook
(`block-merge.py`) only treats `Verdict:` titles from
`AGENTCONTROL_MERGE_VERDICTS` (default `approved,mergeable`) as
approvals, so a `## LGTM` approval needs
`AGENTCONTROL_MERGE_VERDICTS=…,lgtm` (or the default labels) or the
hook won't let the merge through.

**Agent profiles**: `devin` and `cursor` are built-in presets
(`default_agents()` in server/agents_cfg.rs — cursor's argv[0] is
resolved to an absolute path on the daemon PATH when possible:
`cursor-agent` then the `agent` alias). Every `[agents.*]` profile
(built-in or config.toml) is listed by `list_agents` —
`available:false` greys it in the new session dialog. The internal
component **`agents`** (options → composants, or
`acctl comp set agents <id> <argv-csv>`) edits each profile's ACP
command: one `list` param per profile, persisted as the FULL
`[agents.<id>]` section of config.toml (name/env/compact/catalog
included — a partial write would silently drop fields at next boot)
and hot-reloaded into `Daemon.agents` (tokio RwLock — reads via
`.read().await`). Ids are not creatable/removable this way — declare
new profiles in config.toml; **manual `[agents.*]` edits in
config.toml are themselves hot-reloaded** (`Daemon::reload_agent_profiles`,
stamp mtime+size — full merge built-ins + sections, so an added,
modified OR removed section converges to the reboot state; triggered
on `list_agents`/`daemon_status`/component views/`agent_profile`
lookups on spawn/attach AND periodically by `agents_rescan_boot`,
~60 s — `daemon_update` needs nothing extra: the replaced binary
restarts under its supervisor and re-parses at boot).

**Auto-discovered agent profiles** (`server/agents_auto.rs`, #261):
the daemon embeds a registry of known ACP adapters (claude →
`acp-claude` — the in-house Rust binary shipped in the tarball,
#280 — as the ONLY automatic path since #285; the npm
`claude-code-acp`/`claude-agent-acp` are no longer tried, though a
manual `[agents.claude]` may still point at them; codex → `codex-acp`
npm until `acp-codex` exists, gemini,
opencode) and re-syncs it on every `reload_agent_profiles` call —
an adapter installed after boot appears as a `via:"auto"` profile
without restart or config; a declared `[agents.*]` always wins (same
id → `via:"config"`). A profile whose tool is installed but whose
adapter is missing (e.g. `claude` CLI without `acp-claude`) is
still listed, `available:false` — `list_agents` then exposes
`unavailable:{searched, hint}` (install hint per known adapter,
generic PATH hint otherwise) shown as a tooltip in the new-session
dialog. `command` accepts ordered fallbacks:
`command = [["bin1"], ["bin2", "arg"]]` — `AgentProfile.commands` is
the candidate list, `misc::effective_command` picks the first whose
argv[0] resolves (used by spawn, `available`, caps probe, costs,
catalogue); `via` is `builtin`/`config`/`auto`.

**Adapter provisioning** (`server/agents_install.rs`, #277): each
registry spec also declares `install` — ordered `npm i -g <pkg>`
candidates for the third-party adapters (codex, gemini, opencode);
`claude` declares none since #285 (`acp-claude` ships with the
daemon — the Install action fails with the « update the daemon »
hint instead of `npm i -g`). The `install_agent_adapter` RPC (button « Install » of
the agent-capabilities dialog on `available:false` +
`unavailable.installable`, or `acctl agents install <id>`) runs them
daemon-side, user-triggered only: no shell, never sudo, 300 s timeout,
serialized, user-namespace only — when npm's global prefix is outside
HOME the command gets `--prefix ~/.npm-global` (already in
`agent_path_dirs`), with the same fallback on EACCES; an already
resolvable adapter is a `{already:true}` no-op and candidates are
tried in order until a `commands` binary resolves. Success triggers
`reload_agent_profiles` → `available:true` without restart.
`daemon_status.agent_install` advertises the capability.

**Agent profile per role** (`backlog_orchestrator` params, hot-settable
via `acctl comp set`, dropdown of the declared `[agents.*]` profiles in
the config UI): `worker_agent` (`Backlog-W*`), `planner_agent`
(`Backlog-P*` — planners AND implementers), `reviewer_agent`
(`Backlog-R*` plan reviews) and `pr_reviewer_agent` (`PRReview-*`).
Empty = the global `agent` param — a mono-profile config behaves
exactly as before. Modes and models stay per-profile vocabularies:
`*_mode`/`model` are passed as-is to the spawn — a value outside the
profile's announced list logs a warn and the spawn may be refused by
the agent (adjust the role's `*_mode`/profile accordingly; each agent
has its own mode names). The rate_limit serial mode stays global to
the component: a hold on ANY routed profile serializes the whole
pipeline (per-profile quotas aren't declared, so the gate can't be
per-agent yet).

**Failover provider** (`server/orch_failover.rs`): changing a role's
`*_agent` only affects the NEXT spawns — sessions already parked in
`rate_limit` backoff would retry forever on the saturated provider
and keep the serial gate closed. Each tick, a component session
(`Backlog-W*`/`Backlog-P*`/`Backlog-R*`/`PRReview-*`) in `error` +
`last_error.kind == "rate_limit"` whose `meta.agent` no longer
matches the role's effective profile is replaced on the current
profile: workers get their `wip` items `--reassign`ed plus their
open PRs re-attributed in the registry and a handoff prompt;
planners/implementers get a fresh `Backlog-P*` on the right phase
(draft `PLAN_FILE` recovered from the old journal); plan reviewers
are unclaimed (`--unclaim-review`) so the pipeline respawns them;
PR reviewers go back to `retry_wait` for an immediate relaunch.
Active sessions, `ac-wait` waiters and non-`rate_limit` holds
(`model`/`auth`/`other`) are never touched — the drift check makes
it self-healing across restarts. The failover is NOT quota-aware:
picking a profile that shares the same account/quota just reproduces
the rate limit on the new provider — choose a genuinely different
profile.

**Auto review**: when the work looks done **(PR still draft)**,
**note `TS=$(date -u +%FT%TZ)`**, comment `/mr review`
(`gh pr comment <N> --body "/mr review"`),
then **arm an ac-wait waiter** on the comments — never poll them
in a loop (see **Waiting**). The automation first posts an ack
(`Launching a review…`) and the review may come out under
**the user account OR the bot** → don't filter the
author: match the marker **posted after `$TS`** (`--paginate`
required — recent comments are on the last page;
`Review|Verdict` since the format varies: `## Review` / `## Verdict: …`):

```bash
TS=$(date -u +%FT%TZ)
gh pr comment <N> --body "/mr review"
ac-wait --name "verdict PR #<N>" --interval 60 --timeout 1800 -- bash -c "
  gh api --paginate 'repos/{owner}/{repo}/issues/<N>/comments?per_page=100' --jq '
    .[] | select(.body | test(\"^[*#]+ ?(Review|Verdict)\"))
      | select(.created_at > \"$TS\") | .body' | grep -q Verdict"
```

As soon as the ping arrives, read the comment and address it.
`TIMEOUT` (~30 min) → check whether the review was actually
launched (the `Launching a review…` ack can lag when the
orchestrator is busy) — re-arm the waiter if it's running, or
retrigger otherwise.
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

**Android APK signing**: release APKs are signed post-build by
`apksigner` with the CI upload keystore (secrets
`ANDROID_KEYSTORE_BASE64`, `ANDROID_KEYSTORE_PASSWORD`,
`ANDROID_KEY_ALIAS`, optional `ANDROID_KEY_PASSWORD`) — generated
once via `client/tools/android-upload-keystore.sh`. The generated
Gradle project has no `signingConfig`, so the release variant ships
unsigned and the workflow zipaligns + signs it; a verify step
compares the APK signer SHA-256 to the keystore cert (fail-closed).
Without `ANDROID_KEYSTORE_BASE64`, PR/main builds fall back to
`--debug` (ephemeral runner signature, warning only) but a `v*` tag
**fails** — a signed release must never be skipped. `versionCode`
is derived from semver (major*1e6 + minor*1e3 + patch) so it is
identical for `rc.N` and the final `X.Y.Z` of a cycle — Android
accepts equal versionCodes (only downgrades are refused) and the
code strictly grows cycle over cycle. ⚠️ One-time transition: the
first update after this change carries a different signature than
the old debug-keystore APKs → install over the top is refused once —
users must **uninstall then reinstall** (the device key and saved
connections live in app_data → re-pair/re-enter them).

**Release integrity (minisign, I1)**: every `v*` release also
publishes `SHA256SUMS` + `SHA256SUMS.minisig` — a minisign (Ed25519)
signature over the checksum manifest — plus GitHub build-provenance
attestations (`attest-build-provenance`, `id-token`+`attestations`
scoped to the release job). All three install paths verify signature
then sha256 before installing (`proto::release_verify`, key compiled
via `include_str!(packaging/agentcontrol.pub)`): app update, daemon
self-update + acp bootstrap, and remote install (`ensure_daemon`
option (a) — the app downloads+verifies the tarball itself then
pushes it through the ssh stdin; the server needs neither GitHub
access nor curl). Required one-time **user** setup:

```bash
minisign -G -p packaging/agentcontrol.pub -s <path OUTSIDE the repo>
gh secret set MINISIGN_KEY < <that private key>
gh secret set MINISIGN_PASSWORD      # only if the key is passphrase-protected
```

The private key NEVER enters the repo — CI writes it to `$RUNNER_TEMP`
only. Without `MINISIGN_KEY`, or with the placeholder still in
`packaging/agentcontrol.pub` (CI greps for it), a `v*` tag **fails**
— an unsigned release must never be published (same policy as APK
signing). The job also counter-verifies the signature it just
produced against the committed `.pub` — a secret key that doesn't
match the embedded pubkey would publish releases no client can
verify, so it breaks the release instead. Releases older than signed
publishing have no manifest: every client refuses them by default —
`AGENTCONTROL_ALLOW_UNSIGNED=1` (warn-logged) is the troubleshooting
override for pre-signature releases only, never for an invalid
signature or digest. **Key rotation**: generate a new pair, commit
the new `.pub` in the same change, update `MINISIGN_KEY` — but FIRST
ship a transition release still signed by the old key and carrying
the new `.pub` (binaries verify with the key they were built with;
without that step, already-installed clients can never verify a
new-key release again). If SECURITY.md lands with I9, the public
rotation procedure belongs there too.

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
CFBundleVersion require numeric) — the bundle version stays `X.Y.Z`
but the rc identity is **compiled into the binary**: `build.yml`
exports `AGENTCONTROL_RC=K` (extracted from the tag) → `build.rs`
emits `cargo:rustc-env` → the `app_version` command reports
`X.Y.Z-rc.K` to `getVersion()` (updater, ensure_daemon tag,
about/settings) — the minor daemon↔client gate stays safe,
`minorOf()` masks the suffix. Result: an rc client on any OS is
offered rc.k+1 then the final. Linux bundling is limited to `deb`+`appimage`
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
publishes a release on `v*` tags. `.github/workflows/test.yml`
runs the checks that don't need artifacts: `cargo fmt --all --check`,
`cargo test --workspace` and `cargo clippy -D warnings` (required
`rust` job), client tests that need no real daemon
(`js`), and dependency audits (`audit`).

## Automatic versioning (commit keywords)

PR → **`next_release`** (not `main` directly). CI checks
(`daemon-linux` ×2, `client` ×2, `client-android`, `client-macos` +
test.yml `rust`, `js`, `audit` — 6 required by the ruleset,
`client-macos`, `js` and `audit` to be included). CI builds
artifacts on PRs (APK and dmg suffixed `-<sha>`).

**No per-PR bump** — the version only moves at release. The level
is decided by **parenthesized keywords in commit subjects**
scanned over the cycle. Scan bound (`bump_version.py --bound`, shared
with `client/tests/bump-bound-test.sh`) = last `chore: bump version`
**whose `vX.Y.Z` tag exists** on `next_release`, **or the newest
`(reset` / `revert bump` commit even untagged** — a cancelled bump
must not let the range fall back to the previous tag and re-apply the
cancelled cycle's keywords (incident #435/#436: a premature ready
re-bumped 1.0.0 on #422's `(major)` despite the revert), otherwise
(post-reset: nr no longer has a bump commit, its history = `main`'s)
**last semver tag**, which is then an ancestor of nr —
self-maintained bound in all cases:

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
  don't edit it out of the message. ⚠️ The carrier is the **squash
  commit subject** on `next_release`: a single-commit PR squashes to
  the **commit's own message**, not the PR title — a keyword in the
  PR title alone never reaches the cycle (happened: #421). For
  multi-commit PRs the squash body carries the original subjects as
  `* ` bullets — `detect_level` scans **subjects and `* ` bullets of
  `(#N)` squash commits only**; free body text never counts (a body
  citing `'(major)'` re-bumped next_release to 1.0.0 — #441, revert
  #435). Keep the keyword in the squash **subject** to be safe.
- To **quote** a keyword in prose without triggering it (docs, a
  message talking about the rule), write it **without parentheses**
  ("sync") — free commit bodies aren't scanned anymore, but a `* `
  bullet inside a squash body still is, so keep the safe form there.
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
- **Daemon tests**: `cargo test --workspace --no-fail-fast` — the
  required `rust` CI job gates merges on it, along with
  `cargo fmt --all --check` and `cargo clippy --workspace
  --all-targets -- -D warnings` (blocking since the lint cleanup).
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
- `ureq` is shared (ensure_daemon pushes verified artefacts on
  Android too). `check_update`/`update_client` work on Android:
  `update_client` downloads the release APK into the app cache,
  verifies it like the other platforms (`SHA256SUMS` minisign +
  sha256, I1) then hands the path to the ac-device plugin, which
  launches the system installer via FileProvider + intent
  (`REQUEST_INSTALL_PACKAGES` is the app's own permission — the
  « unknown apps » settings screen is opened when missing and the
  command returns `permission_required`).
- CI APK: aarch64 signed by the CI release keystore when the
  `ANDROID_KEYSTORE_*` secrets exist, `--debug` fallback otherwise —
  `v*` tags fail without them (cf. § Releases → Android APK signing).
  `--target` to add other ABIs.

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
  `delete_len` clamped; `if_mtime_ms` ≠ disk mtime → `conflict:` error
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

## Agent skills (provisionnement auto, #264)

Les skills fournis par AgentControl (acctl, ac-wait, ac-steps…) vivent
dans **`skills/<nom>/`** à la racine du repo — source de vérité unique,
embarquée dans le binaire démon par `crates/daemon/build.rs`
(`.devin/skills/` est fusionné en compat). Au spawn de chaque session
(et une fois au boot), le démon les synchronise dans les répertoires
scannés par le provider (`crates/daemon/src/skills.rs`) :
`.devin/skills` (devin), `.claude/skills` (claude),
`.agents/skills` + `.cursor/skills` (cursor), `.agents/skills`
(codex et profils inconnus). Sync gérée par manifeste
`.agentcontrol-skills.json` : contenu réécrit quand l'embarqué change,
jamais d'écrasement d'un skill non managé (warn + skip). Désactivable :
`provision_skills = false` global ou `[agents.<id>] provision_skills =
false`. **Un nouveau skill livré avec le démon va dans `skills/`** — pas
dans `.devin/skills` (répertoire de compat pour les PR en vol, et
répertoire provisionné dans les workspaces — `build.rs` en exclut les
skills listés par son `.agentcontrol-skills.json` pour éviter qu'une
sortie provisionnée soit ré-embarquée). Les sorties provisionnées sont
gitignorées (`.gitignore` racine) ET, depuis #331, ajoutées au
`info/exclude` du dépôt de la session après chaque sync (commondir
partagé entre worktrees — `git status` reste propre dans les repos
utilisateurs sans toucher à leur `.gitignore` ; une racine déjà
trackée n'est jamais exclue).

## Known pitfalls

- `anyhow::bail!` in a `Result<_, String>` function → E0308.
- `ureq`: `tls` feature (rustls) everywhere — `native-tls` is only a
  custom connector via `AgentBuilder::tls_connector`, never a default
  backend → without `tls`, `ureq::get/post` on https fail with
  "no TLS backend is configured". ring (C) is already required via
  russh — the Windows check passes via the zig shims
  `/tmp/zig-shim/x86_64-w64-mingw32-gcc{,++}` (`zig cc -target
  x86_64-windows-gnu`, filter cc-rs's `--target=`).
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
- Claude: `acp-claude` (in-house, #280, sole automatic adapter since
  #285) spawns the real `claude` CLI
  (`-p --input-format stream-json`) → the CLI must be installed
  (unlike `claude-agent-acp`, which bundles the Agent SDK). Auth:
  ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN (`claude setup-token`,
  headless OK).
- Minimal systemd PATH: `ensure_agent_path` (main.rs) injects the
  binary's dir + the usual agent dirs (`~/.local/bin`,
  `~/.npm-global/bin`, `~/.local/share/mise/shims`, `~/.cargo/bin`,
  nvm `$NVM_BIN`/latest `~/.nvm/versions/node/*/bin`,
  `/usr/local/bin`, `/opt/homebrew/bin`) — the same list
  `command_path` (server/misc.rs) searches on every call, so
  `available` reflects post-boot installs without a daemon restart
  and agent spawn re-resolves argv[0] to the same binary.
- **Interrupted drag → stuck "grabbing" cursor** (WebKitGTK/Wayland,
  toolkit bug with no web-side remedy): NEVER detach an element that
  can be a drag source (list rebuild, `.remove()`) while a drag is in
  flight — cf. `dragging` counter + deferred rebuild in `sidebar.ts`.
  `startDragging()` only on `e.detail===1` (a 2nd mousedown during
  toggleMaximize kills the WM grab the same way). `<a>`/`<img>`
  disarmed via `-webkit-user-drag:none` (base.css); keep `draggable`
  only on real DnD sources.
