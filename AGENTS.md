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
(≥30 s après le dernier push) → attendre les 5 checks verts → merge.
**Merge** : vers `main`, toujours l'utilisateur. Vers `next_release`,
**l'agent peut merger** via `gh pr merge <N>` quand la dernière review
auto porte un verdict **`Verdict: APPROVED` en titre** postérieur au
push du head courant (fraîcheur vérifiée via les PushEvents du repo,
fallback feed `activity` — les events peuvent lagger des heures) —
sinon le hook `.devin/` bloque (`git merge`, API, GraphQL, `--admin`,
`main` : toujours interdits). Tripwire : un commentaire `Verdict:
APPROVED` auto-posté passerait — la règle reste de ne merger que sur
review réelle. PR déjà mergée ?
`git checkout next_release && git pull` pour resynchroniser.
Le CI build tous les artefacts sur les PR (APK suffixé `-<sha>`).

**Worktrees** : travailler dans un **git worktree séparé par tâche**
(`git worktree add ../AgentControl-<feat> -b <br> next_release`) — le
checkout principal reste propre et plusieurs tâches peuvent avancer
en parallèle sans se marcher dessus. Nettoyer à la fin :
`git worktree remove ../AgentControl-<feat>`.

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

**Rebase avant review** : si `origin/next_release` a avancé depuis la
branche — `git merge-base --is-ancestor origin/next_release HEAD` faux
**après `git fetch`**, ou `gh pr view <N> --json mergeable` →
`CONFLICTING` (`UNKNOWN` = calcul async GitHub → retry, pas « ok ») —
rebase/merge **avant** de lancer la review. Le verdict est ancré au SHA
du head : tout push ultérieur l'invalide → le verdict est perdu et un
round de plus est obligatoire (arrivé : #35 approved sur une base
stale → rebase → re-review). Perdu ≠ ignoré : si la review a déjà
posté des constats, ils restent valables — **traiter les retours en
même temps que le rebase** (fix + rebase dans le même push), puis
relancer la review. Une base à jour évite aussi de faire
reviewer un diff que le merge réécrira. Si `next_release` avance entre
l'APPROVED et le merge, le verdict reste valide mais GitHub peut
refuser le merge sur conflit → rebase + re-review.

**Review auto** : quand le travail semble fini **(PR encore draft)**,
**noter `TS=$(date -u +%FT%TZ)`**, commenter `/mr review`
(`gh pr comment <N> --body "/mr review"`),
puis **poller les commentaires** jusqu'à la review — l'automation
poste d'abord un ack (`Launching a review…`) et la review peut
sortir sous **le compte user OU le bot** → ne pas filtrer l'auteur :
matcher le marqueur **postérieur à `$TS`** (`--paginate`
requis — les commentaires récents sont sur la dernière page ;
`Review|Verdict` car le format varie : `## Review` / `## Verdict: …`) :
`gh api --paginate 'repos/{owner}/{repo}/issues/<N>/comments?per_page=100' --jq '.[] | select(.body | test("^[*#]+ ?(Review|Verdict)")) | select(.created_at > "'"$TS"'") | .body'`.
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

La **release PR est auto-créée** par `auto-release-pr.yml` : à chaque
push sur `next_release`, une PR draft `next_release` → `main` est
ouverte si absente — pour releaser il suffit de la passer **ready**
(le passage non-draft est le déclencheur, pas de label requis).
La création passe par
**`RELEASE_PAT`** (fallback `GITHUB_TOKEN`) : le réglage repo
« Allow GitHub Actions to create and approve pull requests » interdit
`createPullRequest` au `GITHUB_TOKEN` ; sans `Pull requests: RW` sur le
PAT le job échoue rouge (arrivé : run 40210287826, tentatives avant
l'ajout du scope).

Release = PR `next_release` → `main` passée **ready** →
le job `release-bump` de `version.yml` calcule `dernier tag + niveau`,
commit `chore: bump version` poussé sur `next_release` (la PR se met
à jour, les 5 checks tournent sur le head bumpé) → au merge (par
l'utilisateur), `release-on-merge.yml` tague `v<version>` (lu depuis
`client/package.json`) et dispatche `build.yml` sur le tag (le push
de tag via GITHUB_TOKEN ne déclenche pas les workflows d'où le
dispatch). Idempotent : tag existant → skip, package.json déjà à
NEXT → pas de re-bump. Tant que la PR reste draft, rien ne bump.

**Release candidate** : quand la release PR passe ready, `release-bump`
pousse aussi un tag `v<NEXT>-rc.<k>` (k incrémental, un rc par état de
head — un synchronize post-ready produit rc.k+1) → le push du tag via
PAT déclenche `build.yml` qui publie une **prerelease GitHub** (jamais
marquée `latest`). Le build rc **injecte le suffixe `-rc.k` dans les
fichiers de version** (non commité — le tag pointe le commit propre
X.Y.Z) → le binaire s'identifie `X.Y.Z-rc.k`, et la finale `X.Y.Z` est
proposée en update ensuite (semver : rc < stable). Windows exclu de
l'injection (MSI/NSIS exigent du numérique) — conséquence : un artefact
rc Windows s'identifie `X.Y.Z` → pas de chemin auto-update vers rc.k+1
ni vers la finale, réinstall manuelle. Pour installer la rc depuis
l'app : canal rc opt-in dans les paramètres → la sonde passe de
`releases/latest` (qui exclut les prereleases) à la liste `/releases`
+ max semver. Au merge, `release-on-merge` nettoie les releases+tags
`v<NEXT>-rc.*` du cycle — et une release PR fermée sans merge déclenche
le même cleanup (`rc-cleanup` de version.yml).

**Reconcile** : tout merge nr→main est un squash → le commit `main`
n'est jamais ancêtre des commits nr : sans reconcile la release
suivante reconflicterait sur toute zone retouchée (arrivé : #23, #29),
et la release PR listerait TOUT l'historique nr (#39 : 25 commits
remontant à #11, injectés par GitHub dans le message de squash —
réglage `COMMIT_MESSAGES`). Le job `reconcile-next-release` (push sur
`main`) refait le lien, en deux modes : **arbres identiques** (cas
nominal post-release — nr n'a plus de contenu hors `main`) →
**reset** `next_release` = tip `main` en `--force-with-lease` : les
commits du cycle écoulé quittent nr → la prochaine release PR ne
liste que le cycle courant, merge-base = tip `main` ; **arbres
différents** (feature mergée dans la fenêtre du reconcile,
résolutions de conflit côté `main`, hotfix direct en cours de cycle)
→ **merge** classique (vrai merge récursif, hotfix propagé, conflit =
job rouge fail-closed, jamais de reset destructif).
⚠️ Après un reset, nr est réécrite : clones locaux →
`git fetch && git reset --hard origin/next_release` (pas `pull`) ;
PR ouverte basée sur l'ancien nr → `git fetch && git rebase --onto
origin/next_release <ancien-tip>` — sinon sa liste de commits
affiche l'ancien historique nr (le diff reste correct, mais les
vieux sujets `(minor)`/`(sync)` ré-entreraient dans le body du
squash → sur-bump possible au cycle suivant).
⚠️ `release-bump` et `reconcile-next-release` poussent sur
`next_release` via le secret **`RELEASE_PAT`**, qu'`auto-release-pr`
utilise aussi pour créer la release PR. PAT fine-grained du repo,
scopes requis : **Contents RW** (les deux pushes), **Workflows RW**
(reconcile : le merge importe des squashs `main` touchant
`.github/workflows`, un PAT sans ce scope est refusé indépendamment du
bypass ruleset), **Pull requests RW** (auto-release-pr :
`createPullRequest`) :
`github-actions[bot]` ne peut pas être bypass actor d'un ruleset
(app first-party → 422), et un push `GITHUB_TOKEN` ne déclencherait
pas les checks sur le head poussé (anti-récursion). Le ruleset
`next_release` a le rôle **`Repository admin`** en bypass actor
(Settings → Rules → Rulesets). Sans `RELEASE_PAT`, un push réellement
nécessaire échoue fail-fast (« secret RELEASE_PAT manquant ») — rien à
pousser → skip clean, pas d'échec ; les fine-grained PAT expirent →
renouveler le secret à l'échéance (sinon échec opaque au checkout).

## Après chaque push : vérifier le CI

Obligatoire, ne pas oublier :

```bash
gh run list --limit 3 --json databaseId,conclusion,name
gh run view <id> --log-failed   # si failure
```

Le workflow `.github/workflows/build.yml` build : daemon x86_64+aarch64,
client Tauri Ubuntu+Windows, et publie une release sur les tags `v*`.

## Version automatique (mots-clés de commit)

PR → **`next_release`** (pas `main` directement). 5 checks requis
(`daemon-linux` ×2, `client` ×2, `client-android`). Le CI build les
artefacts sur les PR (APK suffixé `-<sha>`).

**Pas de bump par PR** — la version ne bouge qu'à la release. Le niveau
est décidé par des **mots-clés parenthésés dans les sujets de commit**
balayés sur le cycle. Borne du scan = dernier `chore: bump version`
**dont le tag `vX.Y.Z` existe** sur `next_release`, sinon (post-reset :
nr n'a plus de commit bump, l'historique = celui de `main`) **dernier
tag semver**, qui est alors ancêtre de nr — borne auto-entretenue dans
les deux cas :

- `(major)` → **major** ; `(minor)` ou `(sync)` → **minor** ;
  rien → **patch**
- `(sync)`/`(minor)` = incompatibilité client↔démon (déclenche
  l'auto-update) — le plus fort rencontré gagne.
- Écrire le mot-clé **parenthésé** dans le sujet du commit qui le
  motive : `feat: multi-agents (minor)` — un mot nu (`a major bug`)
  ne doit pas compter. **Au squash, l'élément doit être repris** —
  GitHub inclut par défaut les sujets dans le body du squash : ne pas
  l'éditer hors du message.
- La release numérote depuis le **dernier tag semver**, pas depuis
  `package.json` — la version fichier peut dériver entre releases,
  elle est recalée au bump de release.

Manuel hors-PR : `python3 client/tools/bump_version.py [major|minor|patch|x.y.z]`.

## Avant chaque push (branche de PR)

- **Frontend** : `cd client && npm run build` (tsc + vite).
- **Rust client (src-tauri)** : pas de webkit sur le Pi → typecheck via
  la cible Windows :
  `cd client/src-tauri && PATH="/tmp/zig-shim:$PATH" cargo check --target x86_64-pc-windows-gnu`
  (stub `windres` dans /tmp/zig-shim nécessaire).
- **Daemon** : `cargo build --release -p agentcontrol-daemon` (build
  seulement). **Interdit de toucher au daemon en direct** : pas de
  restart/stop/redéploiement par un agent (`systemctl`, `supervisorctl`,
  kill, remplacement du binaire) — `agentcontrol-daemon` orchestre les
  sessions agents (dont la nôtre) : le redémarrer tue tous les agents
  en cours (sessions → interrupted). Le redéploiement est une action
  de l'utilisateur uniquement.
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

## Conventions de code

- **Qualité et évolutivité d'abord** : tout développement se fait avec
  une idée de qualité de code et d'évolution facile — privilégier des
  abstractions claires, un découplage propre et du code lisible plutôt
  qu'une solution rapide qui deviendra difficile à modifier.
- **Fichiers ≤ 1000 lignes** : éviter les fichiers dépassant 1000
  lignes ; si un fichier approche ou dépasse cette taille, voir pour
  le découper en modules/composants cohérents.
- **Tests** : pas de tests unitaires nécessaires — privilégier les
  tests d'intégration qui valident les flux réels (daemon ↔ client,
  appels ACP, SSH, …).

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
