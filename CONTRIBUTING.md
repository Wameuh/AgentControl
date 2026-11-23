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
- If you touched `client/`, run `cd client && npm run build` (tsc + vite);
  for `client/src-tauri/` also `cd client && npm run check`
  (tsc + `cargo check`).
- Target the `next_release` branch — PRs are squash-merged there and
  `main` is only fed by releases.
- Version bumps are driven by parenthesized keywords in commit subjects
  (`(sync)`, `(major)` — see "Automatic versioning" in `AGENTS.md`), not
  by hand-editing version files.
- A user-visible `feat`/`fix`/`perf` PR adds one line to
  `client/tools/release_notes_en.txt`: `<PR number>: <english note>`
  (`<PR>: [skip] <reason>` for internal changes).

## More docs

`AGENTS.md` is the short contributor guide for coding agents;
[docs/maintainer.md](docs/maintainer.md) holds the maintainer-only
workflow (releases, deployment, orchestration internals) — handy if
you're digging deeper, not needed for a regular PR.

## Security

Found a vulnerability? Please don't open a public issue — see
[SECURITY.md](SECURITY.md).
