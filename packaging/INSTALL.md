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
