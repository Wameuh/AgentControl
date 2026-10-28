# Audit UX Android — rapport

Audit de l'interface mobile/Android du client (viewport 412 × 915, DPR 2.625,
UA Android 15 / Galaxy S25, portrait + landscape 915 × 412, thème sombre +
clair).

## Reproduire

```bash
cd client
npm install && npm run build
node tests/android-audit.mjs /tmp/audit-shots   # OUT_DIR optionnel, PORT auto
```

Le harness (`client/tests/android-audit.mjs`) sert le build via
`vite preview`, pilote Chromium headless (Puppeteer, détection auto du binaire
ou env `CHROME_PATH`), injecte un mock Tauri/RPC complet (sessions, events, fichiers
paginés, recherche, caps daemon) et écrit :

- `NN-nom.png` — une capture par scénario (42 scénarios a01→a32) ;
- `violations.json` — constats automatisés `{screen, kind, detail}` ;
- `dlg-log.json` — trace d'ouverture des dialogues (debug).

Assertions automatisées : cibles tactiles < 44 px, fonts < 12 px, débordement
hors viewport (`offscreen`), dialogues hors viewport, chevauchement
toast/titlebar (`toast-overlap`), perf de scroll > 32 ms/frame, double-push du
history ctx (`ctx-double-push`), échec d'étape (`script-step`).

Dernier run : **1 121 constats bruts** — `target<44` 740, `font<12` 375,
`offscreen` 4, `toast-overlap` 2 (les deux premiers groupes sont très répétitifs
et agrégés ci-dessous). Aucun `script-step`, `ctx-double-push` ou `scroll-perf`
sur le run de référence.

## Constats — sévérité haute

### H1 — Les toasts chevauchent la titlebar et interceptent les taps

Preuve auto : `toast top=42` chevauche la titlebar (`bottom=56`), à
`a28-toast-refresh` et `a28b-rafale`. Visible aussi sur les captures : un toast
d'erreur recouvre le bouton ⋮ et le badge version.

Impact : un toast masque les contrôles de la titlebar *et absorbe les taps*
(plusieurs étapes du harness ont échoué ainsi avant d'écarter les toasts) —
sur téléphone réel, l'utilisateur tape ⋮ et touche le toast.

Piste : ancrer les toasts **sous** la titlebar (`top` ≥ hauteur titlebar +
marge, ou zone `env(safe-area-inset-top)`), ou les rendre non interactifs
(`pointer-events:none` sauf bouton de fermeture dédié).

### H2 — La toolbar de l'éditeur de fichier déborde du viewport

Preuve auto : `button#fv-save right=429` et `svg right=415` (> 412 px) en mode
édition ; capture `a16-fileedit` : le sélecteur d'encodage `UTF-8` prend ~50 %
de la barre et pousse crayon/`✓` hors champ à droite.

Impact : le bouton **Save est à moitié hors écran** — action critique
inaccessible/cliquable au pixel près.

Piste : compacter la barre en mode édition (libellés cachés, sélecteur
d'encodage en icône/menu, `flex-wrap` ou overflow-x scroll), garantir que
`#fv-save` reste toujours visible.

### H3 — Bouton « New agent » du drawer affleurant le bord bas

Capture `a02-drawer` : le bouton repose à 0 px du bord bas du viewport — sans
marge, la barre de gestes Android (navigation gestuelle) le recouvre
partiellement sur appareil réel.

Piste : ajouter `padding-bottom: max(12px, env(safe-area-inset-bottom))` au
conteneur du drawer ; vérifier pareil sur les bottom sheets (`#sctx`, sheet ⋮)
et le composer.

## Constats — sévérité moyenne

### M1 — L'autocomplete `/` reste ouverte après l'envoi

`sendInput` (`client/src/features/composer.ts`) vide `input.value` mais ne
masque pas `#cmd-ac` ni ne relance `refreshAc` — les seuls chemins de
fermeture sont blur / Échap / application d'une commande. Preuve : capture
`a30-landscape` — la barre `/resume …` est restée affichée après le scénario
a11.

Impact : barre flottante résiduelle masquant le bas du chat (surtout gênant en
landscape où l'espace vertical est rare).

Piste : `hide()`/`refreshAc()` explicite dans `sendInput` (et idem après
`sendSlash`), plus un test harness `visible("#cmd-ac") === false` post-envoi.

### M2 — Cibles tactiles < 44 px généralisé (740 occurrences brutes)

Groupé par sélecteur (occurrences) :

| Sélecteur | Taille mesurée | Où |
|---|---|---|
| `div.tool-head` ×90 | 318×40 | en-têtes d'outils du chat |
| `button.btn-allow` ×70 / `btn-reject` ×40 | ~150×36 / 79×36 | barre de permissions |
| `button.grp-add` ×66 | 61×15 | headers connexion/groupe (icône+label) |
| `button.tb-update` ×38 | 70×26 | badge version titlebar |
| `input#sess-filter-input` ×38 | 319×31 | champ filtre sidebar |
| `button#chat-more` ×38 | 42×42 | ⋮ du chat |
| `button#sv-*` ×37×2 | 154×42 | onglets Agents/Repos |
| `button.sess-del` ×33 | 40×40 | corbeille session |
| `select#mode/model-select` ×28×2 | hauteur ~30 | composer |
| `button#btn-cancel` ×28 | — | dialogue |
| `a` / `a.ulink` ×60 | ~19 px hauteur | liens inline du markdown (intrinsèque, acceptable) |
| `div.ac-item` ×13 | 390×39 | items autocomplete `/` |

Le seuil Android recommandé est 48 dp ; 44 px est déjà laxiste. Les plus
importants : boutons de permission (action destructive possible), `sess-del`,
`grp-add` (15 px de haut !), le ⋮ du chat, les onglets de vue.

Piste : passe CSS mobile — `min-height:44px` + padding sur ces classes dans
`mobile.css` ; élargir les zones de tap via `::before` étendu pour les boutons
icône compacts plutôt que grossir les glyphes.

### M3 — Tailles de font < 12 px (375 occurrences brutes)

| Sélecteur | Taille |
|---|---|
| `span.grp-add-lbl` | 9 px |
| `span.badge.st-*` / `span#chat-state` / `steer-badge` | 10–11 px |
| `span#sv-count` | 10 px |
| `span.grp-name` / `span.sess-pr` / `span.a` / `span.q-txt` | 11–11.5 px |
| `button.tb-update` | 11.5 px |

Tolérable pour des badges ornementaux ; en revanche `sess-pr`, `q-txt`
(contenu lu par l'utilisateur) et `grp-add-lbl` (étiquette d'action) méritent
≥ 12 px.

### M4 — Débordement des numéros de ligne de l'éditeur

Preuve auto : `span.syn-num right=592/599` (> 412 px) sur `a16`/`a17` — la
gutter syntaxe dépasse le viewport en mode édition (même famille que H2 : le
layout de l'éditeur n'est pas contraint en largeur mobile).

## Constats — faibles / observations

- **L1 — Landscape** : composer + selects occupent ~40 % de la hauteur (412 px
  utiles) ; envisager de masquer les selects mode/modèle sous un bouton en
  landscape, et un composer `max-height` réduit (capture `a30-landscape`).
- **L2 — Perf scroll liste sessions** : mesure instable — 66 ms/frame sur un
  run, 15,8 ms/frame sur le suivant (40 sessions mockées). Non conclusif en
  headless ; à valider sur appareil réel avant de conclure à une régression.
- **L3 — Liens inline du markdown** (~19 px) : cibles petites par nature ;
  améliorable via `line-height`/padding vertical des paragraphes, priorité
  basse.
- **Bottom sheets** : vérifier `env(safe-area-inset-bottom)` sur `#sctx`,
  sheet ⋮ et `#fctx` (même famille que H3, non mesuré automatiquement).

## Ce qui fonctionne bien

- Back-stack Android : retour ferme la sheet ⋮ puis le drawer, dans l'ordre —
  pas de double-push ctx.
- Garde de buffer sale : retour arrière en édition non sauvegardée ouvre bien
  `dlg-filedirty` (`a17`).
- Tous les dialogues s'ouvrent et restent dans le viewport : connexion (+ volet
  SSH), nouvel agent (scroll interne OK), groupe, resume, paramètres, À propos,
  outdated.
- Menus contextuels session/groupe/fichier, recherche plein texte + navigation
  de résultats, rendu markdown + bascule source, viewer image, panel terminal,
  onglet Repos, filtre de sessions, thème clair — tous exercés sans anomalie.

## Caveats du harness (artefacts, pas des bugs produit)

- Certains `offscreen` « contenu » sont légitimes : éléments dans des
  conteneurs scrollables (fichier, liste) — le filtre ignore déjà la sidebar
  fermée ; les résidus ont été triés manuellement ci-dessus.
- Vite bind uniquement `[::1]` → le harness poll/goto sur `http://[::1]:PORT`.
- Un tap sur `#tb-menu` *pendant* que le drawer est ouvert tombe sur
  `#btn-connect` (le drawer recouvre la titlebar — comportement normal d'un
  drawer modal) : `closeDrawer()` passe par le scrim, comme un vrai utilisateur.
- `page.goBack()` est borné par `history.state.ac` — sans garde, le navigateur
  quitte l'app une fois la pile interne vide.

## Prompts délégables

> Chaque bloc est autonome : le coller tel quel à un agent pour en faire une PR
> (base `next_release`, workflow standard draft → `/mr review` → checks).

### Fix 1 — Position des toasts

```
Bug : les toasts (#toasts) chevauchent la titlebar — mesuré top=42 alors que
la titlebar finit à bottom≈56 — et interceptent les taps sur les boutons ⋮ /
badge update (preuve : client/tests/android-audit.mjs, kind toast-overlap).
Corriger : ancrer les toasts sous la titlebar (top ≥ hauteur titlebar, ex.
top: calc(56px + env(safe-area-inset-top))) et/ou pointer-events:none sauf sur
le bouton de fermeture. Vérifier en mobile 412px et desktop que le toast ne
masque plus aucun contrôle. npm run build + rejouer
node tests/android-audit.mjs /tmp/shots → 0 toast-overlap.
```

### Fix 2 — Toolbar éditeur responsive

```
Bug : en mode édition du viewer fichier (412px), #fv-save déborde (right=429)
et la gutter .syn-num dépasse (right=599) — le sélecteur d'encodage occupe la
moitié de la barre (capture a16-fileedit). Rendre la toolbar responsive :
compacter le sélecteur d'encodage (icône/menu ou width réduit), autoriser
overflow-x/wrap, garantir #fv-save toujours visible ; contraindre la gutter
dans le viewport. npm run build + audit → 0 offscreen sur a16/a17.
```

### Fix 3 — Safe-area bas du drawer et des sheets

```
Bug : le bouton « New agent » du drawer affleure le bord bas du viewport
(a02-drawer) — la barre de gestes Android le recouvre. Ajouter
padding-bottom: max(12px, env(safe-area-inset-bottom)) au footer du drawer ;
vérifier de même #sctx, le sheet ⋮ du chat, #fctx et le composer.
npm run build + vérification visuelle portrait/landscape.
```

### Fix 4 — Fermer l'autocomplete `/` après envoi

```
Bug : sendInput() (client/src/features/composer.ts) vide input.value sans
masquer #cmd-ac → la barre d'autocomplete reste affichée après l'envoi d'une
commande (visible sur a30-landscape). Appeler le hide/refreshAc existant dans
sendInput (et après application d'une commande si nécessaire). Vérifier :
taper "/", envoyer → #cmd-ac hidden. npm run build.
```

### Fix 5 — Cibles tactiles 44 px (passe mobile)

```
Bug : ~20 classes de contrôles < 44px de haut (agrégat violations.json :
.btn-allow/.btn-reject 36px, .sess-del 40px, .grp-add 15px, #chat-more 42px,
.sv 42px, .ac-item 39px, .tool-head 40px, #sess-filter-input 31px,
selects ~30px, .tb-update 26px). Passe CSS dans mobile.css : min-height 44px
(+ padding/tap-area ::before pour les boutons icône compacts) sur ces classes
sous media query mobile. npm run build + audit → target<44 résiduel = liens
inline uniquement.
```

### Fix 6 — Fonts ≥ 12 px sur contenu lu

```
Bug : fonts < 12px sur du contenu lu par l'utilisateur : .sess-pr, .q-txt,
.grp-add-lbl (9px), .fname (violations.json kind font<12). Les badges
ornementaux (.badge.st-*, #sv-count, steer-badge) peuvent rester petits —
juger au cas par cas. Remonter les fonts de contenu à ≥12px dans mobile.css
(ou base si la taille vient des styles desktop). npm run build + audit.
```

### Fix 7 — Densité verticale en landscape

```
Bug : en landscape 915×412, header chat + selects mode/modèle + composer
occupent ~40% de la hauteur utile (a30-landscape). Sous media query
(orientation:landscape) et (max-height:500px) : réduire composer max-height,
envisager de replier les selects derrière un bouton, compacter le chat-header.
npm run build + audit → capture a30 à joindre à la PR.
```

## Limites

- Chrome headless ≠ WebView Android réel : fonts, safe-area et clavier virtuel
  diffèrent ; les constats « bords d'écran » (H3, L3) sont à revalider sur
  appareil.
- Le clavier virtuel n'est pas simulé (pas de resize du viewport à l'ouverture
  du composer) — un audit dédié serait utile.
- Les mesures de scroll perf en headless sont indicatives seulement (L2).
