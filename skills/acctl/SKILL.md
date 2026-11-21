---
name: acctl
description: Piloter le démon AgentControl — sessions (new/spawn/msg/send/rm), fichiers, registre PR, RPC brut pour ce qu'acctl n'expose pas (groupe, rename, file)
argument-hint: "<commande acctl ou RPC brut>"
allowed-tools:
  - exec
---

# acctl — pilotage du démon AgentControl

`acctl` est le CLI du démon : créer/lister/supprimer des sessions, envoyer des
messages (utilisateur ou agent→agent), lire/écrire des fichiers dans le
workspace d'une session, gérer le registre PR.

## Connexion

- `AGENTCONTROL_TCP=hôte:port` → socket TCP (ici le démon du conteneur écoute
  `127.0.0.1:7331`)
- sinon socket Unix `~/.agentcontrol/daemon.sock`
- `AGENTCONTROL_SESSION` = id de la session courante (injecté quand on tourne
  dans une session AgentControl — utile comme `from` pour `spawn`/`msg`)
- Bruit stderr : les binaires des releases ≤ 0.10.4 peuvent imprimer un
  warning `GLIBC_2.39 not found` inoffensif (liés à la glibc du runner
  CI — corrigé en statique musl depuis #266) → ajouter `2>/dev/null`
  aux pipes si le warning apparaît.

## Commandes

| Commande | Rôle |
|---|---|
| `acctl status` | version du démon, nb sessions, self_update |
| `acctl agents` | profils d'agents disponibles |
| `acctl list` | sessions (état, agent, titre, badges `[#N]`, sous-agents indentés `↳`) |
| `acctl new <agent> <cwd> [prompt...] [--title T] [--mode M] [--model X]` | crée une session **racine** (pas de `spawned_by`, pas de groupe) |
| `acctl spawn <from> [task...] [--agent A] [--cwd C] [--title T] [--mode M] [--model X] [--group G]` | crée une **sous-session** de `from` : `spawned_by=from`, imbriquée dans la sidebar ; champs omis hérités du parent ; `--group` explicite possible ; `task` livré en message PeerMessage traçé |
| `acctl send <session> [--steer|--force] <text...>` | message utilisateur → session ; `--steer` injecte pendant le tour, `--force` passe en tête de file ; session endormie → réveil auto |
| `acctl msg <from> <to> [--steer] <text...>` | message agent→agent (PeerMessage journalisé des deux côtés) |
| `acctl events <session> [before_seq] [limit]` | journal d'events formaté |
| `acctl tail <session> [after_seq]` | suit le journal en continu |
| `acctl search <session> <query...>` | plein-texte dans le journal (~20k events/scan) |
| `acctl find <session> <query...>` | fichiers par nom dans le workspace |
| `acctl create <session> <path> [--dir]` | crée fichier/dossier (exclusif : échoue si existe) |
| `acctl write|wr <session> <path> [fichier|-]` | écrit un fichier (stdin si `-` ou absent) |
| `acctl download|dl <session> <path> [dest]` | rapatrie un fichier/dossier |
| `acctl blob <session> <id> [offset] [limit]` | lit un blob du journal |
| `acctl resume <session> [native_id]` | réveille/respawn une session |
| `acctl attach <agent> <cwd> <native_id> [title]` | attache une session native de l'agent |
| `acctl fork <session>` | fork la session native → nouvelle session même groupe, titre `_fork` |
| `acctl sleep <session>` | endort (tue le process, garde le journal — le prochain send réveille) |
| `acctl cancel <session>` | annule le tour en cours |
| `acctl kill <session>` | tue l'agent sans supprimer la session |
| `acctl clear <session>` | vide le journal (`/clear`) |
| `acctl rm <session>` | **supprime la session définitivement** |
| `acctl mode <session> <mode>` / `acctl model <s> <m>` / `acctl setcfg <s> <id> <val>` | réglages runtime de l'agent |
| `acctl respond <session> <request_id> [option_id | --answer JSON]` | répond à une question/permission en attente |
| `acctl daemon-update [version]` | auto-update du démon (défaut : version d'acctl) |
| `acctl pr ls|add|note|rm|sync` | registre PR ↔ session (voir règle globale registre PR) |

## Groupes

- **Créer groupé** : `acctl spawn <parent> --group "a/b" ...` (sous-session)
  ou rien pour `new` (racine). Un spawn sans `--group` hérite le groupe du
  parent.
- **Re-grouper une session existante** : `acctl` n'expose pas
  `set_session_group` → RPC brut (ci-dessous). `group: null` retire du groupe.
- Les noms avec `/` rendent l'arborescence (sous-groupes) dans la sidebar.

## RPC brut (ce qu'acctl n'expose pas)

Protocole NDJSON sur le socket : `{"id":1,"req":"<snake_case>",...}` →
`{"id":1,"ok":true,"result":...}`. Helper TCP :

```bash
ac_rpc() { python3 - "$@" <<'EOF'
import json, socket, sys
s = socket.create_connection(("127.0.0.1", 7331), timeout=10)
f = s.makefile("rw")
for line in sys.argv[1:]:
    f.write(line + "\n"); f.flush()
    print(f.readline().strip())
EOF
}
ac_rpc '{"id":1,"req":"set_session_group","session":"s-xxx","group":"a/b"}'
```

Requêtes utiles hors acctl :

| req | effet |
|---|---|
| `set_session_group {session, group\|null}` | grouper/dégrouper |
| `rename_session {session, title}` | renommer |
| `session_info {session}` | méta complète (modes/models/commands dispo) |
| `list_sessions` | méta JSON complète (group, spawned_by, context_used…) |
| `queue_list / queue_edit / queue_drop / queue_promote {session, ...}` | file de messages en attente |
| `list_dir {session, path}` / `delete_path {session, path}` | fichiers |
| `list_native_sessions {session}` | sessions natives reprennables de l'agent |
| `term_open/term_input/term_resize/term_close` | PTY terminal (usage client) |

Référence complète : `ReqKind` dans `crates/proto/src/lib.rs` (~l. 430).

## Garde-fous

- **Jamais** stopper/redémarrer le démon lui-même (`systemctl`,
  `supervisorctl`, kill du process) — il orchestre toutes les sessions
  agents, dont la vôtre (règle AGENTS.md).
- `rm` est définitif ; préférer `sleep` pour libérer une session qu'on
  peut reprendre.
- `spawn` exige un `from` = id de session existante ; `kill` sur une
  session `dead`/`starting` est inoffensif.
- Visibilité client : toute session créée ici apparaît dans la sidebar au
  prochain refresh — imbriquée sous le parent si `spawned_by` est posé
  (`spawn`), racine sinon (`new`).
