# AgentControl — Installation

Archive : `agentcontrol-server-<arch>-linux.tar.gz`
Contenu : `agentcontrol-daemon` · `acctl` · `acp-mock-agent` · `packaging/` (unit systemd, config exemple)

## 1. Serveur (Ubuntu — où tournent les agents)

```bash
tar xzf agentcontrol-server-*.tar.gz
mkdir -p ~/.local/bin ~/.agentcontrol ~/.config/systemd/user
cp agentcontrol-daemon acctl acp-mock-agent ~/.local/bin/
cp packaging/agentcontrol-daemon.service ~/.config/systemd/user/
cp packaging/config.example.toml ~/.agentcontrol/config.toml

systemctl --user daemon-reload
systemctl --user enable --now agentcontrol-daemon
loginctl enable-linger $USER        # démon actif même sans session ouverte
systemctl --user status agentcontrol-daemon
```

Le démon écoute :
- `127.0.0.1:7331` (loopback → atteignable uniquement via tunnel SSH)
- `~/.agentcontrol/daemon.sock` (local)

Test local : `~/.local/bin/acctl status` → `{"agents":["devin","mock"],...}`

### Sans systemd (test)

```bash
AGENTCONTROL_HOME=~/.agentcontrol ~/.local/bin/agentcontrol-daemon
```

### Prérequis agents

- `devin` : installé + `devin auth login` sur le serveur (commande `devin acp`).
- Autres agents ACP : éditer `[agents.*]` dans `~/.agentcontrol/config.toml`.

## 2. Client (votre machine)

- **Ubuntu** : installer le `.deb`/binaire de l'artifact `agentcontrol-client-linux`
  (ou `cd client && npm i && npx tauri build` depuis le repo).
- **Windows** : installer `.msi`/`.exe` de l'artifact `agentcontrol-client-windows`.
- **macOS** : `.dmg` arm64 de l'artifact `agentcontrol-client-macos` — non
  signé, clic droit → **Ouvrir** au premier lancement (voir README).

### Connexion

Dialog « Connecter » → **SSH** :
- Hôte : `user@serveur` — les alias de `~/.ssh/config` sont proposés ;
  la clé SSH est requise (`BatchMode`, pas de prompt mot de passe →
  `ssh-copy-id user@serveur` si besoin).
- Port démon distant : `7331` (défaut).

Le client ouvre `ssh -N -L <local>:127.0.0.1:7331 <hôte>` automatiquement.

### Vérif rapide du tunnel (CLI)

```bash
ssh -N -L 7331:127.0.0.1:7331 user@serveur &
AGENTCONTROL_TCP=127.0.0.1:7331 acctl status   # → {"sessions":0,...}
```

## 3. Usage

- `+ Nouvelle session` → agent, `cwd` sur le serveur, mode/modèle.
- Fermer l'appli → **l'agent continue** sur le serveur. Rouvrir →
  historique complet (scrollback paginé), sessions `waiting_input`
  à reprendre, permissions/questions encore répondables.
- `/` dans le champ message → autocomplete des slash-commands de l'agent.

## 4. Rétention (config.toml)

```toml
[retention]
journal_max_mb = 64    # compaction events.jsonl
journal_keep_mb = 32
blob_max_mb = 512      # quota sorties de tool calls (LRU)
purge_days = 0         # purge sessions inactives (0=off)
interval_secs = 600
```

## 5. Composants managés — migrer un service supervisord

Le démon supervise lui-même les programmes satellites : `kind=process`
(spawn/stop/restart, healthcheck, logs paginés), `tool`, `external` —
modèle complet dans `docs/components-integration.md`. Les specs vivent
dans `~/.agentcontrol/components.toml`, fichier **possédé par le
démon** : on l'édite via RPC (`component_upsert`/`component_remove`),
jamais en l'ouvrant à chaud. Exemple commenté :
`packaging/components.example.toml`.

### Cas type : backlog-orchestrator

Service aujourd'hui sous `~/.supervisor/conf.d/backlog-orchestrator.conf`
(script `~/.local/bin/backlog-orchestrator.py`, poll 60 s, **singleton
garanti par flock** `~/.agentbacklog/orchestrator.lock`).

**Bascule** — poser la spec, retirer la conf supervisord, **démarrer le
composant**. Ni `component_install` ni `component_upsert` ne lancent le
process : `autostart` n'agit qu'au boot du démon — sans `component_start`
explicite, l'orchestrateur ne poll plus rien jusqu'au prochain restart.
Le flock rend la fenêtre de transition sans risque : tant que l'ancienne
instance tourne, toute seconde instance sort proprement.

```bash
# 1. Installer le composant depuis le catalogue embarqué (recette
#    déclarative : exige le script présent, pose la spec autostart +
#    restart=on-failure + healthcheck=process — état : stopped)
acctl comp install backlog-orchestrator   # acctl ≥ item #27

#    Sans `acctl comp` (version antérieure) — NDJSON brut sur le socket :
python3 - <<'EOF'
import json, os, socket
s = socket.socket(socket.AF_UNIX)
s.connect(os.path.expanduser("~/.agentcontrol/daemon.sock"))
f = s.makefile("rw")
for req in ({"id":1,"req":"component_catalog"},
            {"id":2,"req":"component_install","name":"backlog-orchestrator"}):
    f.write(json.dumps(req)+"\n"); f.flush(); print(f.readline().strip())
EOF

# 2. Retirer la supervision historique — l'instance supervisord s'arrête
#    et libère le flock
rm ~/.supervisor/conf.d/backlog-orchestrator.conf
supervisorctl reread && supervisorctl update
supervisorctl status                      # backlog-orchestrator absent

# 3. Démarrer le composant — le flock est libre, le managé prend le relais
acctl comp start backlog-orchestrator     # ou le RPC ci-dessous
python3 - <<'EOF'
import json, os, socket
s = socket.socket(socket.AF_UNIX)
s.connect(os.path.expanduser("~/.agentcontrol/daemon.sock"))
f = s.makefile("rw")
f.write(json.dumps({"id":1,"req":"component_start","name":"backlog-orchestrator"})+"\n")
f.flush(); print(f.readline().strip())
EOF

# 4. Vérifier côté démon : running, logs qui défilent
acctl comp ls                             # ou component_info/list_components en RPC
acctl comp logs backlog-orchestrator
```

Variante « couverture continue » : démarrer le composant **avant** de
retirer la conf supervisord. L'instance managée échoue au flock et
`restart=on-failure` la retente (backoff ≤ 60 s) jusqu'à obtenir le
lock — aucun trou de couverture, au prix de retries « lock » dans le
log et d'un compteur `restarts` non nul.

**Rollback** : `component_remove` (stop + spec retirée), restaurer la
conf supervisord, `supervisorctl reread && supervisorctl update` —
là aussi sans doublon grâce au flock.

⚠️ Un `process` managé partage le destin du démon : SIGTERM à l'arrêt
du démon, respawn au boot si `autostart` — pour un service qui doit
survivre au restart démon, déclarer `kind=external` à la place
(observation seule).

### Versionner le script (recommandé)

La recette catalogue de `backlog-orchestrator` suppose le script déjà
présent à `~/.local/bin/` — il n'est versionné nulle part. Le déposer
dans **Wameuh/AgentBacklog** ou un `contrib/` d'AgentControl donnerait
une source canonique téléchargeable (recette `run`/`write_file` au lieu
de `require_file` + spec-only). En attendant, la recette reste
volontairement **spec-only quand le script est détecté** : rien à
installer, juste la spec à poser.
