# Plugin démo « hello-plugin » — POC option B

Vue plugin minimale prouvant la chaîne complète : **serve → iframe
sandboxée → ready → query → data → rendu** (backlog #248).

## Installation (démo)

1. Copier les assets dans le dossier de composants du démon :

   ```sh
   mkdir -p ~/.agentcontrol/components/hello-plugin
   cp -r docs/plugin-view-demo/web ~/.agentcontrol/components/hello-plugin/
   ```

2. Déclarer le composant — ajouter `components.snippet.toml` à
   `~/.agentcontrol/components.toml` (nécessite `agentbacklog`
   installé pour la query `items`).

3. Côté client : panneau **Composants** → ligne `hello-plugin` →
   bouton « vue » (icône panneau). Le démon doit avoir `bind_http`
   activé ET être joignable en HTTP depuis le client (mode web :
   same-origin ; desktop : `daemon_status.web.bind` loopback — une
   conn `local`/`tcp` locale fonctionne, une conn `ssh` distante non,
   le bind web étant loopback côté distant).

## Ce que le plugin peut faire (bridge borné)

| Message | Sens | Effet |
|---|---|---|
| `ready` | client → plugin | bootstrap : token éphémère + nom du composant |
| `theme` | client → plugin | `dark: bool` |
| `query` | plugin → client | exécute `spec.queries[query]` du **propre** composant via `component_query` |
| `data` | client → plugin | réponse `{ok, data|error}` |
| `notify` | plugin → client | toast (`info`/`warn`/`error`) |
| `navigate` | plugin → client | `session:<id>` ouvre la session, `component:<name>` le panneau |
| `resize` | plugin → client | hauteur d'iframe (bornée 120–4000 px) |
| `open-external` | plugin → client | `https:` uniquement |

**Interdit** : invoke/RPC libre, autres composants, sessions, fichiers,
secrets, token web, storage partagé — l'iframe est `sandbox` sans
`allow-same-origin` (origine opaque) et le **document**
`/plugin/<name>/` exige le token de bridge éphémère (TTL ~5 min,
scopé au composant). Les sous-ressources (js/css/img) sont servies
sans token — même niveau d'exposition que la SPA : une iframe en
origine opaque ne peut porter ni query ni cookie de session sur ses
requêtes relatives, et les données passent par le bridge (RPC
authentifiée), pas par les assets.

## Limites connues (POC — durcissement en follow-ups)

- Token de bridge dans l'URL du document (`?token=`, TTL 5 min) —
  acceptable pour une démo loopback ; v2 : cookie de session ou
  handshake par en-tête. Une vue ouverte > 5 min qui re-navigue ou
  lazy-charge un document retombera en 401 (les assets, eux, restent
  servis — le token ne gate que le document).
- CSP `script-src 'unsafe-inline'` — v2 : hashes/nonce des assets.
- `web.bind` loopback → vue indisponible pour un démon distant ssh
  (le bind web n'est joignable qu'en local ou via le mode web).
- Pas de push : le plugin poll via `query` (comme les composants).
