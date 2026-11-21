# Audit de découplage environnement — AgentControl

**Item** : #308 — audit de tout ce qui lie AgentControl à l'environnement
de l'auteur (repo `Wameuh/AgentControl`, workflow `next_release` → `main`,
chemins `/workspace`, outils perso `agentbacklog`/`ac-wait`/`gh`).

**Objectif produit** : qu'un tiers installe le démon + le client sur
n'importe quel repo et que l'orchestrateur, les reviews PR, les updates
et le catalogue fonctionnent — ou se désactivent proprement — sans
supposer notre infra.

Méthode : grep de `github.com`, `Wameuh`, `/workspace`, `next_release`,
`agentbacklog`, `gh `, `devcontainer`, `supervisord` sur `crates/`,
`client/`, `packaging/`, `docs/`, `skills/`, `plugins/`, `.devin/`,
`.github/`. Chaque occurrence est classée ci-dessous ou justifiée.

## Légende

- **Gravité** :
  - 🔴 **bloquant** — la feature est inutilisable ou casse pour un
    utilisateur tiers (URL en dur, chemin supposé, binaire requis
    absent).
  - 🟠 **dégradé** — fonctionne mais produit des résultats faux ou
    silencieusement limités (mauvais repo, mauvaise branche, prompts
    qui décrivent un workflow qui n'existe pas).
  - ⚪ **cosmétique** — doc, exemple, libellé, fixture de test.
- **Découplage proposé** : `param` (nouvelle clé config.toml /
  param composant), `détection` (auto depuis git remote / env /
  filesystem), `abstraction` (trait/couche forge ou outil), `doc`
  (documenter comme prérequis ou optionnel), `feature-flag`.

---

## A. Identité du repo GitHub — l'update est GitHub-only et repo-en-dur

| # | Couplage | Localisation | Gravité | Découplage proposé |
|---|----------|--------------|---------|--------------------|
| A1 | Self-update du démon : `const REPO = "Wameuh/AgentControl"` — URL release + API releases en dur | `crates/daemon/src/selfupdate.rs:14,226,304` | 🔴 | `param` — `[daemon] update_repo = "owner/name"` (ou détection du remote git du checkout d'origine) ; abstraction `ReleaseSource { GitHubReleases, UrlTemplate, None }` pour anticiper GitLab. |
| A2 | Check/update client Tauri : 4 URLs `api.github.com/repos/Wameuh/AgentControl/releases…` en dur | `client/src-tauri/src/commands/update.rs:89,124,131,331-333` ; `client/src/features/update.ts:32,55,75,88,151` (doublon TS) | 🔴 | `param` — champ `release_repo` dans la config du client (ou réponse `daemon_status`/`daemon_update` pour que le client suive le repo du démon) ; un seul endroit de vérité côté démon. |
| A3 | `connect.rs` : `const BASE = "https://github.com/Wameuh/AgentControl/releases"` pour le script d'install distant | `client/src-tauri/src/commands/connect.rs:501` | 🔴 | `param`/`détection` — même source que A2 ; à défaut, documenter que le bootstrap install ne marche que pour ce repo. |
| A4 | Lien « repo » dans le client (à propos / shell open) | `client/src/main.ts:503` | ⚪ | `param` — même config que A2. |
| A5 | CSP `connect-src … https://api.github.com https://raw.githubusercontent.com https://objects.githubusercontent.com` + CSP web démon `web/mod.rs:66` | `client/src-tauri/tauri.conf.json:25` ; `crates/daemon/src/web/mod.rs:66` | 🟠 | `param`/`feature-flag` — n'élargir la CSP que si `update_repo` est une forge GitHub ; sinon CSP minimale. Aujourd'hui la CSP GitHub est justifiée *uniquement* par A1/A2. |

## B. Workflow git/forge — `next_release` → `main` et `gh` supposés universels

C'est le couplage le plus diffus : le **registre PR** (`forges.rs`,
`forgesync`, `prs.rs`, `[[forges]]` config) est déjà multi-forge
(GitHub implicite + GitLab + Gerrit déclarés), mais **tout le
pipeline au-dessus** (orchestrateur, review PR, prompts workers)
parle `gh`, `pull request`, base `next_release`.

| # | Couplage | Localisation | Gravité | Découplage proposé |
|---|----------|--------------|---------|--------------------|
| B1 | `const ALLOWED_BASE: &str = "next_release"` — toute review PR est refusée sur une autre base ; le `gh pr list --base next_release` du scanner est en dur | `crates/daemon/src/server/orch_review.rs:59,1560-1603,1805` | 🔴 | `param` — `[backlog_orchestrator] pr_base = "next_release"` (défaut = détection : base de la branche d'intégration du repo, cf. B2) ; `pr_review` devrait pouvoir être désactivé proprement si pas de convention. |
| B2 | Prompts orchestrateur en dur décrivant LE workflow : `origin/next_release`, `gh pr create --base next_release --draft`, `/mr review`, `Verdict: APPROVED`, `gh pr merge --squash --delete-branch`, worktrees `~/worktrees/ac-<feat>`, `AGENTS.md` du repo | `crates/daemon/src/server/orch.rs:68-90` (`PROMPT`), `102-127` (`WORKER_PROMPT`), `134-143` (release PR), `167+` (`MANAGER_PROMPT`) ; `crates/daemon/src/server/orch_plan.rs:539-631` (`PLANNER_PROMPT`, `REVIEWER_PROMPT`, `IMPLEMENTER_PROMPT`, `CLAIMED_WORKER_PROMPT`) ; `crates/daemon/src/server/orch_failover.rs:167-180` | 🔴 | `abstraction`+`param` — les prompts sont déjà surchargeables (`prompt`, `manager_prompt` params) mais les templates *intégrés* décrivent notre repo. Deux voies : (a) templates neutres qui délèguent au `AGENTS.md`/`CLAUDE.md` du workspace cible (« lis le fichier de règles du repo ») au lieu de réciter `next_release`/`gh` ; (b) placeholders `{BASE}`/`{REVIEW_TRIGGER}`/`{MERGE_RULE}` substitués depuis un nouveau param `workflow_profile` (voir B6). |
| B3 | Release PR `next_release → main` : détection codée en dur dans l'orchestrateur (`gh pr list --head next_release --base main`), le registre (`prs.rs:248`, `orch.rs:1415-1422,2499-2536`), forgesync (`crates/forgesync/src/lib.rs:529,582-585` skip release PR), et `ALLOWED_BASE` côté review | `crates/daemon/src/server/orch.rs:134,1415,2499-2536` ; `crates/daemon/src/server/prs.rs:248,516` ; `crates/forgesync/src/lib.rs:582-585` ; `crates/daemon/src/server/orch_review.rs:58-59` | 🔴 | `param` — un triplet `integration_branch`/`release_branch` (défauts `next_release`/`main`) ou un `workflow = "none"|"next_release"` ; quand `none`, l'orchestrateur ne cherche ni release PR ni n'exclut de head. |
| B4 | Daemon-side `gh` : l'orchestrateur et le review appellent `gh pr list/view/comment/ready` en argv direct — aucune abstraction forge pour ces appels (seul le *registre* et forgesync passent par `[[forges]]`) | `crates/daemon/src/server/orch.rs:2321,2477,2504` ; `crates/daemon/src/server/orch_review.rs:1500-1782` ; `crates/daemon/src/server/prs.rs` (gh_api) | 🔴 | `abstraction` — route `gh` à travers la couche forge existante (GitLab `glab`, API REST GitLab) ou introduire un `ForgeCli` par `[[forges]]` ; court terme : documenter que `pr_review`/`backlog_orchestrator` exigent `gh` + GitHub (feature-flag). |
| B5 | Outils attendus par les prompts : `gh`, `ac-wait` (`tools/ac-wait`), `acctl`, `agentbacklog` — le worker prompt impose `ac-wait` pour toute attente > 1 min et `acctl steps set` ; rien ne vérifie leur présence avant de spawner | `crates/daemon/src/server/orch.rs:82,88,125,138,179,183` ; `crates/daemon/src/server/orch_plan.rs:558+` | 🟠 | `doc`+`détection` — prérequis explicites dans INSTALL.md ; à terme prompts conditionnels (« si `ac-wait` est absent, attends en une seule commande ») ou ship `ac-wait`/`acctl` avec le tarball daemon (déjà le cas pour `acctl`/`acp-claude` — `ac-wait` est un script shell dans `tools/`, pas packagé). |
| B6 | Le trigger `/mr review` et le verdict `## Verdict:` sont des conventions de NOTRE automatisation externe ; le review natif (`pr_review`) les reproduit — un repo sans cette convention n'a pas de review | `crates/daemon/src/server/orch_review.rs` (constantes `TRIGGER` via param `pr_review_trigger`, `VERDICT_BEGIN/END`, `EXTERNAL_ACK`) ; prompts `orch.rs:80`, `orch_plan.rs` | 🟠 | `param` — déjà partiellement paramétrable (`pr_review_trigger`) ; documenter que `pr_review=false` + `pr_review_trigger` custom = review générique ; envisager un `review_cmd`/`merge_rule` documenté par repo. |

## C. Chemins et environnement supposés

| # | Couplage | Localisation | Gravité | Découplage proposé |
|---|----------|--------------|---------|--------------------|
| C1 | Défaut `workspace = "/workspace"` dans `[backlog_orchestrator]` + défaut `repo_workspaces = {"AgentControl": "/workspace"}` (« hérité du script d'origine ») | `crates/daemon/src/server/cfg.rs:348,504-526` (`dws`, `drw`) ; affichage `orch_cfg.rs:386` | 🔴 | `détection` — défaut = cwd du démon (`<root>`) ou `git rev-parse --show-toplevel` du premier workspace ; `repo_workspaces` défaut vide (map vide = un workspace implicite = `workspace`). Un install standard atterrit sinon tous les workers dans `/workspace` inexistant. |
| C2 | `~/worktrees/ac-<feat>` dans les prompts workers/managers/failover | `orch.rs` prompts (cf. B2), `orch_failover.rs:178`, `AGENTS.md` | 🟠 | `param` — `{WORKTREE_DIR}` placeholder ou convention neutre « worktree dédié hors du checkout principal » laissée au `AGENTS.md` du repo cible. |
| C3 | État partagé `~/.agentbacklog/` : `orchestrator.lock` (flock), `orchestrator.pause`, `plans/`, `metrics.jsonl` — chemin codé via `$HOME`, non configurable | `crates/daemon/src/server/orch.rs:757-817` ; `crates/daemon/src/server/metrics.rs:51` ; `crates/daemon/src/server/orch_plan.rs:2753` | 🟠 | `param` — `[backlog_orchestrator] state_dir` (défaut `~/.agentbacklog` pour compat) ; ou dériver de `XDG_STATE_HOME`. |
| C4 | `agentbacklog` = CLI externe **obligatoire** pour l'orchestrateur : résolu via PATH puis via le composant `tool` déclaré ; version ≥ 0.7/0.9 exigée pour `plan_review`/`done_when` ; son install catalog pointe notre repo | `crates/daemon/src/server/orch.rs:26-27,1494-1545,2920-2947` ; `orch_plan.rs:2920-2947` ; `crates/daemon/src/components/catalog.rs:109-165` | 🔴 | `abstraction`/`doc` — c'est LA dépendance dure du composant. Options : (a) documenter `agentbacklog` comme prérequis + installer depuis PyPI si le projet y est publié (aujourd'hui `git+https://github.com/Wameuh/AgentBacklog`) ; (b) extraire l'interface « task queue » (`--list/--pop/--done/--claim…`) derrière un trait pour brancher une autre file ; (c) à minima, `catalog.rs` : rendre `repo`/`homepage`/`recipe` paramétrables ou passer par `uv tool install agentbacklog` depuis un index public. |
| C5 | `agentreviewer` dans le catalogue = composant « officiel » pointant `Wameuh/AgentReviewer` + `git+https` install + config rendue daemon-side | `crates/daemon/src/components/catalog.rs:169-180,351,495-535` ; `crates/daemon/src/components/mod.rs:1483` (test) | 🟠 | `param` — le catalogue devrait être extensible : entrées déclaratives dans `components.toml`/config plutôt que `CATALOG` statique, ou champs `repo`/`install` surchargés par param composant. |
| C6 | `uv tool install git+https://github.com/Wameuh/{AgentBacklog,AgentReviewer}` en dur dans les recettes d'install | `crates/daemon/src/components/catalog.rs:159,508` | 🔴 (si C4/C5 non découplés) | `param` — recette = template `uv tool install {spec}` avec `spec` paramétrable. |
| C7 | Vue « Backlog » du client : `const COMP = "agentbacklog"` en dur — la feature ne marche que si le composant s'appelle exactement `agentbacklog` et expose la query `list` | `client/src/features/backlog.ts:32` (+ i18n `bl_absent`/`bl_noquery` dans `client/src/i18n/*.ts`) | 🟠 | `param` — le client devrait découvrir le composant orchestrateur via `daemon_status`/`list_components` (le démon sait lequel est `backlog-orchestrator`) plutôt que par nom ; ou documenter le nom contractuel. |

## D. Tooling et agents supposés

| # | Couplage | Localisation | Gravité | Découplage proposé |
|---|----------|--------------|---------|--------------------|
| D1 | Défauts orchestrateur : `agent = "devin"`, `model = "swe-2-high"`, `mode = "bypass"`, `thought_level = "max"`, préfixes `Backlog-W/P/R`, `PRReview-`, groupe `"AgentControl"` | `crates/daemon/src/server/cfg.rs` (`dagent`, `dmodel`, `dmode`, `dlevel`, `dprefix`, `dgroup`) ; `orch_cfg.rs` (schémas params) | 🟠 | `param` — déjà tous paramétrables ; le problème est le *défaut*. `détection` : `agent` = premier profil `[agents.*]` `available:true` ; `model`/`mode` = vide (= agent décide) plutôt que vocabulaire Devin en dur. Documenter que ces défauts sont calibrés Devin. |
| D2 | Agents presets `devin`/`cursor` + auto-découverte adapters (claude/codex/gemini/opencode) — déjà générique | `crates/daemon/src/server/agents_cfg.rs`, `agents_auto.rs` | ⚪ | Rien — c'est le bon pattern, à généraliser aux points B/C/D1. |
| D3 | `gh` requis authentifié pour forgesync (`gh auth status --hostname github.com`) et les appels orch ; `glab`/API GitLab partiellement câblés dans forgesync mais pas dans orch/review | `crates/forgesync/src/lib.rs:172-175,574-577` ; cf. B4 | 🟠 | `doc` (prérequis) + `abstraction` (B4). |
| D4 | `ac-wait` : les prompts exigent ce script (`tools/ac-wait`, bash) pour toute attente ; l'orchestrateur *lit* son répertoire d'état (`~/.local/state/ac-wait`) pour détecter les waiters vivants — couplage fonctionnel, pas seulement documentaire | `orch.rs:827-1142` (scan waiters), prompts B2 | 🟠 | `param`/`abstraction` — le scan des waiters devrait être une RPC `ac-wait`-side ou un répertoire déclaré ; documenter `ac-wait` comme composant à installer (packager `tools/ac-wait` dans le tarball). |
| D5 | Skills embarqués (`acctl`, `ac-wait`, `ac-steps`) provisionnés dans `.devin/skills`, `.claude/skills`, `.agents/skills`… du cwd de chaque session — déjà générique, mais les *prompts* citent `ac-steps`/`ac-wait` par leur nom | `packaging/config.example.toml` (section provision_skills), `skills/` | ⚪ | `doc` — les skills suivent le provider ; OK. |

## E. Packaging, install, CI, hooks

| # | Couplage | Localisation | Gravité | Découplage proposé |
|---|----------|--------------|---------|--------------------|
| E1 | Workflows GitHub Actions propres à ce repo : `build.yml` (checks nommés `daemon-linux`, `client`, `client-macos`, `client-android` — noms cités dans AGENTS.md), `version.yml` (release-bump sur PR `next_release`→`main`), `auto-release-pr.yml`, `release-on-merge.yml`, `reconcile-next-release.yml` — branches `next_release`/`main` en dur, secrets `RELEASE_PAT`, `ANDROID_KEYSTORE_*` | `.github/workflows/*.yml` | ⚪ (pour CE repo) / 🔴 (si documentés comme requis) | `doc` — ce sont les workflows *du projet*, pas du produit. Mais `packaging/INSTALL.md` devrait préciser qu'AgentControl n'exige AUCUN de ces workflows chez l'utilisateur ; fournir un `docs/ci-example.md` optionnel pour qui veut reproduire le pipeline. |
| E2 | Hooks `.devin/hooks` (`guard-main-worktree.py` = PROTECTED_BRANCH `next_release` en dur ; `block-merge.py` = règle `Verdict: APPROVED` + base `next_release`) — couplés à notre workflow, commités dans le repo | `.devin/hooks/*.py`, `.devin/hooks.v1.json` | 🟠 | `doc`+`feature-flag` — documenter comme hooks d'exemple optionnels ; externaliser `PROTECTED_BRANCH`/base/règle en variables d'env ou args ; un utilisateur qui les copie sur un autre repo aura des blocages incompréhensibles. |
| E3 | `packaging/INSTALL.md`, `deploy.sh`, `config.example.toml`, `agentcontrol-daemon.service` : globalement propres (systemd --user, `~/.local/bin`, `127.0.0.1:7331`, `~/.agentcontrol`) — MAIS la section 5 « migrating a supervisord service » et `components.example.toml` documentent notre migration devcontainer→daemon ; `deploy.sh` suppose systemd user (loginctl linger) | `packaging/INSTALL.md:82-166`, `deploy.sh`, `components.example.toml` | 🟠 | `doc` — séparer « install générique » (sections 1-4, OK) de « notre infra » (section 5, devcontainer) ; `deploy.sh` documenter qu'il faut systemd ou adapter. Ajouter une section « Workflow PR optionnel : orchestrateur » listant les prérequis (agentbacklog, gh, convention de branches) — aujourd'hui absente. |
| E4 | `.devcontainer/` (Dockerfile, compose, supervisord) non commité (untracked) — infra de dev de l'auteur, pas du produit ; superviseur mentionné dans docs | `/workspace/.devcontainer/` (hors git), `docs/components-integration.md:14-42,157` | ⚪ | `doc` — déjà hors repo ; s'assurer que `docs/components-integration.md` distingue « nos composants déployés » de « ce que le démon gère ». |
| E5 | Artefacts release attendus par self-update : `agentcontrol-server-<arch>-linux.tar.gz` publié sur `Wameuh/AgentControl/releases` par `build.yml` — le nommage est cohérent mais l'URL est en dur (A1) ; `acp-claude`, `acctl` dans le tarball (bien) mais pas `ac-wait` | `selfupdate.rs:226`, `.github/workflows/build.yml`, `packaging/INSTALL.md:1-8` | 🟠 | `param` (A1) + inclure `tools/ac-wait` dans le tarball pour que les prompts B2 fonctionnent sur une install fraîche. |

## F. Docs, skills, plugins, divers

| # | Couplage | Localisation | Gravité | Découplage proposé |
|---|----------|--------------|---------|--------------------|
| F1 | `AGENTS.md` racine = LE document de workflow (next_release, hooks, ac-wait, merge rules) — c'est à la fois la doc du projet ET le « contrat » lu par les prompts orchestrateur (`{WS}/AGENTS.md`) | `/workspace/AGENTS.md`, référencé par tous les prompts B2 | 🔴 (structurel) | `abstraction` — c'est en fait la bonne architecture (le repo porte ses règles) MAIS les prompts orchestrateur *citent* nos règles en plus du renvoi à AGENTS.md → doublon source de divergence. Découplage = prompts neutres (B2a) + `AGENTS.md` comme seule source de vérité par repo. |
| F2 | `README.md` : architecture diagram mentionne agentbacklog/orchestrateur, lien releases `Wameuh/AgentControl` | `README.md:26,34,76,147` | ⚪ | `doc` — normal pour le README du projet ; veiller à ce que la doc *d'installation* (INSTALL.md) ne renvoie pas aux prérequis cachés. |
| F3 | `docs/components-integration.md` : très couplé (noms `Wameuh/AgentBacklog`, `next_release`, `{"AgentControl":"/workspace"}`, supervisord devcontainer) — c'est la doc *de notre déploiement* | `docs/components-integration.md` (lignes 14-42, 249-261, 442, 659) | ⚪/🟠 | `doc` — renommer/splitter : « architecture du système de composants » (générique) vs « notre déploiement de référence » ; les URLs `Wameuh/*` dans les exemples de catalogue doivent devenir `{owner}/{repo}`. |
| F4 | `client/tools/android-upload-keystore.sh` : commentaires « dans le repo Wameuh/AgentControl », `O=Wameuh` dans le DN du keystore | `client/tools/android-upload-keystore.sh:15-18,52,62-67` | ⚪ | `doc`/`param` — script de tooling CI du projet ; paramétrer `O=` et le repo en variables. |
| F5 | `tools/ac-wait` (bash), `client/tools/release_notes*.py/txt`, `bump_version.py` : scripts du pipeline release — couplés au workflow mais pas au runtime | `tools/`, `client/tools/` | ⚪ | `doc` — documenter comme scripts CI optionnels. |
| F6 | Tests unitaires avec fixtures `Wameuh/AgentControl`, `/workspace`, `github.com` — normal (données de test) | `forges.rs:526+`, `prs.rs:917+`, `orch_review.rs:2075+`, `catalog.rs:1139+`, `render.rs:481`, `components/mod.rs:1483+`, `orch.rs:3249+`, `orch_plan.rs:3324`, `orch_gc.rs:245`, `metrics.rs:419` | ⚪ | Rien — fixtures. |
| F7 | `mcp.rs` descriptions d'outools citant `next_release` et `Wameuh/AgentControl` en exemple | `crates/daemon/src/mcp.rs:200,206` | ⚪ | `doc` — exemples à généraliser (`ex: <owner>/<repo>`) ou garder (inoffensif) ; `next_release` dans la description de `request_review` devrait devenir « la base configurée (`pr_base`) ». |
| F8 | `forgesync` : skip de la release PR `next_release→main` codé en dur (`head.ref == "next_release"`, base `main` implicite) | `crates/forgesync/src/lib.rs:582-585` ; `crates/daemon/src/server/prs.rs:516-518` (review orchestrateur = github.com seul) | 🟠 | `param` — même source de vérité que B3 (`integration_branch`/`release_branch`/`workflow`) ; `prs.rs:516` devrait consulter `[[forges]]` au lieu de `== "github.com"`. |
| F9 | `proto`/`acctl` : `split_repo_arg`, mapping kind↔host (`github`↔github.com, `gitlab`↔sinon, `gerrit`) — déjà multi-forge | `crates/acctl/src/pr.rs:196-197,369-376,833+` ; `crates/proto/src/lib.rs:681,1236,1633` | ⚪ | Rien — bon pattern. |
| F10 | `docs/forges.md` existe déjà et documente `[[forges]]` — la brique d'abstraction forge est là, sous-exploitée | `docs/forges.md` | ⚪ | Réutiliser comme base du découplage B4. |

## G. Ce qui est DÉJÀ découplé (ne pas refaire)

- `[[forges]]` config + `ForgeLive` hot-reload : GitLab/Gerrit déclarables, hôtes implicites github.com/gitlab.com (`forges.rs`, `forges_cfg.rs`, `docs/forges.md`).
- `[agents.*]` profils : presets devin/cursor, auto-découverte adapters, `commands` fallbacks, install `npm i -g` daemon-side (#261, #277, #285).
- Tous les params `[backlog_orchestrator]` hot-settables via `acctl comp set` + UI (agents par rôle, modes, modèles, flags review, `repo_workspaces`, prompts sur mesure).
- `[backlog_orchestrator] prompt`/`manager_prompt` : surcharge totale des prompts (mais les défauts restent couplés — B2).
- `provision_skills` : skills embarqués multi-provider.
- Le composant `backlog-orchestrator` est **opt-in** (`enabled` param) — un tiers n'a pas l'orchestrateur forcé ; le problème est qu'*s'il l'active*, tout suppose notre env.
- Tarball daemon contient `acctl` + `acp-claude` ; client Tauri et APK construits par CI.

## Synthèse par gravité

**🔴 Bloquants pour un tiers** (features cassées ou silencieusement
mauvaises) : A1, A2, A3, B1, B2, B3, B4, C1, C4, C6, F1.

**🟠 Dégradés** : A5, B5, B6, C2, C3, C5, C7, D1, D3, D4, E2, E3, E5, F3, F8.

**⚪ Cosmétique / déjà correct** : A4, D2, D5, E1, E4, F2, F4, F5, F6, F7, F9, F10.

## Items fils proposés (ordonnés)

1. **`update_repo` paramétrable** — `[daemon] update_repo` + `release_repo` client + `connect.rs` BASE + CSP conditionnelle (A1-A5). *Débloque l'update pour tout fork.*
2. **`workflow_profile` / branches configurables** — `integration_branch` (défaut `next_release`), `release_branch` (défaut `main`), `pr_base` pour `ALLOWED_BASE` et le scan `gh pr list` ; propager à `forgesync` skip et `prs.rs` (B1, B3, F8). *Débloque review + orchestrateur sur un repo sans cette convention.*
3. **Prompts orchestrateur neutres** — réécrire les templates `PROMPT`/`WORKER_PROMPT`/`MANAGER_PROMPT`/`PLANNER_*`/`REVIEWER_*`/`IMPLEMENTER_*`/`CLAIMED_WORKER_PROMPT`/failover pour qu'ils délèguent les règles de branche/PR/merge au `AGENTS.md`/`CLAUDE.md` du workspace (`{WS}`) au lieu de réciter `next_release`/`gh`/`Verdict` ; placeholders `{BASE}`/`{REVIEW_TRIGGER}` si un `workflow_profile` existe (B2, F1). *Gros morceau — le plus rentable pour l'usage tiers.*
4. **`agentbacklog` : prérequis documenté + source d'install découplée** — INSTALL.md section « orchestrateur » ; `catalog.rs` `repo`/`homepage`/`recipe` paramétrables ou passage à un package public ; découverte du composant backlog côté client au lieu du nom en dur `agentbacklog` (C7) ; décision sur l'abstraction « task queue » (C4, C5, C6).
5. **Défauts workspace/agent** — `workspace` défaut = cwd démon ou détection git ; `repo_workspaces` défaut vide ; `agent`/`model`/`mode` défauts vides ou détection (C1, C2, D1).
6. **`gh` → couche forge pour orch/review** — router les appels `gh pr *` via une abstraction `ForgeCli` adossée à `[[forges]]` (GitLab `glab`/API) ou documenter la limitation GitHub-only (B4, D3).
7. **`state_dir` agentbacklog + packager `ac-wait`** — `[backlog_orchestrator] state_dir` ; inclure `tools/ac-wait` dans le tarball ; documenter `ac-wait`/`acctl`/`agentbacklog` comme prérequis (C3, D4, E5).
8. **Hooks `.devin/` documentés comme optionnels** — externaliser `PROTECTED_BRANCH`/base/règle de merge ; note dans AGENTS.md/INSTALL.md (E2).
9. **Docs : splitter générique vs notre infra** — `components-integration.md`, INSTALL.md §5, exemples catalogue `Wameuh/*` → `{owner}/{repo}` ; `docs/ci-example.md` optionnel pour reproduire notre pipeline (E1, E3, F3).
10. **CSP dynamique** — `connect-src` GitHub seulement si forge/update l'exige (A5) — peut fusionner avec l'item 1.

Ordre conseillé : 1 → 2 → 5 (petits, forte valeur) → 4 → 3 (le plus
gros) → 6 → 7 → 8 → 9 → 10.
