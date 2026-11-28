# CI/release pipeline example — this repository's own workflows

> **Optional example.** This document describes the GitHub Actions
> setup of *this* repository (`.github/workflows/`) for anyone who
> wants to reproduce a similar release pipeline on their own project.
> **AgentControl itself requires none of it** — the daemon, the client
> and the component system work without any CI; the orchestrator's PR
> pipeline only needs a forge + a review trigger
> (`packaging/INSTALL.md` §6).

---

## Branch model

Two protected branches (no direct push, PR required, ruleset-enforced):

- **`next_release`** — integration branch. Every feature/fix PR targets
  it (`gh pr create --base next_release --draft`) and accumulates there.
- **`main`** — release branch, only fed by merging `next_release` when a
  release is cut.

## Workflows

| Workflow | Trigger | Role |
| --- | --- | --- |
| `build.yml` | PRs (non-draft), pushes, `v*` tags, dispatch | Builds all artifacts: daemon x86_64 + aarch64, Tauri client (Ubuntu/Windows/macOS arm64 dmg), Android APK. On `v*` tags publishes the GitHub release; on PRs produces check runs (`daemon-linux` ×2, `client` ×2, `client-macos`, `client-android` — 5 required by the ruleset). |
| `version.yml` | release PR ready / synchronize | `release-bump`: computes `last tag + level` (commit-keyword scan, `client/tools/bump_version.py::detect_level`), pushes `chore: bump version` to `next_release`, pushes a `v<NEXT>-rc.<k>` prerelease tag per ready head. Also `rc-cleanup` on release-PR close. |
| `auto-release-pr.yml` | push to `next_release` | Opens/updates the `next_release` → `main` draft release PR (via `RELEASE_PAT`, fallback `GITHUB_TOKEN`). |
| `release-on-merge.yml` | release PR merged | Tags `v<version>` (from `client/package.json`), dispatches `build.yml` on the tag, cleans the cycle's `-rc.*` releases/tags. |
| `reconcile-next-release.yml` | push to `main` | Re-links `next_release` to `main` after each squash: identical trees → `--force-with-lease` reset; divergent → recursive merge (fail-closed on conflict). |

## Versioning convention

No per-PR bump — the version only moves at release. The bump level is
decided by **parenthesized keywords in commit subjects** scanned over
the cycle: `(major)` → major, `(sync)` → minor (client↔daemon protocol
breakage — the only minor trigger; it drives auto-update via `minorOf`
comparison), nothing → patch. Source of truth:
`client/tools/bump_version.py` (`detect_level`).

Release notes: `client/tools/release_notes.py` categorizes squash
subjects with English labels from `client/tools/release_notes_en.txt` —
each `feat`/`fix`/`perf` PR adds a `<N>: <english user note>` line.

## Secrets

| Secret | Used by | Why |
| --- | --- | --- |
| `RELEASE_PAT` | auto-release-pr, release-bump, reconcile | Fine-grained PAT — a `GITHUB_TOKEN` push wouldn't trigger checks on the pushed head, and repo settings forbid `createPullRequest` to it. Scopes: Contents RW, Workflows RW, Pull requests RW; the ruleset's bypass actor is `Repository admin`. |
| `ANDROID_KEYSTORE_BASE64` / `ANDROID_KEYSTORE_PASSWORD` / `ANDROID_KEY_ALIAS` (+ optional `ANDROID_KEY_PASSWORD`) | build.yml | Release APK signing (`apksigner` post-build; keystore generated once via `client/tools/android-upload-keystore.sh`). Absent → `--debug` fallback on PR/main builds, hard fail on `v*` tags. |

## What a minimal reproduction needs

To reuse this pipeline on another repo: copy `.github/workflows/`,
`client/tools/bump_version.py` + `release_notes*.py/txt`, create the
`RELEASE_PAT` secret, configure two protected branches + a ruleset with
a bypass actor, and rename the check-run names everywhere they are
gated on (the `daemon-linux`/`client*` job names are this project's).
Everything else — branches, keywords, rc tags — is convention, freely
adaptable.
