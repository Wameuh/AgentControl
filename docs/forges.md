# Registre de review multi-forges (GitHub, GitLab, Gerrit)

Le démon maintient un registre *référence de review ↔ session*
(`prs.json` à sa racine) : quand un agent crée une PR/MR/change dans une
session, la référence lui est attribuée automatiquement et le badge
s'affiche dans la sidebar (`acctl list`, `acctl pr ls`).

## Détection automatique

Le watcher de chaque session scanne les extraits des tool calls :

- **GitHub** : `github.com/<o>/<r>/pull/<N>` — toujours actif.
- **GitLab** : `<host>/<groupe>/<projet>/-/merge_requests/<N>` — `gitlab.com`
  implicite, hôtes auto-hébergés via `[[forges]]`.
- **Gerrit** : `<host>/c/<projet>/+/<N>`, variantes `/r/` et `#/c/`, URL
  de recherche `/q/I<sha>` — uniquement sur hôtes déclarés (rien ne
  distingue un path Gerrit d'un autre service sans déclaration).
- **`Change-Id: I<40 hex>`** (trailer de commit dans un `git log`,
  `git show`, `git review`) : enregistré comme change Gerrit non résolu
  (`pr` = 0, badge `c#I<8>`) ; le numéro est résolu au prochain
  `acctl pr sync` et l'entrée re-clée, le Change-Id conservé.

Signal fort de (ré)attribution : `gh pr create`, `glab mr create`,
`git review`, push `refs/for/*` dans le titre du tool call.

## Configuration

`config.toml` du démon (`$AGENTCONTROL_HOME/config.toml`) :

```toml
[[forges]]
type = "gitlab"                    # "gitlab" | "gerrit"
hosts = ["gitlab.example.com"]     # host[:port][/prefix] sans schéma
token_env = "GITLAB_TOKEN"         # optionnel — NOM d'une var d'env
                                   # contenant le token (jamais le
                                   # token en clair dans la config)

[[forges]]
type = "gerrit"
hosts = ["gerrit.corp:8080", "example.com/gerrit"]  # port et préfixe
                                                    # de chemin OK
```

- `github.com` et `gitlab.com` sont implicites — pas besoin de les déclarer.
- Un même hôte peut servir plusieurs forges : le path de l'URL discrimine.
- Le set effectif est exposé via le RPC `forge_list`.

## `acctl pr`

```bash
acctl pr ls [--all]                     # badges : #N github, !N gitlab,
                                        # c#N / c#I<8> gerrit
acctl pr add <N|I<cid>> [--session S] [--repo R]
           [--forge github|gitlab|gerrit] [--host H[:port]] [--note t]
acctl pr note <N|I<cid>> <texte…> | acctl pr rm <N|I<cid>>
acctl pr sync                           # rafraîchit les statuts
```

La même collecte tourne côté démon via le RPC `pr_sync` (crate
`forgesync` partagée) : l'onglet « Dépôts » du client le demande à
l'ouverture puis toutes les 60 s tant que l'onglet est actif —
throttle démon ~45 s, un seul run à la fois (`{synced:false,
running|throttled:true}` sinon). Dans ce cadre `token_env` est lu dans
l'env du **démon** (dans celui d'acctl pour `acctl pr sync`).

Sans flags, la cible est déduite du remote `origin` (hostname comparé aux
hôtes configurés — le port ssh `:29418` d'un Gerrit matche l'hôte web
déclaré) ; sinon GitHub comme avant.

## Sync des statuts

- **GitHub** : `gh pr view|list` (inchangé).
- **GitLab** : `GET /api/v4/projects/<p%2Fenc>/merge_requests/<iid>` —
  anonyme sur les projets publics ; pour une instance privée, déclarer
  `token_env` et exporter la variable dans l'env d'`acctl`
  (`PRIVATE-TOKEN`). Un 401/403 ou une panne laisse le statut inchangé
  avec un warning.
- **Gerrit** : `GET /changes/<ref>` anonyme — marche sur les instances
  publiques (vérifié sur `gerrit-review.googlesource.com`). `status`
  NEW → open (+ `work_in_progress` → draft), MERGED → merged,
  ABANDONED → closed.
- **Portée des hôtes** : la collecte n'interroge que les hôtes déclarés
  (`forge_list` — gitlab.com implicite pour gitlab, github.com pour
  github) ; une entrée du registre sur un hôte non déclaré est notée
  « hôte non déclaré — statut inchangé », jamais requêtée.

Le merge d'une review reste une **permission GitHub-only** : le hook de
merge des PR (`/mr review`, gates CI) ne s'étend pas à GitLab ni Gerrit —
le registre y est consultatif (visibilité, attribution, statut).
