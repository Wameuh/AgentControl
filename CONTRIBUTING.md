# Contributing

Thanks for your interest in contributing! A few things to know before
opening a pull request.

## Build from source

```bash
cargo build --release                          # daemon + CLI
cd client && npm install && npx tauri build    # desktop app
```

## Before opening a PR

- Run `cargo test --workspace` — the `rust` CI job gates merges on it.
- If you touched `client/`, run `cd client && npm run build` (tsc + vite).
- Target the `next_release` branch — PRs are squash-merged there and
  `main` is only fed by releases.
- Version bumps are driven by parenthesized keywords in commit subjects
  (`(sync)`, `(major)` — see "Automatic versioning" in `AGENTS.md`), not
  by hand-editing version files.
- A user-visible `feat`/`fix`/`perf` PR adds one line to
  `client/tools/release_notes_en.txt`: `<PR number>: <english note>`
  (`<PR>: [skip] <reason>` for internal changes).

## Security

Found a vulnerability? Please don't open a public issue — see
[SECURITY.md](SECURITY.md).
