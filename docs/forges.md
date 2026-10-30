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

Les sorties outils reproduisent aussi le contenu des fichiers lus
(docs, specs, `AGENTS.md`, tests) — deux barrières empêchent leurs URLs
de créer des entrées fantômes :

- une **denylist de placeholders** à l'extraction : tout chemin de repo
  dont chaque segment tient en un caractère ou est un libellé générique
  (`o/r`, `g/p`, `owner/repo`, `grp/sub/proj`, `a/b`, `foo/bar`,
  `exemple/truc`…) est ignoré avant même d'être une référence ;
- une **vérification d'existence sur la forge** pour toute référence
  *nouvelle* : l'entrée n'est créée que si la forge connaît réellement
  le repo ET la review — une URL plausible mais inventée (`corp/tools`,
  `infra-team/deploy/tool`…) n'entre jamais. Un `pr add` manuel reste
  possible (intention explicite, jamais vérifié), et GitLab/Gerrit ne
  matchent que les hôtes déclarés.

## Vérification d'existence (détection auto)

Une référence inconnue extraite d'une sortie part en **file de
vérification** dédiée (worker unique, un appel forge à la fois,
~300 ms entre appels) — le watcher n'attend jamais le réseau :

- **GitHub** : `gh api repos/{o}/{r}/pulls/{n}` (auth `gh` requise) ;
- **GitLab** : `GET <host>/api/v4/projects/<enc>/merge_requests/<iid>` —
  hôtes déclarés seulement, `token_env` honoré ;
- **Gerrit** : `GET <host>/changes/<num|Change-Id>` anonyme — hôtes
  déclarés seulement.

Verdicts : **existe** → l'entrée est enregistrée (event « PR détectée »
comme avant) ; **inexistante** (404/410, ou 401/403 = jamais relisible
par ce démon ; hôte non déclaré) → rejet + **cache négatif borné**
(1024 entrées, TTL 30 min) : les ré-occurrences de la même URL sont
ignorées sans nouvel appel, mais une référence créée *après* sa
citation redevient enregistrable à expiration ; **forge injoignable**
(panne, 5xx, rate-limit, `gh` absent/non authentifié) → retentatives
en backoff sur ~15 min puis **abandon** — la détection reste
best-effort : une URL réapparaissant plus tard en sortie reprendra le
cycle (et le backfill du journal au redémarrage rejoue les sorties).

Garde-fous : file dédupliquée par clé de référence (plafond 256 —
un flood d'URLs distinctes n'emballe ni la forge ni la mémoire) ;
une référence déjà au registre ne retourne jamais en vérif (la
détection synchrone reprend : réattribution « forte » éventuelle) ;
entrée créée entre-temps (`pr add`, autre file) → vérif sauté au
moment du traitement.

Conséquence sur un démon **sans `gh` fonctionnel** : les URLs github
ne peuvent pas être vérifiées → pas d'enregistrement auto (les
retentatives s'épuisent) ; `acctl pr add` reste le recours — choix
délibéré : une entrée que ce démon ne pourrait jamais relire au sync
n'aurait de toute façon aucune valeur.

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

Sans flags, la cible de `add` est déduite du remote `origin` (hostname
comparé aux hôtes configurés — le port ssh `:29418` d'un Gerrit matche
l'hôte web déclaré) ; sinon GitHub comme avant. `rm`/`note` fonctionnent
autrement : le numéro/Change-Id est **résolu dans le registre** —
`acctl pr rm 42` trouve une MR GitLab ou un change Gerrit aussi bien
qu'une PR GitHub (clé complète forge+host+repo+numéro), `--repo` (nu,
`host:path` ou URL), `--forge` et `--host` agissent comme filtres, et
une ambiguïté liste les candidates.

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

Chaque passage de sync trace aussi les entrées **vues** et
**injoignables** : `sync_fails` du registre est remis à zéro à la
première lecture live et incrémenté sinon (hôte mort ou non déclaré,
repo ou référence absente — pour GitHub, l'échec n'est compté que si
`gh auth status` répond : un démon sans gh fonctionnel ne pénalise
rien). Passé **3 échecs consécutifs** l'entrée devient **« non
résoluble »** : état dédié et ligne estompée dans l'onglet Dépôts, hors
du compteur et des badges de session (le statut est restauré dès
qu'une sync la relit — un hôte revenu démasque l'entrée).

Le merge d'une review reste une **permission GitHub-only** : le hook de
merge des PR (`/mr review`, gates CI) ne s'étend pas à GitLab ni Gerrit —
le registre y est consultatif (visibilité, attribution, statut).
