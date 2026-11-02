---
name: ac-wait
description: Déléguer une attente à un waiter d'arrière-plan qui poll à bas bruit et ping la session via acctl send quand la condition tombe — l'agent rend la main au lieu de boucler
argument-hint: "[opts] -- <cmd...> | --until-pid|--until-file|--until-port|--until-pid-file | --list|--cancel|--log"
allowed-tools:
  - exec
---

# ac-wait — waiters délégués

`ac-wait` spawn un poller détaché (setsid+nohup) qui surveille une condition
et envoie le résultat à ta session via `acctl send` — le message arrive comme
un message normal (la session est réveillée si elle dort). Tu rends la main
immédiatement après le spawn au lieu de brûler des tours à boucler.

Script : `tools/ac-wait` dans le repo AgentControl, installé
`~/.local/bin/ac-wait` (symlink). Deps : bash + `acctl` uniquement
(`AGENTCONTROL_TCP` respecté → marche aussi depuis un hôte distant).

## QUAND l'utiliser

Règle d'or : **si tu comptes lancer `X --watch` ou une boucle
`while ! cond; sleep` dans un tool call → ac-wait à la place.**

- attendre les checks CI d'une PR (`gh pr checks <N>` en boucle)
- attendre le merge d'une PR (`gh pr view <N> --json state`)
- attendre qu'un lock/flock se libère, qu'un item backlog passe done
- attendre un build long, la mort d'un process, un port/démon qui monte
- attendre une condition distante via ssh (`-- ssh host test ...`)
- dormir N minutes (`-- sleep 600` → ping au réveil)

## PROTOCOLE

1. **Spawn** — retourne immédiatement un wait-id :

   ```bash
   ac-wait --name "checks PR #123" -- gh pr checks 123 --required
   # → w-1730000000-12345
   ```

2. **Continue** — fais autre chose ou termine ton tour
   (« en attente du ping ac-wait »). Pour dormir vraiment :
   `acctl sleep "$AGENTCONTROL_SESSION"` — le ping du waiter réveille la
   session automatiquement.

3. **Ping** — `[ac-wait] <name> : OK|TIMEOUT après <durée>` arrive en file +
   les dernières lignes de sortie de la commande. Reprends là où c'était.

La condition est réévaluée toutes les `--interval` s (défaut 30) jusqu'à
exit 0 ; `--timeout` (défaut 2 h) → message TIMEOUT. Le message est borné
(~2 Ko, tail des dernières sorties).

## Exemples

```bash
ac-wait --name "CI #123" --interval 60 -- gh pr checks 123 --required
ac-wait --name "merge #123" -- gh pr view 123 --json state -q .state | grep -q MERGED
#  ^ astuce : la condition = « la commande exit 0 » — pipe/filtre librement
ac-wait --until-file /tmp/build.done --name "build fini"
ac-wait --until-pid 4207 --name "mort du batch"          # pid déjà mort → OK direct
ac-wait --until-port 127.0.0.1:8080 --name "daemon up"
ac-wait --until-pid-file /run/mon.pid --name "mon daemon stoppé"
ac-wait --name "deploy distant" -- ssh prod 'systemctl is-active --quiet app'
ac-wait --name "pause" -- sleep 900                       # simple timer
ac-wait --list                    # waiters vivants (id, session, âge, condition)
ac-wait --cancel w-1730000000-12345   # tue + notifie « annulé »
ac-wait --log w-1730000000-12345      # tail du log (polls, notif)
```

`--session SID` cible une autre session (défaut `$AGENTCONTROL_SESSION`).

## Pièges

- **Toujours un timeout** : un waiter ≠ une surveillance infinie. Ajuste
  `--timeout`/`--interval` au scénario (CI : 60-120 s de poll).
- **Condition idempotente et cheap** : elle tourne en boucle — pas d'effet
  de bord, pas de commande lourde/destructive. Un `--watch` qui ne rend
  jamais exit 0 → timeout garanti (ex. `gh pr checks --watch` : préférer
  `-- gh pr checks N` sans --watch, ou `--required` qui exit non-0 tant
  que pending).
- **Un seul waiter par condition** : ne pas enchaîner/doubler les waiters
  sur la même cible — cancel l'ancien ou attends son ping.
- **Delivery** : le ping est un `acctl send` classique → il arrive en file ;
  si la session est en plein tour il est délivré au prochain point de
  livraison (il n'interrompt pas le tour — pas de `--steer` côté waiter) ;
  session endormie → réveil auto.
- **Secrets** : le log local n'embarque que rc + tail de sortie, mais le
  `meta` contient la commande et le message ping affiche name|cmd — passe
  `--name` et évite les secrets en clair dans `<cmd>`.
- Le waiter survit à la fin de ton tour (process détaché), pas à un reboot
  de la machine.

## Note inter-agents

`ac-wait` est un binaire PATH indépendant de l'agent (devin, cursor, …) —
n'importe quelle session AgentControl peut l'utiliser. La SKILL.md vit dans
le repo pour les sessions devin ; pour un autre agent, pointer le chemin du
script (`~/.local/bin/ac-wait --help`) dans son contexte.
