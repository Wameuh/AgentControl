# Plugin démo « hello-plugin » — POC option B

Vue plugin minimale prouvant la chaîne complète : **serve → iframe
sandboxée → ready → query → data → rendu** (backlog #248,
`~/plugin-views-exploration.md` §5).

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
`allow-same-origin` (origine opaque) et la route `/plugin/<name>/`
exige le token de bridge éphémère (TTL ~5 min, scopé au composant).

## Limites connues (POC — durcissement en follow-ups)

- Token de bridge dans l'URL de l'iframe (`?token=`, TTL 5 min) —
  acceptable pour une démo loopback ; v2 : cookie de session ou
  handshake par en-tête.
- CSP `script-src 'unsafe-inline'` — v2 : hashes/nonce des assets.
- `web.bind` loopback → vue indisponible pour un démon distant ssh
  (le bind web n'est joignable qu'en local ou via le mode web).
- Pas de push : le plugin poll via `query` (comme les composants).
