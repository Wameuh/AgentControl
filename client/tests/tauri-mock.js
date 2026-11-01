// Mock window.__TAURI_INTERNALS__ pour tester la WebView hors Tauri.
// Répond aux invoke() du frontend avec des données réalistes.
(function () {
  // Clé de la connexion simulée — doit correspondre à connKeyOf(cfg) de la
  // saved_connections plus bas (ssh → ssh-native forcé sous Android) pour
  // que les capabilities du daemon_status s'appliquent aux sessions.
  const CONN = /Android/i.test(navigator.userAgent)
    ? "ssh-native:pi5.local:22" : "ssh:pi5.local:22";
  const SESSIONS = [
    { id: "s1", conn: CONN, agent: "devin", title: "Refactor API client",
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
    { id: "s2", conn: CONN, agent: "mock", title: "Fix login crash",
      cwd: "/home/agent/app", created_at: "2026-10-19T09:00:00Z", group: "Serveur pi",
      spawned_by: "s1",
      available_modes: [], available_models: [], available_commands: [], state: "idle" },
    { id: "s3", conn: CONN, agent: "devin", title: "Write unit tests",
      cwd: "/home/agent/tests", created_at: "2026-10-18T08:00:00Z", group: "Serveur pi",
      available_modes: [], available_models: [], available_commands: [], state: "dead" },
    { id: "s4", conn: CONN, agent: "mock", title: "Reduce context usage",
      cwd: "/home/agent/infra", created_at: "2026-10-21T07:30:00Z",
      origin: "component:backlog-orchestrator",
      available_modes: [], available_models: [], available_commands: [], state: "compacting" },
  ];
  const COMPONENTS = [
    { name: "acctl", kind: "tool", autostart: false,
      description: "CLI de contrôle du démon", source: "components.toml",
      spec: { name: "acctl", kind: "tool", command: ["acctl", "--version"] },
      status: { state: "absent" } },
    { name: "agentbacklog", kind: "tool", autostart: false,
      description: "File de tâches partagée", source: "components.toml",
      spec: { name: "agentbacklog", kind: "tool", command: ["agentbacklog", "--version"],
        queries: { list: ["agentbacklog", "--list", "--json"] } },
      status: { state: "installed", health: "ok", version: "agentbacklog 0.3.0" } },
    { name: "backlog-orchestrator", kind: "process", autostart: true,
      description: "Orchestrateur de la file", source: "components.toml",
      spec: { name: "backlog-orchestrator", kind: "process", command: ["backlog-orch"] },
      status: { state: "running", health: "ok", pid: 4210, uptime_s: 3725, restarts: 1 } },
    { name: "web", kind: "internal", autostart: true,
      description: "Interface web servie par le démon", source: "internal",
      spec: { name: "web", kind: "internal" },
      status: { state: "running" } },
  ];
  // File AgentBacklog servie par component_query{name,query:"list"} — les
  // tests la font évoluer via window.__BACKLOG (push) pour vérifier le
  // refresh sans reload, et masquent le composant via window.__blHide
  // (état « non déclaré » + CTA Intégrer → component_install le retire).
  const BACKLOG = [
    { id: 31, title: "[client] Panneau backlog : liste + refresh",
      description: "x", status: "wip", agent_id: "s2",
      created_at: "2026-10-28T10:00:00Z", started_at: new Date(Date.now() - 3700e3).toISOString(),
      done_at: null, priority: 0 },
    { id: 33, title: "[acctl] comp query <name> <query>", status: "pending",
      agent_id: null, created_at: "2026-10-28T11:00:00Z", started_at: null,
      done_at: null, priority: 0 },
    { id: 34, title: "[daemon] garde queries dans check_usable", status: "pending",
      agent_id: null, created_at: "2026-10-28T11:30:00Z", started_at: null,
      done_at: null, priority: 0, depends_on: 33, dep_status: "pending" },
    { id: 26, title: "[client] Vue diagramme", status: "done",
      agent_id: "s-orphan-1", created_at: "2026-10-27T09:00:00Z",
      started_at: "2026-10-27T09:05:00Z",
      done_at: new Date(Date.now() - 3600e3).toISOString(), priority: 0 },
  ];
  window.__BACKLOG = BACKLOG; // mutable par les tests (push → poll le ramasse)
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
  const DIRS = {
    "": {
      dir: "/home/agent/api",
      entries: [
        { name: "src", dir: true, size: 0 },
        { name: "tests", dir: true, size: 0 },
        { name: ".git", dir: true, size: 0 },
        { name: "Cargo.toml", dir: false, size: 1240 },
        { name: "package.json", dir: false, size: 980 },
        { name: "README.md", dir: false, size: 4321 },
        { name: "rapport.pdf", dir: false, size: 797 },
        { name: "logo.png", dir: false, size: 68 },
        { name: ".env", dir: false, size: 120 },
      ],
    },
    "src": {
      dir: "/home/agent/api/src",
      entries: [
        { name: "api", dir: true, size: 0 },
        { name: "client.ts", dir: false, size: 2890 },
        { name: "retry.ts", dir: false, size: 1154 },
        { name: "types.ts", dir: false, size: 640 },
      ],
    },
    "src/api": {
      dir: "/home/agent/api/src/api",
      entries: [
        { name: "client.ts", dir: false, size: 3120 },
        { name: "fetch.ts", dir: false, size: 845 },
      ],
    },
    "tests": {
      dir: "/home/agent/api/tests",
      entries: [
        { name: "client.test.ts", dir: false, size: 2034 },
      ],
    },
  };
  // PDF minimal valide (2 pages, repris de pdf-view-e2e.mjs) — sert la
  // preview du viewer PDF dans les captures.
  const PDF_B64 =
    "JVBERi0xLjQKMSAwIG9iago8PC9UeXBlL0NhdGFsb2cvUGFnZXMgMiAwIFI+PgplbmRvYmoKMiAwIG9iago8PC9UeXBlL1BhZ2VzL0tpZHNbMyAwIFIgNCAwIFJdL0NvdW50IDI+PgplbmRvYmoKMyAwIG9iago8PC9UeXBlL1BhZ2UvUGFyZW50IDIgMCBSL01lZGlhQm94WzAgMCA2MTIgNzkyXS9SZXNvdXJjZXM8PC9Gb250PDwvRjEgNSAwIFI+Pj4+L0NvbnRlbnRzIDYgMCBSPj4KZW5kb2JqCjQgMCBvYmoKPDwvVHlwZS9QYWdlL1BhcmVudCAyIDAgUi9NZWRpYUJveFswIDAgNjEyIDc5Ml0vUmVzb3VyY2VzPDwvRm9udDw8L0YxIDUgMCBSPj4+Pi9Db250ZW50cyA3IDAgUj4+CmVuZG9iago1IDAgb2JqCjw8L1R5cGUvRm9udC9TdWJ0eXBlL1R5cGUxL0Jhc2VGb250L0hlbHZldGljYT4+CmVuZG9iago2IDAgb2JqCjw8L0xlbmd0aCA0Mz4+CnN0cmVhbQpCVCAvRjEgMjQgVGYgNzIgNzIwIFRkIChIZWxsbyBwYWdlIDEpIFRqIEVUCmVuZHN0cmVhbQplbmRvYmoKNyAwIG9iago8PC9MZW5ndGggNDA+PgpzdHJlYW0KQlQgL0YxIDI0IFRmIDcyIDYyMCBUZCAoUGFnZSBkZXV4KSBUaiBFVAplbmRzdHJlYW0KZW5kb2JqCnhyZWYKMCA4CjAwMDAwMDAwMDAgNjU1MzUgZiAKMDAwMDAwMDAwOSAwMDAwMCBuIAowMDAwMDAwMDU0IDAwMDAwIG4gCjAwMDAwMDAxMTEgMDAwMDAgbiAKMDAwMDAwMDIyMyAwMDAwMCBuIAowMDAwMDAwMzM1IDAwMDAwIG4gCjAwMDAwMDAzOTggMDAwMDAgbiAKMDAwMDAwMDQ4OSAwMDAwMCBuIAp0cmFpbGVyCjw8L1NpemUgOC9Sb290IDEgMCBSPj4Kc3RhcnR4cmVmCjU3NwolJUVPRgo=";
  // Épinglés (pinned.json) : un « live » (seq+ts correspondent au
  // journal EVENTS) + un « dead » (icône d'avertissement — son event a
  // disparu après /clear). Couvre les deux états du panneau.
  const PINNED = [
    { seq: 5, ts: "10:04", type: "agent_text",
      text: "J'ai refactoré le client API : withRetry encapsule les appels fetch avec backoff exponentiel (3 tentatives, 500ms de base).",
      pinned_at: "2026-10-21T10:20:00Z", live: true },
    { seq: 3, ts: "09:58", type: "user_message",
      text: "Ajoute un coverage du chemin d'erreur réseau.",
      pinned_at: "2026-10-20T18:02:00Z", live: false },
  ];
  const CAPS = {
    version: "0.10.0", sessions: SESSIONS.length,
    agents: ["devin", "mock"],
    self_update: true, first_post: true, terminal: true, files: true,
    components: true, orchestrator: true, pinned: true, queue_steer: true,
    web: null,
  };
  const FILE = { content: "export async function apiCall(url, opts) {\n  return withRetry(() => fetch(url, opts), 3);\n}\n\nasync function withRetry(fn, n) {\n  try { return await fn(); }\n  catch (e) { if (n > 0) { await sleep(500); return withRetry(fn, n - 1); } throw e; }\n}", truncated: false, path: "/home/agent/api/src/client.ts" };

  function rpc(method, args) {
    switch (method) {
      case "list_sessions": return SESSIONS;
      case "list_agents": return [{ id: "devin", name: "Devin" }, { id: "mock", name: "Mock agent" }];
      case "session_info": {
        const m = SESSIONS.find((x) => x.id === args.session) ?? SESSIONS[0];
        return { meta: m, pending: ["r1"], alive: true, last_seq: EVENTS.length };
      }
      case "events": return { events: EVENTS, last_seq: EVENTS.length };
      case "queue_list": return { queue: ["Compile aussi la doc quand tu as fini"] };
      case "list_dir":
        return DIRS[args.path ?? ""] ??
          { dir: `/home/agent/api/${args.path ?? ""}`, entries: [] };
      case "read_file":
        if (args.binary && String(args.path).endsWith("rapport.pdf")) {
          return { path: "/home/agent/api/rapport.pdf", size: 797,
                   truncated: false, data: PDF_B64 };
        }
        return args.binary ? {
          path: "/home/agent/api/logo.png", size: 68, truncated: false,
          data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
        } : FILE;
      case "list_pinned": return { pinned: PINNED };
      case "pin_message": case "unpin_message": return { pinned: PINNED };
      case "set_session_group": {
        const s = SESSIONS.find((x) => x.id === args.session);
        if (s) s.group = args.group;
        return { group: args.group };
      }
      case "daemon_status": return CAPS;
      case "list_components":
        return COMPONENTS.filter((c) => c.name !== "agentbacklog" || !window.__blHide);
      case "component_query": {
        window.__qCalls = (window.__qCalls || 0) + 1;
        if (args.name !== "agentbacklog" || args.query !== "list")
          return Promise.reject(new Error(`query inconnue: ${args.name}/${args.query}`));
        const raw = JSON.stringify(window.__BACKLOG ?? BACKLOG);
        return { code: 0, data: btoa(unescape(encodeURIComponent(raw))),
          size: raw.length, truncated: false };
      }
      case "component_install": {
        if (args.name === "agentbacklog") window.__blHide = false;
        return { installed: true, name: args.name, state: "stopped", notes: [] };
      }
      // Journal métriques backlog — window.__blMetrics pilote le scénario :
      // "off" → RPC inconnue (vieux démon), [] → journal vide (orch off),
      // tableau → samples injectés, absent → série synthétique de 60 pts.
      case "backlog_metrics": {
        if (window.__blMetrics === "off")
          return Promise.reject(new Error("req inconnue: backlog_metrics"));
        const win = { "24h": 86400, "7d": 7 * 86400, "30d": 30 * 86400 }[
          args.window
        ] ?? 7 * 86400;
        const now = Date.now() / 1000;
        const n = 60;
        const samples = window.__blMetrics ?? Array.from(
          { length: n },
          (_, i) => ({
            ts: now - win + (i / (n - 1)) * win,
            pending: 3 + Math.round(2 * Math.sin(i / 6)),
            poppable: 2,
            wip: 1 + (i % 7 === 0 ? 1 : 0),
            workers_alive: 2 + (i % 5 === 0 ? 1 : 0),
            by_repo: { AgentControl: { pending: 3, wip: 1 } },
          }),
        );
        return {
          window: args.window ?? "7d",
          since: now - win,
          generated_at: new Date().toISOString(),
          journal: "/tmp/metrics.jsonl",
          sample_count: samples.length,
          corrupt_lines: 0,
          samples,
          aggregates: {
            items_available: true,
            items_total: 12, items_done: 6, items_started: 8,
            done_per_day: 0.86,
            lead_time_avg_s: 60300, lead_time_median_s: 54000,
            queue_wait_avg_s: 7400, queue_wait_median_s: 6900,
            workers_peak: 3,
          },
        };
      }
      case "pr_list": return [
        { conn: CONN, repo: "Wameuh/AgentControl", pr: 42,
          session: SESSIONS[0].id, branch: "feat/x", title: "feat: exemple",
          status: "open", via: "detected" },
        { conn: CONN, repo: "Wameuh/AgentControl", pr: 95,
          session: SESSIONS[1].id, branch: "feat/sous-agents",
          title: "feat: sous-agents dans l'arbre", status: "draft",
          via: "manual" },
        { conn: CONN, repo: "Wameuh/AgentControl", pr: 88,
          session: SESSIONS[3].id, branch: "fix/scroll",
          title: "fix: scroll ancré au bas", status: "open",
          via: "detected" },
        { conn: CONN, repo: "cli/cli", pr: 1204,
          session: "agent-externe-qui-n-existe-plus", branch: "patch-1",
          title: "fix: token refresh retry", status: "conflict",
          via: "manual" },
        { conn: CONN, repo: "cli/cli", pr: 1190,
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
      if (cmd === "app_version") return p.then(() => "0.10.0");
      if (cmd === "plugin:event|listen") return p.then(() => ++cbId);
      if (cmd === "plugin:event|unlisten") return p.then(() => null);
      if (cmd.startsWith("plugin:")) return p.then(() => null);
      switch (cmd) {
        case "connect": return p.then(() => ({ ...CAPS, servers: 1 }));
        case "call": return p.then(() => rpc(args.method, args.params ?? args));
        case "subscribe": return p.then(() => null);
        case "saved_connections": return p.then(() => [
          { kind: "ssh", host: "pi5.local", user: "me", port: 22, remote_port: 7331 },
        ]);
        case "cli_conn": return p.then(() => null);
        case "spawn_instance": return p.then(() => null);
        case "ssh_hosts": return p.then(() => []);
        case "ssh_pubkey": return p.then(() => "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDemoAgentControlKeyExample agentcontrol");
        // Appairage QR : pair_begin réussit avec un payload v2 démo (clé
        // jetable factice — captures README) ; le poll renvoie « gone »
        // (le dialogue affiche l'état sans dépendre d'un serveur réel).
        case "pair_resolve": return p.then(() => ({ host: args.cfg?.host ?? "demo.local", port: 22, user: "dev" }));
        case "pair_begin": return p.then(() => ({
          payload: JSON.stringify({
            v: 2, host: "pi5.local", port: 22, user: "me", rport: 7331,
            exp: Math.floor(Date.now() / 1000) + 290,
            k: "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAAEC1tb2NrLWRlbW8ta2V5LWZvci1zY3JlZW5zaG90\n-----END OPENSSH PRIVATE KEY-----",
            hk: ["SHA256:DemoHostKeyFingerprintForScreenshot"],
          }),
          exp: Math.floor(Date.now() / 1000) + 290,
          nonce: "demo-nonce-0001", warn: null, sweep: "cron",
        }));
        case "pair_devices": return p.then(() => []);
        case "pair_payload": return p.then(() => "agentcontrol://pair?v=1&host=demo.local&port=22&rport=7331&user=dev&auth=pw&secret=ZGVtbw");
        case "pair_poll": return p.then(() => "gone");
        case "pair_cleanup": case "pair_revoke": case "pair_import_key":
          return p.then(() => null);
        case "pair_attempt": return p.then(() => "SHA256:DemoFingerprintOfPairedKey");
        case "check_update": return p.then(() => { throw new Error("offline"); });
        case "ensure_daemon": case "save_connection": case "update_client":
          return p.then(() => null);
        default: return p.then(() => null);
      }
    },
  };
})();
