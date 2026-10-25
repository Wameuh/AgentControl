# AgentControl — règles de travail

## Workflow : `next_release` → `main`

Deux branches protégées (**push direct impossible**, PR obligatoire,
5 checks CI requis : `daemon-linux` ×2, `client` ×2, `client-android`) :

- **`next_release`** = branche d'intégration. **Toutes les PR de
  feature/fix visent `next_release`** (`gh pr create --base next_release`)
  et s'y accumulent.
- **`main`** = branche de release. Uniquement alimentée par le merge de
  `next_release` quand l'utilisateur décide de releaser.

Workflow : créer une branche depuis `next_release` → push →
`gh pr create --base next_release --draft` → finir le travail →
`/mr review` **sur la draft** → traiter la review (fix → push →
relancer, toujours en draft) → review soldée → `gh pr ready`
(≥30 s après le dernier push) → attendre les 5 checks verts →
**l'utilisateur merge** (jamais l'agent : ni `gh pr merge`, ni
`git merge`, ni API — un hook `.devin/` le bloque, et c'est interdit
même sans le hook). PR déjà mergée par l'utilisateur ?
`git checkout next_release && git pull` pour resynchroniser.
Le CI build tous les artefacts sur les PR (APK suffixé `-<sha>`).

**Drafts** : une PR reste en **draft tout le cycle travail + review**
— les pipelines ne se lancent qu'**après** la review (`gh pr ready` =
feu vert final : event `ready_for_review` → les 5 checks). **La
review se demande sur la draft** : `/mr review` lance la session même
sans ready (le trigger ne gate pas `isDraft`) → itérer (corriger →
push → relancer `/mr review`) autant de rounds que nécessaire en
restant draft → **un seul run CI complet par PR**, pas un par round.
Tout changement sur une PR non-draft → repasser en draft **avant**
d'y toucher : `gh pr ready --undo`, puis `gh pr ready` quand c'est
fini. **Le CI ne tourne pas sur les drafts** (tous les jobs sont
skippés). **Laisser ~30 s entre le dernier push et `gh pr ready`** —
sinon le dispatch `synchronize` draft (tout skip) peut se
matérialiser après le run ready et l'annuler (même groupe de
concurrency) → head sans CI (arrivé en live, merge bloqué).
⚠️ `gh pr checks <N> --watch` sort vert sur des runs skippés ou
cancelled → gater sur les **check runs du head**, pas sur les
métadonnées de runs (`workflow_run.event` ne rapporte jamais
`ready_for_review`, et `createdAt` est une course) :

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

À lancer depuis la racine du clone (`{owner}/{repo}` = repo du cwd) ;
le filtre de noms doit suivre les jobs required de `build.yml`. Sans
la branche d'échec, un head tout-skippé ressortirait vert après ~5
min — le cas exact que le gate détecte.

**Review auto** : quand le travail semble fini **(PR encore draft)**,
**noter `TS=$(date -u +%FT%TZ)`**, commenter `/mr review`
(`gh pr comment <N> --body "/mr review"`),
puis **poller les commentaires** jusqu'à la review — l'automation
poste d'abord un ack (`Launching a review…`) et la review peut
sortir sous **le compte user OU le bot** → ne pas filtrer l'auteur :
matcher le marqueur `Review` **postérieur à `$TS`** (`--paginate`
requis — les commentaires récents sont sur la dernière page) :
`gh api --paginate 'repos/{owner}/{repo}/issues/<N>/comments?per_page=100' --jq '.[] | select(.body | test("^[*#]+ ?Review")) | select(.created_at > "'"$TS"'") | .body'`.
Dès qu'elle est postée, l'addresser, pas d'attente fixe
(abandon ~10 min si rien).
Des changements à faire → corriger et pousser **en restant draft**,
puis relancer `/mr review` ; `gh pr ready` seulement quand la review
est soldée (voir **Drafts**). NB : une review
lancée juste après un push peut lire un diff périmé → ancrer la
lecture sur `headRefOid` — mais `headRefOid`/`isDraft` peuvent
eux-mêmes lagger ~1 min post-push → recouper avec le tip de branche
(`gh api repos/{owner}/{repo}/branches/<br>`) ou attendre ~60 s.
La commande se poste **seule** dans son commentaire : le trigger
matche `contains` sur le body → toute citation de `/mr review` dans
un autre commentaire relance une session (pour la citer :
`/mr&#32;review`).

## Releases

**Pas de tag `v*` ni de release sans demande explicite de l'utilisateur.**
Pousser les commits et bumper la version, oui — tagguer/publier, non.

Release = PR `next_release` → `main` avec le label **`release`** →
au merge (par l'utilisateur), `release-on-merge.yml` tague `v<version>`
(lu depuis `client/package.json`) et dispatche `build.yml` sur le tag
(le push de tag via GITHUB_TOKEN ne déclenche pas les workflows d'où le
dispatch). Idempotent : tag existant → skip. Sans label `release`, un
merge dans `main` ne tague rien.

## Après chaque push : vérifier le CI

Obligatoire, ne pas oublier :

```bash
gh run list --limit 3 --json databaseId,conclusion,name
gh run view <id> --log-failed   # si failure
```

Le workflow `.github/workflows/build.yml` build : daemon x86_64+aarch64,
client Tauri Ubuntu+Windows, et publie une release sur les tags `v*`.

## Version automatique (labels de PR)

PR → **`next_release`** (pas `main` directement). 5 checks requis
(`daemon-linux` ×2, `client` ×2, `client-android`). Le CI build les
artefacts sur les PR (APK suffixé `-<sha>`).

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
  (`-p <crate>` dans build.yml) et à `CRATES` dans `tools/bump_version.py`
  (le bump de la PR exécute le script de la **base** → éditer `CRATES`
  n'aide que les bumps post-merge ; crate retiré = job rouge
  fail-closed, non bloquant).

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
- `cursor-agent acp` : sous-commande **cachée** (absente de --help),
  aucun argument — pas de placeholder {mode}/{model} possible, le mode/
  modèle demandé à la création est appliqué via set_config_option post-spawn.
- Codex : pas d'ACP natif → adaptateur `@agentclientprotocol/codex-acp`
  (Node ; zed-industries/codex-acp archivé). `CODEX_PATH` **absolu** exigé
  vers le binaire codex standalone : sinon l'adaptateur prend son
  `@openai/codex` npm embarqué (optional dep manquante sur arm64), et via
  npx `codex` dans PATH est le wrapper npm, pas le vrai CLI.
- Claude : `@agentclientprotocol/claude-agent-acp` embarque le Claude
  Agent SDK → CLI `claude` inutile. Auth : ANTHROPIC_API_KEY ou
  CLAUDE_CODE_OAUTH_TOKEN (`claude setup-token`, headless OK).
- PATH systemd minimal : `ensure_agent_path` (main.rs) injecte dir du
  binaire + ~/.local/bin + nvm (`$NVM_BIN`, sinon dernier
  `~/.nvm/versions/node/*/bin`) — requis pour les agents npm-globaux
  (shebang `#!/usr/bin/env node`).
