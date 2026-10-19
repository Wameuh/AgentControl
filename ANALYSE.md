# AgentControl — Analyse & questions de conception

> Document de travail : analyse du besoin, options d'architecture, mécanismes
> techniques et questions ouvertes avec recommandations.
> Date : 2026-10-18 — **V1 implémentée** (voir §10 État d'implémentation)

---

## 1. Reformulation du besoin

- **Application cliente** (Ubuntu + Windows) qui pilote des **agents IA** tournant
  sur un **serveur Ubuntu distant via SSH**.
- Les agents tournent en **mode détaché** : fermer l'appli ne les tue pas, ils
  continuent sur le serveur.
- Le client peut se **reconnecter** à une session en cours, revoir **tout
  l'historique** (scrollback paginé, pas tout en RAM), et **déplier/replier**
  les blocs (commandes CLI, sorties, diffs…).
- Contrainte forte : **léger en RAM**, fluide.

C'est en gros « un client lourd léger » + « un multiplexeur de sessions type
tmux, mais pour des agents » + « un journal d'événements rejouable ».

---

## 2. Constat clé : ce qui existe déjà

Avant de choisir une techno, il faut savoir que les CLI d'agents modernes
exposent déjà des interfaces machine-exploitables :

| Mécanisme | Ce que ça fait | Intérêt pour nous |
| --- | --- | --- |
| **ACP** (`devin acp`, Claude Code, Gemini CLI…) | L'agent parle **JSON-RPC sur stdio** : sessions, streaming de messages, `tool_call` structurés, demandes de permission | C'est **le** protocole prévu pour brancher une GUI sur un agent (c'est ce que font Zed/JetBrains). Les blocs "expandable" (tool calls, terminal output) sont natifs dans le protocole. |
| `devin -p` / `--print` | Mode non-interactif one-shot | Permet un MVP très simple : un process par message. |
| `devin -c` / `-r <id>`, `devin list --format json` | Reprise et listage de sessions persistées **sur disque côté serveur** | L'historique « vrai » vit déjà côté serveur ; le client n'a pas besoin d'être la source de vérité. |
| `--export` (format **ATIF**) | Exporte la conversation en JSON après chaque tour | Permet de rejouer/afficher l'historique sans scraper le terminal. |
| Modes de permission (`accept-edits`, `smart`, `bypass`, `autonomous`+sandbox) | Contrôlent ce que l'agent peut faire sans demander | Crucial pour un agent **détaché** : personne n'est là pour approuver → choix de mode à gérer explicitement. |
| `devin --cloud` | Fait déjà « agent distant qui survit à la fermeture » | …mais sur le cloud Cognition, pas sur **votre** serveur. C'est le benchmark fonctionnel, pas la solution. |

**Conséquence** : on n'a pas besoin d'écrire un émulateur de terminal ni de
parser de la sortie brute si on s'appuie sur ACP ou sur des exports structurés.
La difficulté réelle se déplace sur : **garder le process agent vivant quand le
client se déconnecte, et pouvoir se ré-attacher proprement.**

---

## 3. Le problème central : le « detach »

ACP parle sur stdio → c'est lié **à un process et une connexion**. Si le client
part, le process meurt (SIGHUP / socket fermé). Trois stratégies :

### Option A — Démon/serveur de session ✅ **choisie**

Un petit démon tourne en permanence sur le serveur (`agentcontrol-daemon`).
C'est lui qui :

- spawn les agents (`devin acp`, `claude`, shell…) comme **process enfants**,
- tient la connexion ACP/stdio avec eux,
- journalise chaque événement dans un **log append-only par session**
  (`~/.agentcontrol/sessions/<id>/events.jsonl` + blobs pour les grosses sorties),
- expose une API au client : `list_sessions`, `get_events(id, from_seq, limit)`,
  `subscribe(id, last_seen_seq)` → stream live, `send_message`, `interrupt`,
  `approve_permission`…

Le client n'est qu'une **vue** : il peut partir, revenir, demander les N
derniers événements puis s'abonner au live avec un numéro de séquence →
**zéro trou dans l'historique**, reconnexion propre.

C'est le pattern « tmux/dtach pour agents » ou « serveur ACP multiplexé ».

- Transport client↔démon :
  - **via tunnel SSH** (`ssh -L`, ou canal direct dans la lib SSH du client) :
    zéro port ouvert, auth = SSH existante. *Recommandé pour un usage perso.*
  - WebSocket+TLS direct : nécessaire seulement si multi-machine client sans SSH
    ou appli web.

### Option B — tmux/dtach + scraping

Lancer l'agent dans `tmux new-session -d`, récupérer la sortie via
`tmux pipe-pane` vers un fichier ou `capture-pane`.

- ✅ Marche avec **n'importe quel CLI**, même sans mode machine.
- ❌ Historique = texte terminal brut : expand/collapse, statuts de tool calls,
  diffs → il faut parser de l'ANSI, fragile. Pas de réponse interactive aux
  prompts de permission propre.
- Bon en **fallback** pour les agents non-ACP, pas en architecture principale.

### Option C — Process one-shot par message (MVP ultra simple)

`devin -p "msg" --export events.json` et `devin -r <id> -p "msg suivant"`.

- ✅ Survit à tout (état sur disque), zéro démon.
- ❌ Pas de streaming temps réel (il faut poller le fichier d'export), latence de
  démarrage du process à chaque message, pas d'interruption propre.
- Peut servir de **v0 jetable** pour valider l'UI, mais pas cible.

**Décision : Option A**, en gardant B comme mode « agent générique » futur.

---

## 4. Technologie du client

| Choix | RAM / poids | Pour | Contre |
| --- | --- | --- | --- |
| **Tauri** (Rust + webview système) | ~30–80 MB | Cross-plat Ubuntu/Windows, UI web moderne (virtual list, markdown, diff), backend Rust pour SSH+protocole | Webview système = rendu légèrement variable selon OS |
| Electron | 150–300+ MB | Écosystème énorme | **Contraire au cahier des charges RAM** |
| Qt (C++/PySide) | ~50–100 MB | Natif, solide | Dev plus lourd, licence PySide |
| **TUI** (Ratatui/Textual/Bubbletea) | ~10–30 MB | Le plus léger, tourne même en SSH | Pas de vraie GUI, courbes d'apprentissage UX (expand, scrollback, souris) |
| **Appli web servie par le démon** | ~0 côté install | Le client = navigateur, accessible depuis n'importe où | Nécessite TLS/auth correcte si exposée ; moins « desktop » |

**Décision : Tauri.** UI riche (liste virtualisée, markdown, blocs
pliables, diffs), binaire unique, léger, et la partie SSH/protocole vit dans le
backend Rust (crates `russh`/`ssh2`, `tokio`).
Option TUI envisageable en v2 « lite ».
L'option web devient intéressante si vous voulez y accéder depuis un téléphone.

---

## 5. Modèle de données : l'event sourcing

C'est **le** mécanisme qui rend tout le reste simple (scrollback, expand,
reconnexion, rejeu).

Chaque session = un journal append-only côté serveur :

```jsonl
{"seq": 1, "ts": "…", "type": "user_message", "text": "…"}
{"seq": 2, "ts": "…", "type": "agent_text", "text": "…", "streaming": true}
{"seq": 3, "ts": "…", "type": "tool_call", "id": "tc1", "kind": "exec",
  "title": "cargo test", "status": "running"}
{"seq": 4, "ts": "…", "type": "tool_output", "call_id": "tc1",
  "blob": "blobs/tc1.out", "size": 184230, "exit_code": 0}
{"seq": 5, "ts": "…", "type": "permission_request", "call_id": "tc2", …}
```

- Les **gros outputs** → fichiers blob à part (`blobs/<id>`), l'événement ne
  garde qu'un résumé + pointeur → *expand = fetch du blob à la demande*.
- `seq` monotone → le client dit « donne-moi tout après seq 4821 ».
- En ACP, ce mapping est quasi direct : `session/update` notifications →
  événements.

Le client maintient une **fenêtre** : les ~200 derniers événements en RAM ;
scroller vers le haut déclenche `get_events(before_seq)` par pages. Jamais tout
l'historique en mémoire → exigence RAM tenue même sur des sessions de 100 000
lignes.

---

## 6. Questions à trancher

### Q1 — Quels agents doit-on supporter ?

- **a) Un seul** (ex. Devin CLI via `devin acp`) → plus simple, UX optimisée.
- **b) Plusieurs agents ACP** (Devin, Claude Code, Gemini, Codex…) → même
  protocole, effort marginal faible, grosse valeur.
- **c) Shell générique** (juste un terminal détaché) → utile en bonus via tmux.
- **Reco : b)** — ACP unifie ; prévoir l'adaptateur dès le départ mais n'en
  implémenter qu'un (Devin) en MVP.
- ✅ **Décision : b) multi-agents ACP.** Conséquence : il faut une couche
  d'abstraction « profil d'agent » pour les modes/commandes hétérogènes —
  voir §6.1.

### Q2 — Où vit le démon et comment le client s'y connecte ?

- **a) SSH tunnel uniquement** — le client embarque une connexion SSH (clé de
  `~/.ssh` ou agent), ouvre un canal vers le socket du démon. Zéro config
  réseau, sécurité = SSH. **Reco pour v1.**
- b) Port TCP + TLS + token — requis pour web/multi-device, à ajouter plus tard.
- c) Le client *déploie et démarre* le démon via SSH s'il est absent
  (auto-bootstrap) → **très recommandé** : `scp` du binaire +
  `systemd --user` ou `nohup`.
- ✅ **Décision : a** (+ c auto-bootstrap). b reste possible en v2 — le protocole
  démon↔client doit donc être transport-agnostique (même format de messages
  sur canal SSH ou WebSocket).

### Q3 — Comment gérer les permissions d'un agent détaché ?

Personne devant l'écran quand l'agent demande « puis-je exécuter `rm` ? » :

- **a)** Le client relaie la demande → notification + l'utilisateur répond quand
  il revient (l'agent attend, c'est OK, il ne meurt pas).
- **b)** Mode `accept-edits`/`smart`/`bypass` lancé dès le départ selon le
  niveau de confiance choisi **par session**.
- **c)** Règles d'auto-approbation côté démon (allowlist de commandes).
- **Reco : a + b** — choix du mode à la création de session ; les demandes
  restantes sont poussées au client et **persistent** dans le journal (visible
  et répondable après reconnexion).
- ✅ **Décision : a + b, avec changement de mode en cours de session.**
  C'est supporté nativement : ACP expose `session/set_mode`, et les CLI ont leurs
  slash commands (`/mode`, `/plan`, `/bypass`…). Le démon doit exposer
  `set_mode(session, mode)` et le client proposer un sélecteur de mode **par
  session**, alimenté par les modes *déclarés par l'agent* (pas une liste en
  dur). Voir §6.1.

### Q4 — Format d'interface : GUI, TUI ou web ?

Voir §4. ✅ **Décision : GUI Tauri.**

### Q5 — Un seul serveur ou plusieurs ?

- a) Un serveur configuré — v1.
- b) Multi-serveurs dans une sidebar — prévoir le modèle de données
  (`server_id` partout) dès maintenant, implémenter plus tard. **Reco : b en
  conception, a en code.**
- ✅ **Décision : a en v1, b non verrouillé** — `server_id` dans tous les
  schémas/config dès le départ.

### Q6 — Notifications quand l'appli est fermée ?

Un agent qui finit à 3h du mat'…

- a) Rien — l'état est visible à la réouverture (badge « terminé/en attente »).
- b) Notification locale si appli ouverte en tâche de fond.
- c) Webhook (ntfy, mail, Matrix…) déclenché par le démon.
- **Reco : a puis c** (ntfy.sh = 20 lignes côté démon).
- ✅ **Décision : a puis c** — badges d'état v1, webhook ntfy en v1.x.

### Q7 — Faut-il voir les diffs / fichiers modifiés ?

- a) Non, juste le chat + tool calls.
- b) Vue diff par tool call d'édition (ACP fournit `diff` dans les updates).
- c) Explorateur de fichiers du workspace distant + git status.
- **Reco : b** natif via ACP ; c en v2.
- ✅ **Décision : b** — diff par tool call d'édition.

### Q8 — Sécurité : que peut toucher l'agent sur le serveur ?

- a) Utilisateur dédié `agent` sur le serveur (pas votre user principal).
- b) Workspaces confinés : un dossier racine par session, `bwrap`/sandbox.
- c) Conteneur/VM par session (lourd).
- **Reco : a + b** — Devin CLI a déjà un mode `--sandbox --permission-mode
  autonomous` (bubblewrap) exploitable sous Linux.
- ✅ **Décision : a + b, avec option bypass** — utilisateur `agent` dédié +
  workspaces confinés ; `bypass`/`dangerous` reste sélectionnable par session
  pour ceux qui acceptent le risque.

### Q9 — Historique : source de vérité ?

- a) Le journal du démon (events.jsonl) — contrôle total sur le format.
- b) Le store natif de l'agent (`devin -r`) — dédoublonnage zéro mais format
  non garanti + dépend du CLI.
- **Reco : a comme vérité d'affichage, b comme vérité de reprise** : le journal
  sert à l'UI, `devin -r <id>` sert à faire continuer l'agent.
- ✅ **Décision : a** — le journal du démon est la source de vérité
  d'affichage. Les IDs de session natifs de l'agent sont stockés dans les
  métadonnées pour permettre `session/load` / `-r` à la reprise.

### Q10 — Quelle granularité d'historique ?

- Garder **brut** (tous les chunks) ou **compacté** (un message = un event) ?
- **Reco : compacté** dans le journal + raw ACP archivé en debug — le journal
  reste petit, scrollable, et le raw sert au diagnostic.
- ✅ **Décision : configurable** — `journal.detail = compact | raw` dans la
  config du démon, défaut `compact`. Le mode `raw` garde chaque chunk de
  streaming (utile debug, ~10× plus volumineux).

### Q11 — Multi-utilisateur ?

- Mono-utilisateur mono-machine : v1. Le multi-user change tout (auth, isolation
  des sockets, quotas) → **hors scope v1**, mais ne pas le verrouiller.
- ✅ **Décision : hors scope, non verrouillé** — le démon tourne sous un user
  Unix ; le socket Unix a naturellement les permissions du user → l'isolation
  multi-user viendrait « gratuitement » d'une instance par user.

### Q12 — Récupération après crash / reboot du serveur ?

- a) systemd `--user` avec `Restart=always` pour le démon ; les agents
  redémarrent via `-r` (reprise de session) ou sont marqués « interrompus ».
- b) Rien de spécial, sessions marquées orphelines, reprise manuelle.
- **Reco : a pour le démon, b pour les agents** (reprendre un agent au milieu
  d'une action est de toute façon risqué).
- ✅ **Décision : a pour le démon** — unité `systemd --user`
  `Restart=always` installée par l'auto-bootstrap. Agents : marqués
  « interrompus » au redémarrage, reprise manuelle via `session/load` / `-r`.

### 6.1 Adaptateur d'agents : gérer l'hétérogénéité des modes/commandes

Chaque agent ACP a ses propres modes, slash commands et conventions (Devin :
`/mode`, `/plan`, `/ask`, `/model`, Shift+Tab ; Claude Code : autres noms,
autre cycle…). On ne peut pas coder ça en dur → le démon maintient un
**descripteur d'agent** rempli à la connexion :

```rust
struct AgentProfile {
    id: String,                 // "devin", "claude-code", …
    display_name: String,
    spawn_cmd: Vec<String>,     // ["devin", "acp", "--model", "{model}"]
    resume: ResumeStrategy,     // SessionLoad | SpawnWithFlag("--resume", id)
    capabilities: Capabilities, // découvertes via `initialize` ACP
}
```

L'essentiel est **découvert dynamiquement** via le protocole, pas configuré :

| Besoin UI | Mécanisme ACP | Fallback |
| --- | --- | --- |
| Sélecteur de mode par session | `session/set_mode` + `sessionInfo.modes` renvoyés par l'agent | Envoyer la slash command équivalente (`/mode bypass`) comme message |
| Liste des commandes dispo | `available_commands` dans les notifications `session/update` (Devin les annonce : `/plan`, `/ask`, `/compact`…) | Fichier de profil statique par agent |
| Changement de modèle | `session/set_model` si supporté | Slash command `/model opus` |
| Interruption | `session/cancel` | SIGINT au process |
| Reprise de session | `session/load` (confirmé dans `devin acp`) | Respawn avec `-r <id>` |

→ Le client UI est **agnostique** : il affiche les modes/commandes que le démon
lui transmet. Ajouter un nouvel agent = ajouter un `AgentProfile` (10 lignes de
TOML/JSON), zéro code UI.

### 6.2 Quand l'agent pose une question en cours de route

Deux cas distincts, à traiter différemment :

**a) Demande de permission** (« puis-je exécuter `npm install` ? »)
→ ACP `session/request_permission` avec options (`allow_once`, `allow_always`,
`reject`…). Le démon :
1. journalise un event `permission_request` (persistant),
2. le pousse au client s'il est connecté (notification + inline dans le chat),
3. **le garde « pending » sinon** → répondable après reconnexion, l'agent
   attend côté serveur sans mourir,
4. applique les règles d'auto-approbation du démon si configurées (allowlist).

**b) Question ouverte de l'agent** (`ask_user_question`, « quelle branche
viser ? ») → Devin CLI utilise l'**elicitation ACP standard** (confirmé dans le
changelog). Le client doit annoncer la capability `elicitation` à
l'`initialize`, et le démon relaie pareil : event `question_request` avec
schéma de réponse (choix multiples ou texte libre), persistant tant que non
répondu.

Points d'attention :
- **Timeout configurable** : `question.timeout = never | 1h | …` ; option
  « timeout → réponse par défaut » ou « timeout → passer en mode bypass ».
- Une question pending **bloque le tour en cours** : l'UI doit la rendre
  très visible (badge rouge sur la session, notification webhook ntfy en v1.x).
- Réponse d'un autre device / reconnect : l'event `question_answered` est
  journalisé avec la réponse → tous les clients voient la résolution.

### 6.3 Stockage : où et combien ça pèse côté serveur

Tout vit sous `~/.agentcontrol/` (user `agent` dédié) :

```
~/.agentcontrol/
├── daemon.sock                  # socket Unix (accès via tunnel SSH)
├── config.toml
└── sessions/<session_id>/
    ├── meta.json                # titre, agent, workspace, native_session_id
    ├── events.jsonl             # journal (vérité d'affichage)
    └── blobs/<event_id>         # outputs volumineux, un fichier par tool call
```

> Implémenté : pas de SQLite finalement — `meta.json` + index **en mémoire**
> seq→offset (~16 o/evt, reconstruit au démarrage) suffisent : les lectures
> d'historique sont en seek direct même sur des journaux de plusieurs Mo.
> La rétention est appliquée par un janitor (voir `[retention]` de la config) :
> compaction du journal au-delà d'un seuil, purge LRU des blobs, purge
> optionnelle des sessions inactives.

Ordres de grandeur (session de dev intense, ~8h) :

| Donnée | Taille typique | Notes |
| --- | --- | --- |
| events.jsonl | 1–5 MB, plafonné par `journal_max_mb` | messages + métadonnées de tool calls |
| blobs/ | plafonné par `blob_max_mb` (LRU) | dominé par les sorties de build/tests |

**Leviers de légèreté (implémentés)** :
- Externalisation : sortie > 32 KB → blob, l'event garde un extrait de 4 KB.
- Janitor : compaction `events.jsonl` > 64 MB → garde 32 MB de queue ;
  blobs > 512 MB/session → purge LRU ; sessions inactives → `purge_days` (off par défaut).
- Index mémoire seq→offset : historique en O(1), ~16 o/evt seulement.
- Pas de copie de l'historique natif de l'agent : on ne stocke que l'ID.

→ Budget réaliste : **< 600 MB par session** au pire (borné par les quotas),
typiquement quelques dizaines de MB par mois.

---

## 7. Découpage MVP → v1

| Étape | Contenu | Valide |
| --- | --- | --- |
| **MVP0** | Démon Rust minimal : spawn `devin acp`, journal JSONL, socket Unix. Client = script CLI `send`/`tail`. | detach + journal + reconnexion |
| **MVP1** | Client Tauri : liste sessions, chat live, scrollback paginé, blocs tool call pliables | l'UX cœur |
| **v1** | Multi-agents ACP, permissions relayées, auto-bootstrap SSH du démon, notifications ntfy | le produit annoncé |
| **v2** | Vue diff/fichiers, multi-serveurs, mode TUI, web optionnel | extensions |

---

## 8. Risques & points d'attention

1. ~~**Cycle de vie ACP** : `session/load`~~ — ✅ levé : `devin acp` supporte
   `session/load` et restaure titres/modes/usage à la reprise (changelog CLI).
   Le spike MVP0 reste utile pour valider la latence et le flux d'events.
2. **Backpressure** : un agent peut produire des Mo de sortie (build logs) →
   tronquer les blobs, échantillonner le streaming, ne jamais pousser tout le
   flux au client.
3. **SSH depuis Windows** : utiliser une lib (`russh`) plutôt que `ssh.exe` —
   clés, agent Pageant/ssh-agent, keepalive, reconnexion automatique.
4. **Un seul writer par session** : si deux clients s'abonnent et envoient,
   arbitrer côté démon (ou accepter — vous êtes seul).
5. **Taille du journal** : rotation/compaction des events.jsonl, purge des
   blobs > X jours.

---

## 9. Résumé exécutif

> **Démon Rust `agentcontrol-daemon` sur le serveur** (spawne et possède les
> agents ACP, journalise tout en JSONL+blobs sous `~/.agentcontrol/`) **+
> client Tauri** qui se connecte via **tunnel SSH**, affiche une fenêtre
> paginée du journal et s'abonne au flux live par numéro de séquence.
> Multi-agents via des **profils déclaratifs** + découverte ACP (modes,
> commandes, elicitation). Permissions/questions **persistantes** : répondables
> après reconnexion, l'agent attend côté serveur. C'est le seul design qui
> satisfait simultanément : detach réel, reconnexion sans perte, historique
> complet sans RAM excessive, blocs expandables natifs, et stockage serveur
> contenu (< 200 MB/mois, purgable).

---

## 10. État d'implémentation (2026-10-19)

Livré et **testé end-to-end** (pas de tests unitaires, vérification réelle) :

| Composant | État |
| --- | --- |
| `crates/daemon` | ✅ socket Unix + TCP loopback, spawn ACP, journal JSONL indexé (seq→offset) + blobs, subscribe/catch-up, permissions/elicitation persistants, `session/load`, dedupe replay, janitor rétention (compaction/purge), systemd |
| `crates/client-core` | ✅ transports unix/tcp/`ssh -L`, calls + subscribe dédié, timeout 60 s |
| `crates/mock-agent` | ✅ scénarios : chunks, tool call, perm, elicitation, blob > 32 KB, cancel |
| `crates/acctl` | ✅ CLI complet (utilisé pour tous les tests e2e) |
| `client/` Tauri | ✅ code complet (backend commands + frontend TS, build vite OK **11 KB JS**) — non lancé ici (webkit2gtk absent) |
| `packaging/` | ✅ unit systemd `--user` + `deploy.sh` (scp + enable + linger) |

Vérifié en e2e : **agent Devin réel** (`devin acp`) — handshake, modes, slash
commands remontées, pensées/réponses streamées, `session/load` après restart ;
**agent mock** — permissions, elicitation, blobs, cancel ; démon ≈ **6,5 MB RSS** ;
journal de 20 000 events lu en **7 ms** au milieu du fichier ; compaction
3,2 MB → 1 MB ; purge blobs LRU ; purge session inactive (`purge_days`).

Reste pour v1.x+ : webview Tauri à tester sur machine dev, notifications ntfy,
auto-bootstrap du démon depuis le client, allowlist d'auto-approbation,
vue diff enrichie, multi-serveurs.
