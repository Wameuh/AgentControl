// Mock window.__TAURI_INTERNALS__ pour tester la WebView hors Tauri.
// Répond aux invoke() du frontend avec des données réalistes.
(function () {
  const SESSIONS = [
    { id: "s1", conn: "local", agent: "devin", title: "Refactor API client",
      cwd: "/home/agent/api", created_at: "2026-10-20T10:00:00Z",
      available_modes: ["normal", "auto"], available_models: ["devin-latest", "opus"],
      available_commands: [{ name: "compact", description: "Compacter le contexte" }],
      config_options: [
        { id: "mode", name: "Mode", category: "mode", type: "select",
          currentValue: "normal", options: [{ value: "normal", name: "Normal" }, { value: "auto", name: "Auto" }] },
        { id: "model", name: "Model", category: "model", type: "select",
          currentValue: "devin-latest", options: [{ value: "devin-latest", name: "devin-latest" }, { value: "opus", name: "opus" }] },
      ],
      state: "working" },
    { id: "s2", conn: "local", agent: "mock", title: "Fix login crash",
      cwd: "/home/agent/app", created_at: "2026-10-19T09:00:00Z", group: "Serveur pi",
      available_modes: [], available_models: [], available_commands: [], state: "idle" },
    { id: "s3", conn: "local", agent: "devin", title: "Write unit tests",
      cwd: "/home/agent/tests", created_at: "2026-10-18T08:00:00Z", group: "Serveur pi",
      available_modes: [], available_models: [], available_commands: [], state: "dead" },
    { id: "s4", conn: "local", agent: "mock", title: "Reduce context usage",
      cwd: "/home/agent/infra", created_at: "2026-10-21T07:30:00Z",
      available_modes: [], available_models: [], available_commands: [], state: "compacting" },
  ];
  const EVENTS = [
    { seq: 1, ts: "10:01", type: "user_message", text: "Refactorise le client API pour utiliser fetch avec retry exponentiel." },
    { seq: 2, ts: "10:01", type: "agent_thought", text: "I need to look at the existing client structure first, then introduce a retry wrapper around fetch calls." },
    { seq: 3, ts: "10:02", type: "tool_call", call_id: "c1", title: "Read src/api/client.ts", status: "done", excerpt: "export async function apiCall(url) { const r = await fetch(url); return r.json(); }" },
    { seq: 4, ts: "10:03", type: "tool_call", call_id: "c2", title: "Edit src/api/client.ts", status: "done", excerpt: "+async function withRetry(fn, n = 3) { … }" },
    { seq: 5, ts: "10:04", type: "agent_text", text: "J'ai refactoré le client API : `withRetry` encapsule les appels fetch avec backoff exponentiel (3 tentatives, 500ms de base)." },
    { seq: 6, ts: "10:05", type: "tool_call", call_id: "c3", title: "Run npm test", status: "running", excerpt: "" },
    { seq: 7, ts: "10:05", type: "permission_request", request_id: "r1",
      title: "Autoriser l'exécution de npm test ?",
      options: [
        { option_id: "always", name: "Toujours autoriser", kind: "allow_always" },
        { option_id: "once", name: "Autoriser une fois", kind: "allow_once" },
        { option_id: "no", name: "Refuser", kind: "reject_once" },
      ] },
    { seq: 8, ts: "10:06", type: "user_message", steer: true, text: "Pendant que tu y es, ajoute aussi un test d'intégration." },
    { seq: 9, ts: "10:07", type: "agent_text", text:
      "## Résumé du refactor\n\nVoici ce qui a été fait :\n\n- `withRetry` avec **backoff exponentiel**\n- Gestion des erreurs *transient*\n- Tests ajoutés dans tests/client.test.ts\n\n```ts\nasync function withRetry(fn, n = 3) {\n  try { return await fn(); }\n  catch (e) {\n    if (n > 0) { await sleep(500); return withRetry(fn, n - 1); }\n    throw e;\n  }\n}\n```\n\n| Fichier | Lignes |\n|---------|--------|\n| client.ts | +42 |\n| tests | +18 |\n\n> Attention : le retry n'est pas idempotent sur les POST.\n\nVoir [la doc](https://example.com/retry) pour les détails. Et aussi un _it_ et ~~barré~~." },
    { seq: 10, ts: "10:08", type: "user_message",
      text: "Regarde https://github.com/Wameuh/AgentControl/pull/2 et src/api/client.ts:12 stp" },
    { seq: 11, ts: "10:09", type: "agent_text",
      text: "Le build tourne ici : https://github.com/Wameuh/AgentControl/actions/runs/39909340620 — je surveille." },
  ];
  const DIR = {
    dir: "/home/agent/api",
    entries: [
      { name: "src", dir: true, size: 0 },
      { name: "tests", dir: true, size: 0 },
      { name: ".git", dir: true, size: 0 },
      { name: "Cargo.toml", dir: false, size: 1240 },
      { name: "package.json", dir: false, size: 980 },
      { name: "README.md", dir: false, size: 4321 },
      { name: "logo.png", dir: false, size: 68 },
      { name: ".env", dir: false, size: 120 },
    ],
  };
  const FILE = { content: "export async function apiCall(url, opts) {\n  return withRetry(() => fetch(url, opts), 3);\n}\n\nasync function withRetry(fn, n) {\n  try { return await fn(); }\n  catch (e) { if (n > 0) { await sleep(500); return withRetry(fn, n - 1); } throw e; }\n}", truncated: false, path: "/home/agent/api/src/client.ts" };

  function rpc(method, args) {
    switch (method) {
      case "list_sessions": return SESSIONS;
      case "list_agents": return [{ id: "devin", name: "Devin" }, { id: "mock", name: "Mock agent" }];
      case "session_info": return { meta: SESSIONS[0], pending: ["r1"], alive: true, last_seq: EVENTS.length };
      case "events": return { events: EVENTS, last_seq: EVENTS.length };
      case "queue_list": return { queue: ["Compile aussi la doc quand tu as fini"] };
      case "list_dir": return DIR;
      case "read_file": return args.binary ? {
        path: "/home/agent/api/logo.png", size: 68, truncated: false,
        data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
      } : FILE;
      case "set_session_group": {
        const s = SESSIONS.find((x) => x.id === args.session);
        if (s) s.group = args.group;
        return { group: args.group };
      }
      case "daemon_status": return { version: "0.2.9", sessions: SESSIONS.length };
      case "pr_list": return [
        { conn: "local", repo: "Wameuh/AgentControl", pr: 42,
          session: SESSIONS[0].id, branch: "feat/x", title: "feat: exemple",
          status: "open", via: "detected" },
        { conn: "local", repo: "Wameuh/AgentControl", pr: 95,
          session: SESSIONS[1].id, branch: "feat/sous-agents",
          title: "feat: sous-agents dans l'arbre", status: "draft",
          via: "manual" },
        { conn: "local", repo: "Wameuh/AgentControl", pr: 88,
          session: SESSIONS[3].id, branch: "fix/scroll",
          title: "fix: scroll ancré au bas", status: "open",
          via: "detected" },
        { conn: "local", repo: "cli/cli", pr: 1204,
          session: "agent-externe-qui-n-existe-plus", branch: "patch-1",
          title: "fix: token refresh retry", status: "conflict",
          via: "manual" },
        { conn: "local", repo: "cli/cli", pr: 1190,
          session: "", branch: "docs/readme",
          title: "docs: contribution guide", status: "open", via: "manual" },
      ];
      case "clear_session": return { session: args.session };
      case "list_native_sessions": return { sessions: [] };
      case "get_blob": return { data: "blob", total: 4 };
      default: return null;
    }
  }

  let cbId = 0;
  const callbacks = new Map();
  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: "main" }, currentWebview: { label: "main" } },
    transformCallback(cb) { const id = ++cbId; callbacks.set(id, cb); return id; },
    runCallback(id, data) { callbacks.get(id)?.(data); },
    convertFileSrc(p) { return p; },
    invoke(cmd, args) {
      const p = Promise.resolve();
      if (cmd === "platform") return p.then(() => (location.hash === "#desktop" ? "linux" : "android"));
      if (cmd === "plugin:app|version") return p.then(() => "0.2.9");
      if (cmd === "plugin:event|listen") return p.then(() => ++cbId);
      if (cmd === "plugin:event|unlisten") return p.then(() => null);
      if (cmd.startsWith("plugin:")) return p.then(() => null);
      switch (cmd) {
        case "connect": return p.then(() => ({ version: "0.2.9", sessions: SESSIONS.length, servers: 1 }));
        case "call": return p.then(() => rpc(args.method, args.params ?? args));
        case "subscribe": return p.then(() => null);
        case "saved_connections": return p.then(() => []);
        case "ssh_hosts": return p.then(() => []);
        case "ssh_pubkey": return p.then(() => "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDemoAgentControlKeyExample agentcontrol");
        case "check_update": return p.then(() => { throw new Error("offline"); });
        case "ensure_daemon": case "save_connection": case "update_client":
          return p.then(() => null);
        default: return p.then(() => null);
      }
    },
  };
})();
