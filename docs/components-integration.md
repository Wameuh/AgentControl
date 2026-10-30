# Intégration des composants au démon — analyse & architecture

> Document de conception : cartographie de l'écosystème, modèle de
> « composant » managé par le démon, surface proto, vue diagramme, et
> plan de découpage en tâches implémentables (items AgentBacklog).
> Date : 2026-10-29 — analyse pour la tâche backlog #19.

---

## 1. Cartographie de l'existant

L'écosystème a grandi organiquement. Aujourd'hui, seul le serveur MCP est
intégré au démon ; tout le reste vit à côté, supervisé par d'autres moyens
(supervisord du conteneur dev, systemd --user, crontab, rien du tout).

### 1.1 Ce que le démon gère déjà (interne, compilé dedans)

| Élément | Rôle | Où |
| --- | --- | --- |
| Sessions agent ACP | spawn/monitor des process agents, journal events.jsonl + blobs, reprise auto post-crash | `crates/daemon/src/session/`, `server/sessions.rs` |
| Serveur MCP `agentcontrol` | outils agent↔agent (list/send/create/read/sleep) injecté via `mcpServers` de session/new\|load ; sous-commande cachée `mcp-serve` du même binaire, spawnée par chaque agent | `crates/daemon/src/mcp.rs`, injection `server/mod.rs::mcp_servers` |
| Registre PR ↔ session | prs.json, watcher par session (détection `gh pr create`), RPC `pr_list/track/untrack/sync` (collecte `crates/forgesync`) | `crates/daemon/src/prs.rs`, `server/prs.rs` |
| Web UI | listener HTTP loopback + token (`bind_http`), sert la SPA (embed ou disque), pont WS vers le proto NDJSON, /rpc /media /download | `crates/daemon/src/web/` |
| Janitor | rétention : compaction journaux, purge LRU blobs, purge sessions inactives | `server/mod.rs::janitor` |
| Terminaux PTY | shell par connexion, cycle de vie borné à la conn | `server/term.rs` |
| Self-update | `UpdateDaemon` : download release + remplacement binaire + exit (relance par superviseur) | `selfupdate.rs` |
| `acctl` | CLI client du démon (même proto NDJSON) | `crates/acctl` |

### 1.2 Ce qui vit à côté (externe)

| Programme | Rôle | Supervision actuelle | État/log |
| --- | --- | --- | --- |
| `backlog-orchestrator.py` | poll 60 s : spawn les workers `Backlog-W*` (acctl spawn, devin/bypass/swe-2-high, groupe AgentControl), nudge les workers idle avec PR ouverte/tâche wip, supprime les sessions soldées, release les wip orphelins ; ledger worker→{tâches,PRs} dans son state ; le titre suit l'item wip et l'orchestrateur natif journalise le contenu de l'item popé en `user_message` `origin` (post présentationnel, jamais livré à l'agent ; suivi `posted_wip` dans le state — une reprise depuis le script Python, qui ne préserve pas ce champ dans `assignments`, reposte une fois le wip courant de chaque worker) | `~/.supervisor/conf.d/backlog-orchestrator.conf` (autostart+autorestart, env `AGENTCONTROL_TCP=127.0.0.1:7331`) | `~/.agentbacklog/orchestrator-{state.json,log,stdout.log,stderr.log}` ; lock flock `orchestrator.lock` ; pause via `orchestrator.pause` ; CLI `--status/--once/--dry-run` |
| `agentbacklog` (CLI) | file FIFO de features en SQLite — transactions atomiques, **pas de démon** | `uv tool` dans `~/.local/bin` (repo `github.com/Wameuh/AgentBacklog`) | `~/.agentbacklog/backlog.db` |
| `pr-db` | registre PR local sqlite — **legacy**, remplacé par le registre intégré (feat #59) | `~/.local/bin` | `~/.agentcontrol/prs.db` |
| `graphify`, `graphify-mcp` | outils uv externes | `~/.local/bin` | — |
| `acp-mock-agent` | agent ACP factice de test | binaire du tarball release | — |
| `supervisord` | **PID 1 du conteneur dev** : sshd + cron + agentcontrol-daemon + backlog-orchestrator | conf `/etc/supervisor/conf.d/` + `~/.supervisor/conf.d/` | — |
| `cron`/`crontab` | jobs planifiés | supervisé (volume `cron-spool`) | — |
| Review automation | `/mr review` → session de review | **autre serveur**, hors périmètre local | — |
| systemd `agentcontrol-daemon.service` | superviseur du démon en prod (`Restart=always`) | `packaging/` | — |

### 1.3 Managé vs externe — le constat

- Le démon sait déjà gérer **des process** (sessions agents) : spawn via
  `tokio::process`, surveillance de l'exit, redémarrage à la demande,
  env injectée. Le mécanisme existe — il est spécialisé ACP.
- Les services satellites (orchestrateur, futurs MCP/scripts) refont chacun
  leur propre supervision, logs, état et CLI — et sont **invisibles dans le
  client** : on ne voit ni qu'ils tournent, ni qu'ils sont liés aux sessions.
- La vision : tout programme de l'écosystème devient un **composant
  déclaré** — le démon l'affiche, offre sa configuration, et (selon le
  type) pilote son cycle de vie ; le client propose une vue diagramme des
  programmes + sessions attachées.

---

## 2. Architecture cible

### 2.1 Modèle de composant : quatre `kind`

```toml
# <root>/components.toml — fichier possédé par le démon
# (config.toml reste édité à la main ; components.toml est réécrit
#  atomiquement par les RPC d'édition)

[[components]]
name = "backlog-orchestrator"
kind = "process"                 # internal | process | external | tool
command = ["/usr/bin/python3", "/home/dev/.local/bin/backlog-orchestrator.py"]
cwd = "/home/dev"
autostart = true
restart = "on-failure"           # always | on-failure | never
restart_sec = 5                  # backoff de base (exponentiel ×2, cap 60 s)
healthcheck = { kind = "process" }  # none | process | tcp | http | command
description = "Spawn les workers du backlog AgentBacklog"

[components.env]
AGENTCONTROL_TCP = "127.0.0.1:7331"

[[components]]
name = "agentbacklog"
kind = "tool"                    # CLI stateless : pas de process
command = ["agentbacklog", "--version"]
description = "File FIFO de features (SQLite ~/.agentbacklog/backlog.db)"
```

| `kind` | Sens | Cycle de vie | Exemples |
| --- | --- | --- | --- |
| `internal` | feature compilée dans le démon | pas de process ; état déduit de la config (web actif si `bind_http`, a2a si activé…) | mcp-serve, prs, web, janitor |
| `process` | programme long managé par le démon | spawn/stop/restart/healthcheck/logs | backlog-orchestrator, futurs MCP/scripts |
| `external` | déclaré, géré ailleurs | observé par healthcheck seulement — le démon ne touche jamais au cycle de vie | supervisord, sshd, cron, review-bot, systemd unit |
| `tool` | CLI/install stateless | présence + version (`command --version` sonde) ; pas de process long — états `installed`/`absent` | agentbacklog, acctl, graphify |

Les composants `internal` sont **déclarés dans le code** (pas dans
components.toml) et apparaissent dans `ListComponents` comme les autres —
le diagramme montre ainsi tout l'écosystème d'un coup.

Les `tool` n'ont **pas de cycle de vie** — `ComponentStart/Stop/Restart`
sont refusés. Leur `state` est une observation : `installed` quand
argv0 est résolvable (PATH du démon ou chemin exécutable), `absent`
sinon — pas une panne, juste « pas là ». La sonde périodique (30 s)
exécute `command` (typiquement `… --version`, timeout 3 s) et publie
la 1re ligne en `status.version` (≤200 c) — l'état `installed`
lui-même est resondé à la demande, frais même avant la 1re passe.
Santé et version ne sont retenues que pour un tool `installed` :
`absent` invalide les deux caches (l'état se suffit — pas de
« santé : fail » résiduel à la réinstallation), tout comme un upsert
de spec, en attendant la passe suivante.

### 2.2 Cycle de vie d'un `process` managé

- **Spawn** : `tokio::process::Command` au boot du démon si `autostart`,
  sinon à la demande (`ComponentStart`). `setsid` n'est pas nécessaire :
  le process meurt avec le démon, comme les agents.
- **Env injectée automatiquement** (comme `agent_parts` pour l'a2a) :
  `AGENTCONTROL_SOCK`, `AGENTCONTROL_TCP`, `AGENTCONTROL_HOME`,
  `AGENTCONTROL_COMPONENT=<name>` → un composant découvre le démon sans
  config, sur n'importe quelle installation. `AGENTCONTROL_COMPONENT`
  sert aussi à l'attribution des sessions créées (§2.3.1).
- **Restart** : `on-failure`/`always` avec backoff exponentiel
  (`restart_sec` de base, ×2, cap 60 s, reset après 5 min stables) ;
  compteur `restarts` + `last_exit` exposés.
- **Healthcheck** périodique (30 s) : `process` = pid vivant ;
  `tcp`/`http` = sonde ; `command` = petit check exécuté — une spec
  `command` est donc de l'exécution **périodique** (pas à la demande)
  au même niveau de confiance que `NewSession` (§3). État :
  `running | degraded | stopped | failed | starting`.
- **Logs** : stdout/stderr redirigés vers
  `<root>/components/<name>.log`, rotation bornée (1 Mio ×2).
  `ComponentLogs` fait du tail paginé.
- **Arrêt du démon** : SIGTERM aux process managés — même destin que
  les agents (interrompus au restart démon, relancés au boot si
  autostart), avec un signal plus doux que le `kill_on_drop` des
  agents. Un composant qui **ne doit pas** être interrompu par un
  restart démon → `kind = "external"`.
- **Orphans** (démon SIGKILLé) : pidfile
  `<root>/components/<name>.pid`. Au boot : pid mort → clean ; pid vivant
  → V1 kill + respawn (déterministe — évite un process orphelin qui garde
  des fd/état). Option `adopt` possible plus tard.
- **Garde** : refuser une spec `process` dont la commande pointe le
  binaire démon lui-même, `supervisord` ou `systemctl` — le système de
  composants ne gère ni son propre superviseur ni lui-même.

### 2.3 Surface proto

Nouveaux `ReqKind` (NDJSON, comme le reste — ajout rétrocompatible : un
vieux client les ignore ; un vieux démon répond `parse: unknown variant`
avec l'`id` correctement extrait, donc l'appel échoue proprement) :

| ReqKind | Réponse |
| --- | --- |
| `ListComponents` | `[{name, kind, state, pid?, autostart, health, uptime_s?, restarts, last_exit?, version?, description?, source?}]` |
| `ComponentInfo {name}` | spec complète + état détaillé |
| `ComponentStart/Stop/Restart {name}` | `{name, state}` |
| `ComponentLogs {name, offset?, limit?}` | `{data(b64)|lines, size, truncated}` (tail paginé) |
| `ComponentUpsert {name, spec}` | réécrit components.toml atomiquement |
| `ComponentRemove {name}` | idem (+ stop si process vivant) |
| `ComponentCatalog {refresh?}` | entrées du catalogue (§2.5) |
| `ComponentInstall {name}` | exécute la recette + upsert la spec |

Capability : `daemon_status.components = true` → le client gate ses
boutons comme pour `terminal`/`first_post`. Le commit qui ajoute les
ReqKind porte le mot-clé `(sync)` (nouvelle surface client↔démon).

Push : pas de canal d'events daemon-wide (le `Subscribe` actuel est
par-session) — le client **poll** (`ListComponents` + `ListSessions`,
~3 s pendant que la vue est ouverte), comme le watchdog connexions
existant (10 s). Un `component_status_changed` push viendra en V2 si le
poll s'avère insuffisant.

#### 2.3.1 Attribution composant ↔ session — le chaînon du diagramme

La vue diagramme attache les sessions à leur programme parent ; pour
cela le démon doit savoir **quel composant** a créé une session.
Aujourd'hui `spawned_by` ne couvre que le cas session→session
(`PeerSpawn` exige `from` = session vivante et pose
`spawned_by = Some(from)` ; `NewSession` pose `None`) : un composant
`process` qui crée des workers via `acctl` produirait des sessions
rattachées à une session pivot invisible ou à la racine — **jamais au
nœud composant** (l'env `AGENTCONTROL_COMPONENT` vit côté composant,
le démon ne la revoit pas sur le RPC).

Mécanisme : un champ **`created_by`** sur les requêtes de création
(`NewSession`, à marge `PeerSpawn` — `AttachSession` idem si besoin :
attacher une session native sous un nœud composant), persisté dans
`SessionMeta` et exposé par `ListSessions`/`SessionInfo` — valeurs
`component:<name>` | `service:<nom>` | `user` (défaut). Le composant
connaît son nom via `AGENTCONTROL_COMPONENT` (§2.2) et le re-transmet
— ex. l'orchestrateur `acctl new … --as backlog-orchestrator` produit
des `Backlog-W*` rattachés au nœud orchestrateur dans le diagramme.
Validation cheap côté démon (spec #24) : n'accepter `component:<name>`
que si `<name>` figure dans components.toml — le champ reste
auto-déclaré (même confiance que `NewSession`), mais un composant ne
peut pas s'attribuer un nom inexistant.

C'est **le même canal que la provenance externe de l'item #21**
(spawn par services externes, en cours) : les deux chantiers décrivent
le même besoin — le champ de la spec de #24 est celui que #26 consomme
pour les arêtes ; si #21 a déjà tranché une forme (`origin`) au moment
où #24 lande, elle fait foi et le diagramme s'y aligne — sinon c'est
le choix de #24 (`created_by`) qui fixe le nom consommé par #26.
À intégrer au modèle dès #24 pour éviter une seconde évolution proto
(un second cycle `(sync)`).

### 2.4 acctl

`acctl comp <sub>` — miroir des RPC pour l'usage terminal (agents,
scripts, orchestrateur lui-même) :

```
acctl comp ls [--json]            # nom, kind, état, pid, uptime, restarts
acctl comp info <name>            # spec + état
acctl comp start|stop|restart <name>
acctl comp logs <name> [--tail N]
acctl comp catalog [--json] [--refresh]  # + versions upstream
acctl comp install <name>         # recette catalogue → spec managée
acctl comp add <name> --kind K --cmd "<argv>" [--env K=V …] [--autostart]
acctl comp rm <name>
```

### 2.5 Catalogue & installation — les « options d'intégration »

Objectif : **sur une autre installation**, proposer d'intégrer un
composant connu en quelques clics — ses métadonnées (description,
version, doc) étant récupérées depuis sa source upstream.

- **Catalogue embarqué** dans le démon (table const ou JSON packagé) :
  `name, description, repo ("Wameuh/AgentBacklog"), homepage,
  suggested_spec (kind/command/env/healthcheck préremplis), recipe`.
- **Fetch upstream** (`ComponentCatalog refresh: true`) :
  `ureq` (déjà dep du démon, TLS rustls) →
  `api.github.com/repos/<repo>/releases/latest` (tag + body) ; repli
  `releases` → `tags` → HEAD. Résultat mis en cache quelques minutes.
  Ex. agentbacklog → `Wameuh/AgentBacklog`, dernière release/tag.
- **Recettes déclaratives** auditées dans le code (jamais de commande
  arbitraire envoyée par le client) : étapes typées
  `run {argv}` / `write_file {path, content}` / `upsert_spec {}`.
  Exemples :
  - `agentbacklog` : `uv tool install git+https://github.com/Wameuh/AgentBacklog`
    (install upstream documentée) + spec `tool`.
  - `agentreviewer` : `uv tool install git+https://github.com/Wameuh/AgentReviewer`
    + spec `process` (boucle `sh` autour du watcher one-shot — il reçoit
    `AGENTCONTROL_SOCK`/`AGENTCONTROL_TCP` injectés et crée ses reviews
    en sessions `origin=component:agentreviewer`, groupe
    « AgentReviewer »).
  - `backlog-orchestrator` : le script vit aujourd'hui hors repo dans
    `~/.local/bin` → **recommandation : le versionner** (repo
    `Wameuh/AgentBacklog` ou `contrib/` d'AgentControl) pour que la
    recette ait une source canonique (`raw.githubusercontent.com`) ;
    en attendant, la recette installe la spec `process` pointant le
    chemin existant quand il est détecté.
- **Détection** : la catalogue marque chaque entrée `installed` (commande
  trouvée sur le PATH du démon / fichier présent) + version si sondable
  → le client affiche « installé v0.2 — intégrer ? » vs « disponible ».
- Futurs composants = une entrée catalogue, pas de nouveau code client.

### 2.6 UI client — panneau « Composants »

Vue overlay plein `#chat` (pattern `settings-view`) ouverte depuis un
bouton titlebar/menu ⋮ :

- **Liste** des composants de la connexion courante : nom, kind badge,
  état animé, uptime, restarts ; actions contextuelles
  start/stop/restart + viewer logs (tail auto-refresh).
- **Édition** : formulaire sur la spec (command, env, autostart, restart,
  healthcheck) → `ComponentUpsert`.
- **Onglet Catalogue** : cartes des entrées connues (nom, description,
  version upstream fetchée, « installé ? »), bouton « Intégrer » →
  `ComponentInstall` puis formulaire prérempli de la `suggested_spec`.
- Capability-gated sur `components: true`.

### 2.7 Vue diagramme animée

La pièce « vivante » : **graphe des programmes et des sessions**.

- **Nœuds** : le démon au centre ; composants en anneau (icône par kind) ;
  sessions groupées autour de leur parent — le nœud composant quand
  `created_by` = `component:<name>` (§2.3.1 : les workers `Backlog-W*`
  sous l'orchestrateur), la session parente quand `spawned_by`, le
  démon sinon.
- **Arêtes** : démon→composant managé (`manages`), composant→démon
  (RPC — l'orchestrateur parle au socket), composant→session
  (`created_by`), démon→session (`spawned`), session→session
  (`spawned_by`/`peer_message`).
- **États animés** : pulse sur les sessions `working`, point coloré par
  état (idle/working/waiting/dead/degraded/stopped), arêtes en pointillés
  animés sur les flux actifs. Les `tool` sont des **nœuds passifs** :
  icône dédiée, `installed` = liseré discret, `absent` = nœud et arête
  en tirets quasi effacés (observation, pas panne), version sondée en
  tooltip + carte info. Tooltips : titre, état, uptime ; clic
  session → ouvre le chat ; clic composant → panneau §2.6.
- **Technique** : **SVG en vanilla TS** (le client n'a aucune dep de
  graphe — pas de lib externe). Layout radial déterministe (démon centre,
  composants sur un cercle, sessions sur des arcs autour de leur parent)
  — pas de force-directed : stable, lisible, suffisant (< ~100 nœuds).
  Animations CSS (`stroke-dashoffset`, `transform`) — pas de canvas.
- **Données** : `ListComponents` + `ListSessions` + `PrList` en poll ~3 s
  uniquement pendant que la vue est ouverte.
- Intégration : nouvelle vue overlay `map-view` à côté de `settings-view`
  (même mécanique open/close + `nav.push`), ouverte via le menu ⋮ et un
  bouton titlebar.

### 2.8 Choix tranchés : internal vs managé vs externe

| Composant | Décision | Pourquoi |
| --- | --- | --- |
| mcp-serve | `internal` | déjà sous-commande du binaire — le diagramme compte les instances par session |
| registre PR | `internal` | déjà dans le démon |
| web UI | `internal` | feature `bind_http` — état = actif/inactif |
| janitor | `internal` | tâche interne |
| backlog-orchestrator | **`process` managé** | cas d'usage typique : migre de supervisord vers le démon (cycle de vie + logs + visibilité) |
| agentbacklog | `tool` | CLI stateless — nœud passif + version + install catalogue |
| acctl | `tool` | idem |
| pr-db | externe/legacy | en fin de transition (feat #59) — déclarable en `external` si encore présent |
| supervisord / sshd / cron | `external` | infra du conteneur — jamais managés (le démon ne touche pas à son propre superviseur) |
| agentreviewer | **`process` managé** | watcher du même démon qu'il pilote — env `AGENTCONTROL_*` injectée, reviews en sessions groupées (un watcher d'un autre démon/hôte resterait `external`) |
| le démon lui-même | nœud racine, pas un composant | le système ne se supervise pas lui-même |

### 2.9 « Manager » — intendance backlog par workspace

Quand le composant `agentbacklog` est déclaré **et** l'orchestrateur
natif actif (`enabled`), le démon expose `orchestrator.workspaces` —
le routing **repo → cwd** (`[backlog_orchestrator] repo_workspaces`,
défaut `{"AgentControl": "/workspace"}` hérité du `REPO_WORKSPACES` du
script) — et la vue Backlog propose un Manager par entrée : bouton
« Créer » → RPC `manager_spawn{repo}` → session `Manager · <repo>`
(`origin: feature:backlog-manager`, prompt `MANAGER_PROMPT` intégré
surchargeable via `manager_prompt`). Le Manager est une intendance —
transformer des demandes en items structurés (`--add --repo`
`--depends-on`, découpage ordonné), répondre aux questions d'état,
ajuster la file — jamais un worker : pas de `--pop`/`--done`, et son
titre échappe au `title_prefix` (en plus de l'exclusion `origin` du
filtre workers → jamais nudge/rm/garde PR). Spawn idempotent
(`manager_lock` + dédupe origin+titre+état vivant) — un Manager
existant est renvoyé tel quel ; `rename_session` casse la dédup
(assumé : le titre est le contrat).

---

## 3. Risques & contraintes

1. **Le démon orchestre les agents** — le redémarrer tue les sessions
   (règle AGENTS.md). Un `process` managé subit le même sort (SIGTERM à
   l'arrêt, respawn au boot si autostart). C'est **cohérent** mais à
   documenter : un service devant survivre au restart démon → `external`.
   Le développement/test des features démon se fait sur un
   `AGENTCONTROL_HOME` jetable — jamais sur le démon live.
2. **Compat proto** : nouveaux `ReqKind` = extension sûre (vieux client
   ignore ; vieux démon → `parse:` propre avec id extrait). Le client
   gate sur `daemon_status.components`. Mot-clé `(sync)` sur la PR
   proto+daemon (incompat client↔démon = auto-update).
3. **Sécurité** : le démon ne bind que loopback **par défaut** —
   socket unix 0600 et `bind_http` enforcé loopback ; `bind_tcp` reste
   configurable librement dans `config.toml` (sans garde) — la
   confiance des RPC composants vaut « qui parle au socket pilote les
   agents » (`NewSession` peut déjà spawn des process ;
   `ComponentInstall` n'exécute que des **recettes déclaratives
   embarquées**, jamais du shell arbitraire envoyé par le client).
   Gardes : refuser les specs visant le démon/supervisord/systemd ;
   components.toml hérite les permissions du root (0600 conseillé,
   comme daemon.sock).
4. **Persistance** : `components.toml` réécrit en `.part`+rename atomique
   (pattern `write_file`). `config.toml` n'est **pas** réécrit par le
   démon — deux fichiers, deux responsabilités.
5. **Windows** : le démon est Linux-only (socket Unix) — le modèle ne
   l'interdit pas mais le `process` management Windows est hors scope.
6. **Charge** : un superviseur de plus dans l'event-loop démon — borné :
   healthchecks ~30 s, spawns rares. Aucune dep Rust nouvelle requise
   (`toml`, `ureq` déjà présents).
7. **Orphans/split-brain** : un composant externe qui garde un état
   (orchestrateur : son ledger + flock) doit rester **singleton** — le
   flock existant du script couvre déjà les doublons ; la migration
   retire la conf supervisord pour éviter deux superviseurs qui le
   relancent (flock = second instance exit propre, safe).

---

## 4. Plan de découpage — items AgentBacklog

Ordre : A (cœur démon) → B (édition+catalogue) → {C, D, E, F, G}.

| # | Item backlog | Contenu | Dépend de |
| --- | --- | --- | --- |
| A | **#24** | daemon — modèle de composants + superviseur + RPC lecture (`ListComponents/ComponentInfo/Start/Stop/Restart/Logs`, components.toml, logs, healthcheck, capability, **champ `created_by` d'attribution** §2.3.1) | — |
| B | **#25** | daemon — édition de config + catalogue + install (`ComponentUpsert/Remove/Catalog/Install`, fetch GitHub, recettes agentbacklog + orchestrateur) | #24 |
| C | **#27** | acctl — commandes `comp` (ls/info/start/stop/restart/logs/catalog/install) | #25 |
| D | **#26** | client — vue diagramme animée SVG (nœuds programmes+sessions via `created_by`, états, flux) | #24 |
| E | **#28** | client — panneau gestion composants (liste, actions, logs, édition spec, onglet catalogue/intégration) | #25 |
| F | **#29** | orchestrateur — migration supervisord → composant managé (spec, doc migration, retrait conf après bascule) | #25 |
| G | **#30** | kind `tool` — détection/version + nœuds passifs catalogue (agentbacklog dans le diagramme) | #25 |

#24 est poppable immédiatement ; #26 dès que #24 est `done` ; les
autres se débloquent quand #25 passe `done` — le `--depends-on`
d'AgentBacklog garantit l'ordre de prise en charge entre workers.

### Critères de prise en charge

Chaque item est autonome : worktree dédié basé sur `origin/next_release`,
spec complète dans sa description backlog, PR draft → `/mr review` →
ready → les checks requis verts (5 requis par le ruleset sur les 6 runs
du workflow, `client-macos` en attente d'inclusion — gating sur les
check-runs du head SHA, workflow AGENTS.md). Le champ `description`
embarque : fichiers concernés, schéma exact des types/reqs, vérifs
(`cargo build --release -p agentcontrol-daemon` / `npm run build`),
et interdit de toucher au démon live (tests sur `AGENTCONTROL_HOME`
jetable).
