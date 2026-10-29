// E2E « scroll-up historique » — journal de 650 events avec un run de 50
// tool_call_update (seqs 201-250, sans rendu) en pleine zone de pagination.
// Sans le chaînage, cette page ne pousse pas la sentinelle hors de vue →
// l'observer ne se redéclenche jamais → scroll-up mort.
// + garde trimRendered : garé dans l'historique, un event live ne doit pas
// aspirer les pages sous les yeux.
// Usage : npm run build && node tests/scrollup-e2e.mjs
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";

const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";

const MOCK = `(function () {
  const J = [];
  for (let s = 1; s <= 650; s++) {
    J.push(s >= 201 && s <= 250
      ? { seq: s, ts: "x", type: "tool_call_update", call_id: "c1", excerpt: "x" }
      : { seq: s, ts: "x", type: "agent_text", text: "msg " + s });
  }
  const SESS = { id: "s1", conn: "local", agent: "devin", title: "ScrollTest",
    cwd: "/home/agent/api", created_at: "2026-10-20T10:00:00Z",
    available_modes: [], available_models: [], available_commands: [],
    config_options: [], state: "idle" };
  const calls = [];
  window.__calls = calls;
  function rpc(method, args) {
    switch (method) {
      case "list_sessions": return [SESS];
      case "list_agents": return [{ id: "devin", name: "Devin" }];
      case "session_info": return { meta: SESS, pending: [], alive: true, last_seq: J.length };
      case "events": {
        calls.push({ before: args.before_seq ?? null, limit: args.limit });
        const lim = args.limit ?? 200;
        const pool = args.before_seq ? J.filter((e) => e.seq < args.before_seq) : J;
        return { events: pool.slice(-lim), last_seq: J.length };
      }
      case "daemon_status": return { version: "0.6.0", sessions: 1 };
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
      if (cmd === "plugin:app|version") return p.then(() => "0.6.0");
      if (cmd === "plugin:event|listen") {
        window.__acListeners[args.event] = args.handler;
        return p.then(() => ++cbId);
      }
      if (cmd === "plugin:event|unlisten") return p.then(() => null);
      if (cmd.startsWith("plugin:")) return p.then(() => null);
      switch (cmd) {
        case "connect": return p.then(() => ({ version: "0.6.0", sessions: 1, servers: 1 }));
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

const server = spawn("npx", ["vite", "preview", "--port", "4192", "--strictPort"], { stdio: "ignore" });
process.on("exit", () => server.kill());
await new Promise((r) => setTimeout(r, 1800));
const b = await puppeteer.launch({ executablePath: CHROMIUM, headless: true, args: ["--no-sandbox"] });
const p = await b.newPage();
await p.setViewport({ width: 1280, height: 860 });
p.on("console", (m) => { if (m.type() === "error") console.log("[page]", m.text()); });
p.on("pageerror", (e) => console.log("[pageerror]", e.message));
await p.evaluateOnNewDocument(MOCK);

let pass = 0, fail = 0;
const check = (name, ok, extra = "") => {
  if (ok) { pass++; console.log("PASS", name); }
  else { fail++; console.log("FAIL", name, extra); }
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const click = (sel) => p.evaluate((s) => document.querySelector(s)?.click(), sel);
const st = () => p.evaluate(() => {
  const evs = [...document.querySelectorAll("#events .ev")];
  const el = document.getElementById("events");
  return { n: evs.length, first: evs[0]?.dataset.seq, last: evs[evs.length - 1]?.dataset.seq,
           top: Math.round(el.scrollTop), h: el.scrollHeight };
});
const calls = () => p.evaluate(() => window.__calls.slice());
const scrollTop = async () => { await p.evaluate(() => { document.getElementById("events").scrollTop = 0; }); await sleep(700); };
const live = (seq) => p.evaluate((s) => {
  const id = window.__acListeners["session-event"];
  window.__TAURI_INTERNALS__.runCallback(id, { event: "session-event", payload: {
    session: "s1", event: { seq: s, ts: "x", type: "agent_text", text: "LIVE " + s } } });
}, seq);

await p.goto("http://localhost:4192/", { waitUntil: "networkidle0" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
await click("#btn-connect"); await sleep(300);
await p.type("#c-host", "local"); await click("#c-go"); await sleep(1200);
await p.evaluate(() => document.querySelector(".sess")?.click());
await sleep(1500);

// Ouverture : dernière page (200) — seqs 451-650 tout agent_text.
let s = await st();
check("open : 200 events rendus", s.n === 200, JSON.stringify(s));
check("open : premier seq = 451", s.first === "451");

// Scrolls jusqu'au run d'updates (before=251 → page 100% tool_call_update).
for (let i = 0; i < 4; i++) await scrollTop();
s = await st();
check("avant run : premier seq = 251", s.first === "251", JSON.stringify(s));

// LE test : un seul scroll doit CHAÎNER la page d'updates (0 nœud) + la
// page suivante — sinon scroll-up mort (sentinelle jamais expulsée).
let mark = (await calls()).length;
await scrollTop();
s = await st();
const news = (await calls()).slice(mark);
check("run updates chaîné : 2 appels events", news.length === 2, JSON.stringify(news));
check("run updates chaîné : before=251 puis 201",
  news[0]?.before === 251 && news[1]?.before === 201, JSON.stringify(news));
check("run updates sauté : premier seq = 151", s.first === "151", JSON.stringify(s));

// Jusqu'à la tête du journal.
for (let i = 0; i < 4; i++) await scrollTop();
s = await st();
check("tête atteinte : premier seq = 1", s.first === "1", JSON.stringify(s));
check("tête atteinte : 600 nodes", s.n === 600, String(s.n));
check("sonde before=1 -> [] passée", (await calls()).some((c) => c.before === 1));
mark = (await calls()).length;
await scrollTop(); await scrollTop();
check("fin de journal : plus aucun appel", (await calls()).length === mark,
  JSON.stringify((await calls()).slice(mark)));

// Garde trimRendered : garé en haut de l'historique, un event live ne doit
// PAS aspirer la page qu'on lit (ancien tapis roulant).
await live(651); await sleep(400);
s = await st();
check("live garé en haut : tête intacte (seq 1)", s.first === "1", JSON.stringify(s));
check("live garé en haut : +1 seulement", s.n === 601, String(s.n));

// Retour en bas : les vieux nœuds redeviennent retirables au prochain live.
await p.evaluate(() => { document.getElementById("events").scrollTop = 1e9; });
await sleep(300);
await live(652); await sleep(400);
s = await st();
check("live en bas : fenêtre au cap 400", s.n === 400, String(s.n));
check("live en bas : queue intacte", s.last === "652", JSON.stringify(s));

console.log(`\n${pass} PASS, ${fail} FAIL`);
await b.close(); server.kill();
process.exit(fail ? 1 : 0);
