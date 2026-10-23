# AgentControl — règles de travail

## Branche `main` protégée → workflow PR

`main` est protégée : **push direct impossible**, PR obligatoire.
Checks CI requis avant merge : `daemon-linux` (x86_64 + aarch64),
`client` (Ubuntu + Windows), `client-android`.

Workflow : créer une branche → push → `gh pr create` → attendre les
5 checks verts → merge (squash ou merge commit, pas de contrainte).
Le CI build tous les artefacts sur les PR (APK suffixé `-<sha>`).

## Releases

**Pas de tag `v*` ni de release sans demande explicite de l'utilisateur.**
Pousser les commits et bumper la version, oui — tagguer/publier, non.

Exception : le label **`release`** sur une PR = demande de release →
au merge, `release-on-merge.yml` tague `v<version>` (lu depuis
`client/package.json`) et dispatche `build.yml` sur le tag (le push de
tag via GITHUB_TOKEN ne déclenche pas les workflows d'où le dispatch).
Idempotent : tag existant → skip (version non bumpée = rien à releaser).

## Après chaque push : vérifier le CI

Obligatoire, ne pas oublier :

```bash
gh run list --limit 3 --json databaseId,conclusion,name
gh run view <id> --log-failed   # si failure
```

Le workflow `.github/workflows/build.yml` build : daemon x86_64+aarch64,
client Tauri Ubuntu+Windows, et publie une release sur les tags `v*`.

## Version automatique (labels de PR)

`main` est protégée : **push direct impossible, PR obligatoire** —
5 checks requis (`daemon-linux` ×2, `client` ×2, `client-android`).
Le CI build les artefacts sur les PR (APK suffixé `-<sha>`).

Le workflow `version.yml` bump la version selon les labels de la PR :
- `daemon` et/ou `client` : composants touchés → bump **patch**
- `sync` : daemon+client à déployer ensemble → bump **minor**
- `patch` / `minor` / `major` : niveau explicite (prioritaire)
- `release` : pas de bump — au merge, tag + release (voir plus haut)
- aucun label composante → pas de bump (docs, CI…)
- `minor` = incompatibilité client↔démon (déclenche l'auto-update)

Le commit `chore: bump version` est poussé sur la branche automatiquement ;
ne pas bumper à la main en parallèle (le job skip si déjà bumpé vs base).
Manuel hors-PR : `python3 client/tools/bump_version.py [major|minor|patch|x.y.z]`.

## Avant chaque push (branche de PR)

- **Frontend** : `cd client && npm run build` (tsc + vite).
- **Rust client (src-tauri)** : pas de webkit sur le Pi → typecheck via
  la cible Windows :
  `cd client/src-tauri && PATH="/tmp/zig-shim:$PATH" cargo check --target x86_64-pc-windows-gnu`
  (stub `windres` dans /tmp/zig-shim nécessaire).
- **Daemon** : `cargo build --release -p agentcontrol-daemon`, puis
  redéployer : `systemctl --user stop agentcontrol-daemon && cp target/release/agentcontrol-daemon ~/.local/bin/ && systemctl --user start agentcontrol-daemon`.
  ⚠️ Le restart tue les agents en cours (sessions → interrupted).
- **Suppression d'un crate/binaire** : penser au workflow CI
  (`-p <crate>` dans build.yml) et à `CRATES` dans `tools/bump_version.py`.

## Android

- `gen/android` NON commité — généré par `npx tauri android init --ci` dans le
  job `client-android` (le manifest généré inclut déjà `INTERNET`).
- SSH : pas de binaire ssh → transport `Transport::SshNative` (russh) forcé
  sur Android, disponible en opt-in sur desktop (`native: true` dans la cfg).
  `direct-tcpip` remplace `ssh -L` ; `ssh_native::ssh_exec` remplace `ssh host`.
- Clé ed25519 générée par l'app : `<base>/agentcontrol_ed25519` — sur Android
  `client_core::init_paths(app_data_dir)` car HOME est absent.
- `ureq`, `tauri-plugin-shell` : desktop-only (target cfg dans src-tauri/
  Cargo.toml) ; `check_update`/`update_client` renvoient Err sur Android.
- APK CI : aarch64 debug-signé (artifact + release). `--target` pour ajouter
  d'autres ABI.

## Pièges rencontrés

- `anyhow::bail!` dans une fonction `Result<_, String>` → E0308.
- `ureq` : feature `native-tls` (pas rustls/ring → pas de compilateur C
  requis pour le check Windows) ; `libssl-dev`/`pkg-config` nécessaires
  sur Ubuntu (CI). NB : russh **exige** ring (C) — le check Windows passe
  via les shims zig `/tmp/zig-shim/x86_64-w64-mingw32-gcc{,++}`
  (`zig cc -target x86_64-windows-gnu`, filtrent `--target=` de cc-rs).
- `flex-shrink:0` requis sur les items d'un flex-column scrollable.
- Tauri 2 : `set_var` est `unsafe` ; `get_webview_window` exige
  `use tauri::Manager` ; `Image::new_owned` prend du RGBA brut.
