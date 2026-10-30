// E2E « bouton steer d'une entrée de file » — chaque message en file
// porte un bouton ↪ qui appelle queue_steer {session, index} :
//   delivered "steer"    → injection lancée, file à jour (entrée retirée)
//   delivered "requeued" → refus agent → toast warn, entrée toujours là
//   delivered "gone"     → index périmé → toast warn
//   capability absente   → bouton masqué (vieux démon)
// Usage : npm run build && node tests/queue-steer-e2e.mjs
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";

const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";

// Session s1 avec 3 entrées en file. window.__queue alimente queue_list ;
// window.__steerNext pilote le `delivered` renvoyé par queue_steer.
// `__withCap` contrôle la capability queue_steer du daemon_status.
const MOCK = `(function () {
  const SESS = { id: "s1", conn: "local", agent: "devin", title: "QSteer",
    cwd: "/w", created_at: "2026-10-20T10:00:00Z",
    available_modes: [], available_models: [], available_commands: [],
    config_options: [], state: "working" };
  window.__queue = ["premier msg", "deuxième msg", "troisième msg"];
  window.__steerNext = "steer";
  window.__rpc = [];
  window.__withCap = true;
  function rpc(method, args) {
    window.__rpc.push({ method, args });
    switch (method) {
      case "list_sessions": return [SESS];
      case "list_agents": return [{ id: "devin", name: "Devin" }];
      case "session_info": return { meta: SESS, pending: [], alive: true, last_seq: 1 };
      case "events": return { events: [{ seq: 1, ts: "x", type: "agent_text", text: "hi" }], last_seq: 1 };
      case "queue_list": return { queue: window.__queue.slice() };
      case "queue_steer": {
        const d = window.__steerNext;
        if (d === "steer") window.__queue.splice(args.index, 1);
        if (d === "gone") window.__queue.splice(args.index, 1);
        return { delivered: d, reason: d === "held" ? "compact" : null,
                 queue: window.__queue.slice() };
      }
      case "daemon_status":
        return { version: "0.9.0", sessions: 1, queue_steer: window.__withCap };
      case "pr_list": return [];
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
      if (cmd === "plugin:app|version") return p.then(() => "0.9.0");
      if (cmd === "plugin:event|listen") {
        window.__acListeners[args.event] = args.handler;
        return p.then(() => ++cbId);
      }
      if (cmd === "plugin:event|unlisten") return p.then(() => null);
      if (cmd.startsWith("plugin:")) return p.then(() => null);
      switch (cmd) {
        case "connect": return p.then(() => ({
          version: "0.9.0", sessions: 1, servers: 1,
          queue_steer: window.__withCap }));
        case "call": return p.then(() => rpc(args.method, args.params ?? {}));
        case "subscribe": return p.then(() => null);
        case "saved_connections": case "ssh_hosts": return p.then(() => []);
        case "ssh_pubkey": return p.then(() => "ssh-ed25519 AAAA x");
        case "check_update": return p.then(() => { throw new Error("offline"); });
        default: return p.then(() => null);
      }
    },
  };
})();`;

const PORT = 4197;
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });
process.on("exit", () => server.kill());
await new Promise((r) => setTimeout(r, 1800));
const b = await puppeteer.launch({ executablePath: CHROMIUM, headless: true, args: ["--no-sandbox"] });
const p = await b.newPage();
await p.setViewport({ width: 1280, height: 860 });
p.on("pageerror", (e) => console.log("[pageerror]", e.message));
await p.evaluateOnNewDocument(MOCK);

let pass = 0, fail = 0;
const check = (name, ok, extra = "") => {
  if (ok) { pass++; console.log("PASS", name); }
  else { fail++; console.log("FAIL", name, extra); }
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const click = (sel) => p.evaluate((s) => document.querySelector(s)?.click(), sel);
// Boutons steer visibles dans la barre de file (1 par chip).
const steerBtns = () => p.evaluate(() =>
  [...document.querySelectorAll("#queue-bar .q-item button")]
    .filter((x) => x.querySelector(".ic-steer")).length);
const barTxt = () => p.evaluate(() => document.getElementById("queue-bar").textContent);
const steerCalls = () => p.evaluate(() => window.__rpc.filter((c) => c.method === "queue_steer"));

await p.goto(`http://localhost:${PORT}/`, { waitUntil: "networkidle0" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
await click("#btn-connect"); await sleep(300);
// kind=local : les capabilities sont enregistrées sous la clé "local"
// (connKeyOf({kind:"local"})), identique au conn des sessions mockées.
await p.select("#c-kind", "local");
await click("#c-go"); await sleep(1200);
await p.evaluate(() => document.querySelector(".sess")?.click());
await sleep(1200);

// --- capability présente : un bouton steer par entrée ----------------------
check("3 entrées → 3 boutons steer", (await steerBtns()) === 3);

// --- steer accepté : entrée retirée, file renvoyée -------------------------
await p.evaluate(() => {
  document.querySelectorAll("#queue-bar .q-item")[1]
    .querySelector("button .ic-steer").closest("button").click();
});
await sleep(500);
let sc = await steerCalls();
check("clic steer chip #2 → queue_steer index 1",
  sc.length === 1 && sc[0].args.session === "s1" && sc[0].args.index === 1,
  JSON.stringify(sc));
check("entrée retirée de la barre (delivered steer)",
  !(await barTxt()).includes("deuxième"), await barTxt());
check("les 2 autres entrées restent",
  (await barTxt()).includes("premier") && (await barTxt()).includes("troisième"),
  await barTxt());

// --- refus agent : requeued → toast warn + entrée toujours en file ---------
await p.evaluate(() => { window.__steerNext = "requeued"; });
await p.evaluate(() => {
  document.querySelectorAll("#queue-bar .q-item")[0]
    .querySelector("button .ic-steer").closest("button").click();
});
await sleep(500);
sc = await steerCalls();
check("clic steer chip #1 → queue_steer index 0",
  sc.length === 2 && sc[1].args.index === 0, JSON.stringify(sc));
check("requeued : entrée conservée dans la barre",
  (await barTxt()).includes("premier"), await barTxt());

// --- index périmé : gone → toast warn, pas de crash ------------------------
await p.evaluate(() => { window.__steerNext = "gone"; });
await p.evaluate(() => {
  document.querySelectorAll("#queue-bar .q-item")[0]
    .querySelector("button .ic-steer").closest("button").click();
});
await sleep(500);
check("gone : barre rerendue sans erreur", (await barTxt()).length > 0, await barTxt());

// --- event système live → refresh de la file (re-queue asynchrone) ---------
// Le démon émet « steer refusé — message remis en file » quand un steer
// tardif est refusé : la barre doit refléter le retour en file sans
// attendre un status_changed.
await p.evaluate(() => {
  window.__queue = ["msgA", "msgB"];
  const id = window.__acListeners["session-event"];
  window.__TAURI_INTERNALS__.runCallback(id, { event: "session-event", payload: {
    session: "s1", event: { seq: 9, ts: "x", type: "system",
      text: "steer refusé (x) — message remis en file" } } });
});
await sleep(600);
check("event système live → file rafraîchie",
  (await barTxt()).includes("msgA") && (await barTxt()).includes("msgB"),
  await barTxt());

// --- panneau d'inspection : un bouton steer par ligne aussi ----------------
await click("#queue-open"); await sleep(400);
const panelSteer = await p.evaluate(() =>
  [...document.querySelectorAll("#qp-list .qp-item .qp-acts button")]
    .filter((x) => x.querySelector(".ic-steer")).length);
check("panneau : 2 lignes → 2 boutons steer", panelSteer === 2, `n=${panelSteer}`);

// --- vieux démon : capability absente → bouton masqué ----------------------
// Nouvelle page avec __withCap=false AVANT le connect : les caps lues à
// markConnected n'ont pas queue_steer → gate masque le bouton.
const p2 = await b.newPage();
await p2.setViewport({ width: 1280, height: 860 });
p2.on("pageerror", (e) => console.log("[pageerror2]", e.message));
await p2.evaluateOnNewDocument(MOCK);
await p2.evaluateOnNewDocument(() => { window.__withCap = false; });
await p2.goto(`http://localhost:${PORT}/`, { waitUntil: "networkidle0" });
await p2.waitForSelector("#btn-connect", { timeout: 15000 });
await p2.evaluate(() => document.getElementById("btn-connect").click());
await sleep(300);
await p2.select("#c-kind", "local");
await p2.evaluate(() => document.getElementById("c-go").click());
await sleep(1200);
await p2.evaluate(() => document.querySelector(".sess")?.click());
await sleep(1200);
const noCap = await p2.evaluate(() => ({
  chips: document.querySelectorAll("#queue-bar .q-item").length,
  steer: [...document.querySelectorAll("#queue-bar .q-item button")]
    .filter((x) => x.querySelector(".ic-steer")).length,
  drop: [...document.querySelectorAll("#queue-bar .q-item button")]
    .filter((x) => x.querySelector(".ic-close")).length,
}));
check("capability absente : file affichée, 0 bouton steer",
  noCap.chips > 0 && noCap.steer === 0 && noCap.drop === noCap.chips,
  JSON.stringify(noCap));

console.log(`\n${pass} PASS, ${fail} FAIL`);
await b.close(); server.kill();
process.exit(fail ? 1 : 0);
