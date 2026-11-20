// E2E « écho optimiste d'envoi » — Enter affiche la bulle user
// immédiatement (.um-pending, opacité réduite) sans attendre le
// round-trip démo→agent→update ; le vrai user_message du journal la
// réconcilie sans doublon ; un envoi refusé retire le pending et
// restaure le brouillon.
// Usage : npm run build && node tests/echo-e2e.mjs
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";

const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";

// Session s1 ; window.__sendFail fait échouer le RPC send ;
// window.__queue alimente queue_list (barre de file).
const MOCK = `(function () {
  const SESS = { id: "s1", conn: "local", agent: "devin", title: "Echo",
    cwd: "/w", created_at: "2026-10-20T10:00:00Z",
    available_modes: [], available_models: [], available_commands: [],
    config_options: [], state: "working" };
  window.__queue = [];
  window.__sendFail = false;
  window.__rpc = [];
  function rpc(method, args) {
    window.__rpc.push({ method, args });
    switch (method) {
      case "list_sessions": return [SESS];
      case "list_agents": return [{ id: "devin", name: "Devin" }];
      case "session_info": return { meta: SESS, pending: [], alive: true, last_seq: 1 };
      case "events": return { events: [{ seq: 1, ts: "x", type: "agent_text", text: "hi" }], last_seq: 1 };
      case "queue_list": return { queue: window.__queue.slice() };
      case "send": {
        if (window.__sendFail) throw new Error("send refused");
        window.__queue.push(args.text);
        return { queued: true, queue: window.__queue.slice() };
      }
      case "daemon_status": return { version: "0.9.0", sessions: 1 };
      case "pr_list": return [];
      case "list_components": return [];
      case "list_pinned": return { pinned: [] };
      case "other_instances": return [];
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
      if (cmd === "app_version") return p.then(() => "0.9.0");
      if (cmd === "plugin:event|listen") {
        window.__acListeners[args.event] = args.handler;
        return p.then(() => ++cbId);
      }
      if (cmd === "plugin:event|unlisten") return p.then(() => null);
      if (cmd.startsWith("plugin:")) return p.then(() => null);
      switch (cmd) {
        case "connect": return p.then(() => ({ version: "0.9.0", sessions: 1, servers: 1 }));
        case "call": return p.then(() => rpc(args.method, args.params ?? {}));
        case "subscribe": return p.then(() => null);
        case "saved_connections": case "ssh_hosts": return p.then(() => []);
        case "other_instances": return p.then(() => []);
        case "cli_conn": return p.then(() => null);
        case "ssh_pubkey": return p.then(() => "ssh-ed25519 AAAA x");
        case "check_update": return p.then(() => { throw new Error("offline"); });
        default: return p.then(() => null);
      }
    },
  };
})();`;

const PORT = 4198;
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });
process.on("exit", () => server.kill());
await new Promise((r) => setTimeout(r, 1800));
const b = await puppeteer.launch({ executablePath: CHROMIUM, headless: true, args: ["--no-sandbox"] });
const p = await b.newPage();
await p.setViewport({ width: 1280, height: 860 });
p.on("pageerror", (e) => console.log("[pageerror]", (e.stack || e.message)));
await p.evaluateOnNewDocument(MOCK);

let pass = 0, fail = 0;
const check = (name, ok, extra = "") => {
  if (ok) { pass++; console.log("PASS", name); }
  else { fail++; console.log("FAIL", name, extra); }
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const click = (sel) => p.evaluate((s) => document.querySelector(s)?.click(), sel);
const pendings = () => p.evaluate(() =>
  [...document.querySelectorAll("#events .msg.user.um-pending")].map((n) => n.textContent));
const userBubbles = (txt) => p.evaluate((t) =>
  [...document.querySelectorAll("#events .msg.user")]
    .filter((n) => n.textContent.includes(t)).length, txt);
const pushEvent = (ev) => p.evaluate((e) => {
  const id = window.__acListeners["session-event"];
  window.__TAURI_INTERNALS__.runCallback(id, { event: "session-event", payload: {
    session: "s1", event: e } });
}, ev);

await p.goto(`http://localhost:${PORT}/`, { waitUntil: "networkidle0" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
await click("#btn-connect"); await sleep(300);
await p.select("#c-kind", "local");
await click("#c-go"); await sleep(1200);
await p.evaluate(() => document.querySelector(".sess")?.click());
await sleep(1200);

// --- envoi : pending immédiat, pas de bulle confirmée ----------------------
await p.type("#input", "hello echo");
await click("#btn-send");
await sleep(60); // > latence frame, ≪ round-trip démo sur lien lent
let pend = await pendings();
check("Enter → bulle pending visible tout de suite", pend.length === 1 && pend[0].includes("hello echo"), JSON.stringify(pend));
check("aucune bulle confirmée en double", (await userBubbles("hello echo")) === 1);
check("champ vidé après envoi", (await p.$eval("#input", (n) => n.value)) === "");

// --- livraison : le vrai user_message réconcilie sans doublon -------------
await pushEvent({ seq: 2, ts: "x", type: "user_message", text: "hello echo" });
await sleep(300);
pend = await pendings();
check("user_message réel → pending retiré", pend.length === 0, JSON.stringify(pend));
check("exactement une bulle user pour le message", (await userBubbles("hello echo")) === 1);
check("la bulle restante n'est plus pending",
  await p.evaluate(() => !document.querySelector("#events .msg.user.um-pending")));

// --- envoi en file : le pending survit tant que le chunk n'arrive pas ----
await p.type("#input", "queued msg");
await click("#btn-send");
await sleep(200);
check("envoi en file → pending conservé", (await pendings()).some((t) => t.includes("queued msg")));
check("barre de file visible (représentation file distincte)",
  await p.evaluate(() => !document.getElementById("queue-bar").hidden));

// --- erreur RPC : pending retiré + brouillon restauré ----------------------
await p.evaluate(() => { window.__sendFail = true; });
await p.type("#input", "bad msg");
await click("#btn-send");
await sleep(300);
pend = await pendings();
check("envoi refusé → pending retiré", !pend.some((t) => t.includes("bad msg")), JSON.stringify(pend));
check("brouillon restauré dans le champ",
  (await p.$eval("#input", (n) => n.value)).includes("bad msg"),
  await p.$eval("#input", (n) => n.value));
check("le pending « queued msg » est toujours là",
  (await pendings()).some((t) => t.includes("queued msg")));

// --- livraison du message en file : réconciliation différée ----------------
await pushEvent({ seq: 3, ts: "x", type: "user_message", text: "queued msg" });
await sleep(300);
check("livraison différée → pending retiré, une seule bulle",
  (await pendings()).length === 0 && (await userBubbles("queued msg")) === 1);

await b.close();
server.kill();
console.log(`\n${pass} PASS, ${fail} FAIL`);
process.exit(fail ? 1 : 0);
