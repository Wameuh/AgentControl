// Repro #96 — loads de fond sous coupure transitoire : list_pinned ne
// doit PAS tomber en « list_pinned: Error: connection lost ». Le load
// attend le retour de la conn (gate connReady) ou est rejoué une fois
// après un rejet transport (« connexion fermée ») — jamais de toast brut.
// Assertions : compteur d'appels list_pinned (parké pendant la panne),
// absence de toast .t-error, ligne épinglée rendue après le retour.
// Usage : npm run build && node tests/conn-bgload-e2e.mjs
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";

const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";

const MOCK = `(function () {
  window.__acWdMs = 300;
  window.__logs = [];
  window.__connectCalls = 0;
  window.__pinCalls = 0;
  window.__hangPins = false;    // list_pinned reste en vol tant que vrai
  window.__pendingCalls = [];   // resolvers de call() figés par le test
  window.__pendingConnects = [];
  window.__connectHang = false;
  const OK = { version: "9.9.9", pinned: true, sessions: 1 };
  const DEAD = { error: "connexion fermée", kind: "error", rtt_ms: 5 };
  const SESS = { id: "s1", title: "S", agent: "devin", state: "idle",
                 conn: "local", cwd: "/tmp", created_at: "t",
                 native_session_id: "n1", available_modes: [],
                 available_commands: [], available_models: [] };
  window.__ds = { local: OK, servers: 1, sessions: 1 };
  window.__connectResult = OK;
  // Tue la conn du point de vue client : rejette les call() en vol (le
  // drain Rust fait pareil en vrai) puis émet conn-dead.
  window.__dead = () => {
    for (const p of window.__pendingCalls.splice(0)) p(new Error("connexion fermée"), true);
    window.__ds = { local: DEAD, servers: 1, sessions: 0 };
    const id = window.__acListeners["conn-dead"];
    if (id != null) window.__TAURI_INTERNALS__.runCallback(id, { payload: "local" });
  };
  window.__heal = () => { window.__ds = { local: OK, servers: 1, sessions: 1 }; };
  function rpc(method, args) {
    switch (method) {
      case "daemon_status":
        return args.conn ? OK : window.__ds;
      case "list_sessions": return [SESS];
      case "pr_list": return [];
      case "queue_list": return { queue: [] };
      case "session_info":
        return { meta: { id: "s1", title: "S", agent: "devin", state: "idle",
                         cwd: "/tmp", created_at: "t", native_session_id: "n1",
                         available_modes: [], available_commands: [],
                         available_models: [] },
                 pending: [], alive: true, last_seq: 5 };
      case "events": return { events: [], last_seq: 5 };
      case "list_pinned":
        window.__pinCalls++;
        if (window.__hangPins)
          return new Promise((res, rej) =>
            window.__pendingCalls.push((err, failed) =>
              failed ? rej(err) : res({ pinned: [] })));
        return { pinned: [{ seq: 1, ts: "t", type: "msg", text: "epingle",
                            pinned_at: "t", live: true }] };
      default: return null;
    }
  }
  let cbId = 0;
  const callbacks = new Map();
  window.__acListeners = {};
  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: "main" }, currentWebview: { label: "main" } },
    transformCallback(cb) { const id = ++cbId; callbacks.set(id, cb); return id; },
    runCallback(id, data) { callbacks.get(id)?.(data); },
    convertFileSrc(p) { return p; },
    invoke(cmd, args) {
      const p = Promise.resolve();
      if (cmd === "platform") return p.then(() => "linux");
      if (cmd === "app_version") return p.then(() => "9.9.9");
      if (cmd === "plugin:event|listen") {
        window.__acListeners[args.event] = args.handler;
        return p.then(() => ++cbId);
      }
      if (cmd === "plugin:event|unlisten") return p.then(() => null);
      if (cmd.startsWith("plugin:")) return p.then(() => null);
      switch (cmd) {
        case "connect":
          window.__connectCalls++;
          if (window.__connectHang)
            return new Promise((res) => window.__pendingConnects.push(res));
          return p.then(() => window.__connectResult);
        case "call": return p.then(() => rpc(args.method, args.params ?? {}));
        case "subscribe": return p.then(() => null);
        case "saved_connections": return p.then(() => [{ kind: "local" }]);
        case "ssh_hosts": return p.then(() => []);
        case "ssh_pubkey": return p.then(() => "ssh-ed25519 AAAA x");
        case "check_update": return p.then(() => { throw new Error("offline"); });
        case "log_write":
          window.__logs.push(args.level + "|" + args.msg);
          return p.then(() => null);
        default: return p.then(() => null);
      }
    },
  };
})();`;

const PORT = 4231;
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });
process.on("exit", () => server.kill());
await new Promise((r) => setTimeout(r, 1800));
const b = await puppeteer.launch({ executablePath: CHROMIUM, headless: true, args: ["--no-sandbox"] });
const p = await b.newPage();
await p.setViewport({ width: 1280, height: 860 });
p.on("console", (m) => { if (m.type() === "error") console.log("[page]", m.text()); });
p.on("pageerror", (e) => console.log("[pageerror]", e.message));
await p.evaluateOnNewDocument(MOCK);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let pass = 0, fail = 0;
function check(name, ok, extra = "") {
  if (ok) { pass++; console.log("PASS", name); }
  else { fail++; console.log("FAIL", name, extra); }
}
const pinCalls = () => p.evaluate(() => window.__pinCalls);
const errToasts = () => p.evaluate(() =>
  document.querySelectorAll("#toasts .t-error").length);
const pinRows = () => p.evaluate(() =>
  document.querySelectorAll("#pp-list .pin-row").length);
const drain = () => p.evaluate(() => {
  for (const r of window.__pendingConnects.splice(0)) r(window.__connectResult);
});
// Le retry backoff (~2 s + jitter) lance connect — attendre qu'un
// connect soit réellement en vol avant de le relâcher.
const waitPendingConnect = async (timeout = 6000) => {
  const t0 = Date.now();
  while (Date.now() - t0 < timeout) {
    if (await p.evaluate(() => window.__pendingConnects.length) > 0) return true;
    await sleep(80);
  }
  return false;
};
const segCls = () => p.evaluate(() =>
  document.querySelector(".conn-seg .conn-st")?.className ?? "none");

await p.goto(`http://localhost:${PORT}/`, { waitUntil: "networkidle0" });
await sleep(800);

// t0 — conn auto ok, session rendue : clic → open (session_info, events,
// subscribe) → sessionOpened → premier list_pinned.
check("t0 conn ok", (await segCls()).includes("ok"), await segCls());
await p.evaluate(() => document.querySelector('.sess[data-sid="s1"]')?.click());
await sleep(700);
check("t0 list_pinned chargé", (await pinCalls()) === 1,
  "calls=" + (await pinCalls()));
check("t0 panneau épinglés fermé", await p.evaluate(() =>
  document.getElementById("pins-panel").hidden));

// t1 — conn-dead pendant la panne : le load de fond est PARKÉ (gate
// connReady), jamais rejeté en erreur visible.
await p.evaluate(() => { window.__connectHang = true; window.__dead(); });
await sleep(500);
check("t1 conn marquée down", (await segCls()).includes("bad") ||
  (await segCls()).includes("warn"), await segCls());
// Ouverture du panneau épinglés → load() → callBg → attente conn.
await p.evaluate(() => document.getElementById("btn-pins").click());
await sleep(1200);
check("t1 list_pinned parké (pas de 2e appel)", (await pinCalls()) === 1,
  "calls=" + (await pinCalls()));
check("t1 aucun toast d'erreur", (await errToasts()) === 0,
  "toasts=" + (await errToasts()));
check("t1 aucun log list_pinned", (await p.evaluate(() =>
  window.__logs.filter((l) => l.includes("list_pinned")).length)) === 0);

// Reconnexion : la gate se lève → l'appel part (le reopen rejoue
// sessionOpened → 2 appels possibles). La liste se remplit sans bruit.
check("t1 retry lancé", await waitPendingConnect());
await p.evaluate(() => { window.__heal(); });
await drain();
await sleep(1200);
check("t1 list_pinned rejoué au retour", (await pinCalls()) >= 2,
  "calls=" + (await pinCalls()));
check("t1 épinglé rendu", (await pinRows()) === 1,
  "rows=" + (await pinRows()));
check("t1 toujours aucun toast d'erreur", (await errToasts()) === 0,
  "toasts=" + (await errToasts()));

// t2 — rejet transport en vol (« connexion fermée » pendant l'appel) :
// connOp rejoue une fois la conn revenue, sans toast non plus.
await p.evaluate(() => { window.__hangPins = true; });
await p.evaluate(() => document.getElementById("pp-close").click());
await sleep(150);
await p.evaluate(() => document.getElementById("btn-pins").click());
await sleep(400);
check("t2 appel en vol compté", (await pinCalls()) === 3,
  "calls=" + (await pinCalls()));
await p.evaluate(() => { window.__connectHang = true; window.__dead(); });
await sleep(1200);
check("t2 rejet transitoire → en attente", (await pinCalls()) === 3,
  "calls=" + (await pinCalls()));
check("t2 aucun toast d'erreur", (await errToasts()) === 0,
  "toasts=" + (await errToasts()));
check("t2 retry lancé", await waitPendingConnect());
await p.evaluate(() => { window.__hangPins = false; window.__heal(); });
await drain();
await sleep(1200);
check("t2 rejeu post-retour", (await pinCalls()) >= 4,
  "calls=" + (await pinCalls()));
check("t2 épinglé toujours rendu", (await pinRows()) === 1,
  "rows=" + (await pinRows()));
check("t2 aucun toast d'erreur", (await errToasts()) === 0,
  "toasts=" + (await errToasts()));

console.log(`${pass} PASS, ${fail} FAIL`);
if (fail) {
  const all = await p.evaluate(() => window.__logs);
  console.log("== logs =="); for (const l of all) console.log(l);
}
await b.close();
server.kill();
process.exit(fail ? 1 : 0);
