// E2E « bouton d'envoi alternatif » — le bouton secondaire du composer
// reflète le mode INVERSE de la préférence d'envoi :
//   pref queue → icône steer, clic = send(steer:true)   (comportement historique)
//   pref steer → icône clock, clic = send(normal) → file côté démon
// Visibilité : steer seulement pendant "working" ; file pendant un tour
// (working / waiting_input) — hors ces états les deux modes se confondent.
// Usage : npm run build && node tests/alt-send-e2e.mjs
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";

const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";

// Session s1 démarre "working". Le mock enregistre les RPC dans
// window.__rpc ; un send non-steer/non-force est enfilé (window.__queue)
// comme le ferait le démon, puis queue_list la reflète dans la barre.
const MOCK = `(function () {
  const SESS = { id: "s1", conn: "local", agent: "devin", title: "AltBtn",
    cwd: "/w", created_at: "2026-10-20T10:00:00Z",
    available_modes: [], available_models: [], available_commands: [],
    config_options: [], state: "working" };
  window.__queue = [];
  window.__rpc = [];
  function rpc(method, args) {
    window.__rpc.push({ method, args });
    switch (method) {
      case "list_sessions": return [SESS];
      case "list_agents": return [{ id: "devin", name: "Devin" }];
      case "session_info": return { meta: SESS, pending: [], alive: true, last_seq: 1 };
      case "events": return { events: [{ seq: 1, ts: "x", type: "agent_text", text: "hi" }], last_seq: 1 };
      case "queue_list": return { queue: window.__queue.slice() };
      case "send":
        if (!args.steer && !args.force) window.__queue.push(args.text);
        return { queued: !args.steer };
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

const server = spawn("npx", ["vite", "preview", "--port", "4193", "--strictPort"], { stdio: "ignore" });
process.on("exit", () => server.kill());
await new Promise((r) => setTimeout(r, 1800));
const b = await puppeteer.launch({ executablePath: CHROMIUM, headless: true, args: ["--no-sandbox"] });
const p = await b.newPage();
await p.setViewport({ width: 1280, height: 860 });
p.on("pageerror", (e) => console.log("[pageerror]", e.message));
await p.evaluateOnNewDocument(MOCK);
// Préférence d'envoi = steer pour le premier scénario.
await p.evaluateOnNewDocument(() => localStorage.setItem("ac-sendmode", "steer"));

let pass = 0, fail = 0;
const check = (name, ok, extra = "") => {
  if (ok) { pass++; console.log("PASS", name); }
  else { fail++; console.log("FAIL", name, extra); }
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const click = (sel) => p.evaluate((s) => document.querySelector(s)?.click(), sel);
const altBtn = () => p.evaluate(() => {
  const b = document.getElementById("btn-alt");
  return { hidden: b.hidden, title: b.title, aria: b.getAttribute("aria-label"),
           icon: b.dataset.icon, hasClock: !!b.querySelector(".ic-clock"),
           hasSteer: !!b.querySelector(".ic-steer") };
});
const sends = () => p.evaluate(() => window.__rpc.filter((c) => c.method === "send"));
const typeInput = (txt) => p.evaluate((t) => {
  const i = document.getElementById("input");
  i.value = t;
  i.dispatchEvent(new Event("input", { bubbles: true }));
}, txt);
// Change la préférence via le select des settings (chemin utilisateur réel).
const setPref = (v) => p.evaluate(async (val) => {
  document.getElementById("tb-settings").click();
  const s = document.getElementById("s-sendmode");
  s.value = val;
  s.dispatchEvent(new Event("change"));
  document.getElementById("settings-close").click();
}, v);
// Pousse un état live (status_changed → updateHeader → syncAltBtn).
const setState = (state, seq) => p.evaluate((s, q) => {
  const id = window.__acListeners["session-event"];
  window.__TAURI_INTERNALS__.runCallback(id, { event: "session-event", payload: {
    session: "s1", event: { seq: q, ts: "x", type: "status_changed", state: s } } });
}, state, seq);

await p.goto("http://localhost:4193/", { waitUntil: "networkidle0" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
await click("#btn-connect"); await sleep(300);
await p.type("#c-host", "local"); await click("#c-go"); await sleep(1200);
await p.evaluate(() => document.querySelector(".sess")?.click());
await sleep(1200);

// --- pref=steer, session working : bouton « file » -------------------------
let st = await altBtn();
check("pref steer + working : bouton visible", st.hidden === false, JSON.stringify(st));
check("pref steer : icône clock", st.hasClock && st.icon === "clock", JSON.stringify(st));
check("pref steer : tooltip file", /queue/i.test(st.title), st.title);
check("pref steer : aria-label = tooltip", st.aria === st.title, st.aria);

await typeInput("compile la doc aussi");
await click("#btn-alt"); await sleep(400);
let sd = await sends();
check("clic bouton file → send(normal)", sd.length === 1 && sd[0].args.steer === false && sd[0].args.force === false,
  JSON.stringify(sd));
const qbar = await p.evaluate(() => {
  const bar = document.getElementById("queue-bar");
  return { hidden: bar.hidden, txt: bar.textContent };
});
check("envoi arrivé dans la queue-bar", qbar.hidden === false && qbar.txt.includes("compile la doc"),
  JSON.stringify(qbar));

// --- bascule pref → queue, sans reload : bouton « steer » ------------------
await setPref("queue"); await sleep(300);
st = await altBtn();
check("pref queue : icône steer (sans reload)", st.hasSteer && st.icon === "steer", JSON.stringify(st));
check("pref queue : tooltip steer", /inject/i.test(st.title), st.title);
check("pref queue + working : bouton visible", st.hidden === false, JSON.stringify(st));

await typeInput("passe en mode auto");
await click("#btn-alt"); await sleep(400);
sd = await sends();
check("clic bouton steer → send(steer:true)", sd.length === 2 && sd[1].args.steer === true && sd[1].args.force === false,
  JSON.stringify(sd));

// --- visibilité selon l'état ----------------------------------------------
await setState("idle", 2); await sleep(300);
st = await altBtn();
check("pref queue + idle : steer caché", st.hidden === true, JSON.stringify(st));

await setPref("steer"); await sleep(300);
st = await altBtn();
check("pref steer + idle : file cachée", st.hidden === true, JSON.stringify(st));

await setState("waiting_input", 3); await sleep(300);
st = await altBtn();
check("pref steer + waiting_input : file visible", st.hidden === false && st.hasClock, JSON.stringify(st));

await setState("compacting", 4); await sleep(300);
st = await altBtn();
check("pref steer + compacting : file cachée (steer=file en retenue)", st.hidden === true, JSON.stringify(st));

console.log(`\n${pass} PASS, ${fail} FAIL`);
await b.close(); server.kill();
process.exit(fail ? 1 : 0);
