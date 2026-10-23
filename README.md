# AgentControl

Client léger (Tauri) pour piloter des **agents ACP détachés** tournant sur un
serveur Ubuntu. Fermez l'appli : les agents continuent. Rouvrez : historique
complet, reprise des sessions, réponses aux permissions/questions en attente.

Voir `ANALYSE.md` pour la conception.

## Architecture

```
portable (Tauri)                serveur Ubuntu
┌─────────────────┐   SSH -L    ┌──────────────────────────────┐
│ client-core ────┼────────────►│ agentcontrol-daemon          │
│ UI chat/events  │  NDJSON     │  ├─ devin acp   (enfant)      │
└─────────────────┘  unix/tcp   │  ├─ claude …   (enfant)      │
                                │  └─ ~/.agentcontrol/sessions │
                                └──────────────────────────────┘
```

- `crates/daemon` — le démon (possède les process agents, journalise tout)
- `crates/proto` — protocole client↔démon + modèle d'événements
- `crates/client-core` — transports (unix/tcp/ssh -L) + client protocole
- `crates/mock-agent` — agent ACP factice pour tester sans abonnement
- `crates/acctl` — client CLI de test
- `client/` — app Tauri (frontend TS vanilla, ~11 KB JS)

## Build

```bash
cargo build --release          # démon + acctl + mock
```

## Démon (sur le serveur)

```bash
# directement
AGENTCONTROL_HOME=~/.agentcontrol ./agentcontrol-daemon

# ou déploiement systemd --user depuis votre poste :
./packaging/deploy.sh user@serveur
```

Écoute : `~/.agentcontrol/daemon.sock` + `127.0.0.1:7331` (loopback → tunnel SSH).
Agents configurés dans `~/.agentcontrol/config.toml` (voir `packaging/config.example.toml`).

## Client Tauri

```bash
cd client
npm install
npx tauri dev      # nécessite webkit2gtk-4.1 sous Ubuntu :
                   # sudo apt install libwebkit2gtk-4.1-dev
npx tauri build    # binaire natif
```

Connexion SSH : type « SSH », `user@serveur` — le client ouvre
`ssh -N -L <port>:127.0.0.1:7331` automatiquement (clés/agent ssh standard).

## Test CLI (acctl)

```bash
export AGENTCONTROL_SOCK=~/.agentcontrol/daemon.sock
acctl agents                          # agents configurés
acctl new devin ~/projet --title t    # nouvelle session
acctl list                            # sessions (état : idle/working/waiting_input/dead)
acctl send s-xxx "corrige les tests"  # envoyer un message
acctl tail s-xxx                      # suivre en live
acctl events s-xxx                    # paginer l'historique
acctl respond s-xxx pr-xxx allow_once # répondre à une permission
acctl mode s-xxx bypass               # changer de mode à chaud
acctl resume s-xxx                    # reprendre (session/load)
acctl kill s-xxx / cancel s-xxx
```

## État validé (e2e)

- spawn `devin acp` réel via le démon, prompts, chunks compactés, tool calls
- permission → `waiting_input` persistant → réponse après reconnexion
- elicitation (ask_user_question) relayée
- sorties > 32 KB externalisées en blob + relecture
- `set_mode` à chaud, `session/load` (reprise), restart démon → `interrupted` → resume
- subscribe live + rattrapage par seq

## Agent↔agent

Chaque session reçoit (via `mcpServers` ACP) un serveur MCP `agentcontrol`
exposant 3 outils : `agentcontrol_list_sessions` (découverte des pairs),
`agentcontrol_send_message` (`delivery: "queue"` = fin de tâche, `"steer"` =
injection après le tool call courant si l'agent le supporte) et
`agentcontrol_read_events` (transcript d'un pair).

Le message est journalisé `peer_message` dans les deux sessions (rendu
dédié côté client, cliquable vers la session pair) et livré à la cible
comme prompt préfixé de la provenance. Une session morte est relancée
automatiquement. Anti-boucle : `rate_per_min` (20/min/paire, section
`[a2a]` de config.toml — `enabled = false` pour tout couper).

Fallback sans MCP : `AGENTCONTROL_SOCK`/`AGENTCONTROL_SESSION` sont dans
l'env de l'agent → `acctl msg <from> <to> [--steer] <texte>`.
