---
name: ac-steps
description: Publier la liste d'étapes de la tâche en cours au démon AgentControl (acctl steps set) — visible dans le panneau « Étapes » du client et persistée par session
argument-hint: ""
allowed-tools:
  - exec
---

# ac-steps — progression de tâche visible dans le client

Quand tu travailles sur une tâche dans une session AgentControl, publie
une liste d'étapes minimale (nom + statut) au démon. Le client affiche
un panneau « Étapes » par session qui se met à jour en direct, et la
liste survit au redémarrage du démon (steps.json) et au resume.

Granularité volontairement minimale : **juste le nom de l'étape et son
statut** — pas de sous-tâches, pas de descriptions, pas de nesting.

## Statuts

`todo` | `in_progress` | `done` | `abandoned`

- **au plus une étape `in_progress`** à la fois — si tu avances, passe
  l'ancienne à `done` dans le même `set`.
- Marque `abandoned` plutôt que de **supprimer** une étape devenue
  obsolète — l'historique de ce qui a été tenté reste visible.
- Noms courts (un libellé, pas une phrase — ~200 caractères max,
  tronqués au-delà).

## Protocole

Ta session est `$AGENTCONTROL_SESSION` (injectée par le démon — même
variable qu'`acctl send`). Tu n'as pas besoin de la passer :

```bash
# Au démarrage de la tâche : publier la liste complète
acctl steps set "Analyser le code:todo,Implémenter:in_progress,Tests:todo,PR et review:todo"

# À chaque changement : repousser la liste ENTIÈRE (remplacement complet,
# idempotent — jamais d'édition incrémentale, pas de drift d'index)
acctl steps set "Analyser le code:done,Implémenter:done,Tests:in_progress,PR et review:todo"

# Voir l'état courant (debug)
acctl steps get
```

- La forme compacte est `nom:statut` séparés par des virgules ; le
  statut est après le **dernier** `:` (un nom peut contenir `:`).
  Alias acceptés : `wip`, `in-progress` → `in_progress` ;
  `complete`, `finished` → `done` ; `skip`, `skipped` → `abandoned`.
- Forme JSON aussi acceptée :
  `acctl steps set '[{"name":"Tests","status":"in_progress"}]'`
- Vider la liste : `acctl steps set -` (ou `[]`).
- La liste est **bornée à ~100 étapes** — au-delà le démon refuse ;
  fusionne ou abandonne les étapes soldées.
- Le démon émet un event `steps` à chaque changement → live update
  client ; `session_info` porte `steps` pour reconstruire à
  l'ouverture. `/clear` et le rebind vers une autre session native
  vident la liste (le cycle repart à zéro).

## Quand publier

- **Au début** d'une tâche non triviale : découpe en 3-8 étapes
  représentatives du plan (ne pas sur-découper — le panneau doit rester
  lisible).
- **À chaque transition** d'étape (todo → in_progress → done).
- **Quand le plan change** : republie la liste entière avec les étapes
  révisées (`abandoned` pour celles abandonnées).
- Pas besoin de republier à chaque action triviale — seulement quand
  l'état d'une étape change.

## Compatibilité / autres providers

Le protocole est `session_set_steps` (RPC unique, liste complète) —
indépendant du provider. Ce skill vit dans `.devin/skills/ac-steps/`
(format SKILL.md standard) : les agents Devin le voient nativement.
Pour Claude Code / Codex / Cursor, copier ou lier ce répertoire dans
leur répertoire de skills (`~/.claude/skills/`, `~/.codex/…`,
`~/.cursor/…`) — le contenu reste identique, seul le chemin change ;
`acctl` et `$AGENTCONTROL_SESSION` sont déjà disponibles dans toute
session spawnée par le démon, quel que soit le provider.
