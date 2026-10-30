// Repro #74 — la sonde keepalive ne doit tuer une conn que sur mort
// PROUVÉE (transport silencieux ou erreur de lien), pas sur lenteur.
// La page boote avec une conn « local » ; __ds pilote la réponse
// fusionnée daemon_status et le watchdog est raccourci (__acWdMs).
// Assertions : classe du segment conn (ok/warn/bad), compteur de
// connect (les kills déclenchent un retry), log ui capturé.
// Usage : npm run build && node tests/conn-flap-e2e.mjs
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";

const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";

const MOCK = `(function () {
  window.__acWdMs = 300;
  window.__logs = [];
  window.__connectCalls = 0;
  // Sonde pilotée par le test : {local: réponse|{error,rtt_ms,silent_ms}}.
  const OK = { version: "9.9.9", sessions: 1 };
  window.__ds = { local: OK, servers: 1, sessions: 1 };
  // Résolvers de connect en attente — le test les relâche à la main.
  window.__pendingConnects = [];
  window.__connectHang = false;
  window.__connectResult = OK;
  function rpc(method, args) {
    switch (method) {
      case "daemon_status":
        return args.conn ? OK : window.__ds;
      case "list_sessions": return [];
      case "pr_list": return [];
      case "events": return { events: [], last_seq: 0 };
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
      if (cmd === "plugin:app|version") return p.then(() => "9.9.9");
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

const server = spawn("npx", ["vite", "preview", "--port", "4194", "--strictPort"], { stdio: "ignore" });
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
const segCls = () => p.evaluate(() =>
  document.querySelector(".conn-seg .conn-st")?.className ?? "none");
const st = async () => (await segCls()).replace("conn-st ", "");
const setDs = (local) => p.evaluate((v) => {
  window.__ds = { local: v, servers: 1, sessions: 1 };
}, local);
const logs = (sub) => p.evaluate((s) =>
  window.__logs.filter((l) => l.includes(s)), sub);
const logLen = () => p.evaluate(() => window.__logs.length);
// Attend qu'un log POSTÉRIEUR à `from` apparaisse (les strikes sont
// horodatés par le watchdog — assertions à la réception du log, pas
// sur des fenêtres qui peuvent chevaucher 2 ticks).
const waitLog = async (sub, from = 0, timeout = 4000) => {
  const t0 = Date.now();
  while (Date.now() - t0 < timeout) {
    const n = await p.evaluate((s, f) =>
      window.__logs.slice(f).filter((l) => l.includes(s)).length, sub, from);
    if (n > 0) return true;
    await sleep(40);
  }
  return false;
};
const waitSeg = async (want, timeout = 6000) => {
  const t0 = Date.now();
  while (Date.now() - t0 < timeout) {
    if ((await st()) === want) return true;
    await sleep(60);
  }
  return false;
};
const connectCalls = () => p.evaluate(() => window.__connectCalls);

await p.goto("http://localhost:4194/", { waitUntil: "networkidle0" });
await sleep(600);

// t0 — la conn saved auto-connectée est ok, segment vert.
check("t0 conn auto ok", (await st()) === "ok", await segCls());
const calls0 = await connectCalls();

// t1 — sonde timeout MAIS transport vivant (silent 400 ms ≪ 8 s) :
// 5 ticks sans strike, sans kill, sans reconnect — le flap est coupé.
await setDs({ error: "timeout", rtt_ms: 8010, silent_ms: 400 });
await sleep(1600);
check("t1 lenteur+trafic → ok", (await st()) === "ok", await segCls());
check("t1 aucun retry", (await connectCalls()) === calls0,
  "calls=" + (await connectCalls()));
check("t1 log transport vivant",
  (await logs("transport vivant")).length >= 1);

// t2 — timeout + SILENCE radio (silent 30 s) : blackhole suspect →
// strike 1 (encore ok) puis strike 2 → down.
const l2 = await logLen();
await setDs({ error: "timeout", rtt_ms: 8010, silent_ms: 30000 });
check("t2 strike1 loggé", await waitLog("sonde ko ×1", l2));
check("t2 strike1 encore ok", (await st()) === "ok", await segCls());
check("t2 strike2 loggé", await waitLog("sonde ko ×2", l2));
check("t2 strike2 → down", (await st()) === "bad", await segCls());
check("t2 log raison de mort",
  (await logs("conn local down — sonde: timeout ×2")).length === 1,
  JSON.stringify(await logs("conn local down")));

// t3 — retry planifié (backoff ~2 s) : connect figé → reconnecting,
// puis sonde guérie + connect relâché → ok + télémétrie de retour.
await p.evaluate(() => { window.__connectHang = true; });
await sleep(2800); // backoff 1.5–2.5 s → retry part
const calls1 = await connectCalls();
check("t3 retry lancé", calls1 === calls0 + 1, "calls=" + calls1);
check("t3 seg reconnecting", (await st()) === "warn", await segCls());
await setDs({ version: "9.9.9", sessions: 1 }); // démon guéri
await p.evaluate(() => {
  window.__connectHang = false;
  for (const r of window.__pendingConnects.splice(0)) r(window.__connectResult);
});
await sleep(600);
check("t3 reconnect → ok", (await st()) === "ok", await segCls());
check("t3 log reconnect", (await logs("reconnect #")).length >= 1);

// t4 — erreur de transport (« connexion fermée ») : mort réelle →
// strikes normaux → down en 2 ticks.
const l4 = await logLen();
await setDs({ error: "connexion fermée", rtt_ms: 12 });
check("t4 err strike1", await waitLog("connexion fermée", l4));
check("t4 err strike1 ok", (await st()) === "ok", await segCls());
check("t4 err strike2", await waitLog("sonde ko ×2", l4));
check("t4 err strike2 down", (await st()) === "bad", await segCls());
await p.evaluate(() => {
  for (const r of window.__pendingConnects.splice(0)) r(window.__connectResult);
});

// t5 — sonde redevenue saine → markAlive efface les strikes : un err
// isolé repart à ×1 (compteur reset, pas cumul inter-pannes).
const l5 = await logLen();
await setDs({ version: "9.9.9", sessions: 1 });
check("t5 retour sonde saine", await waitLog("de retour", l5));
check("t5 sonde saine → ok", (await st()) === "ok", await segCls());
const l5b = await logLen();
await setDs({ error: "connexion fermée", rtt_ms: 12 });
check("t5 err isolée repart à ×1", await waitLog("sonde ko ×1", l5b));
check("t5 err isolée → ok", (await st()) === "ok", await segCls());
// timeout SANS silent_ms (vieux démon) → conservé en strike (mort non
// prouvée vivante) : le ×2 ci-dessous doit tomber.
const l5c = await logLen();
await setDs({ error: "timeout", rtt_ms: 8000 });
check("t5 timeout sans silent → ×2", await waitLog("sonde ko ×2", l5c));
check("t5 strike2 → down", (await st()) === "bad", await segCls());
await p.evaluate(() => {
  for (const r of window.__pendingConnects.splice(0)) r(window.__connectResult);
});

// t6 — charge simulée : « démon saturé » (timeout mais trafic vivant)
// entrecoupé de réponses saines → jamais down, jamais de retry.
// waitSeg("ok") : le retry de t5 est soldé (connect relâché → markAlive,
// timer purgé) → le compteur est stable avant de mesurer.
check("t6 conn revenue ok", await waitSeg("ok"));
const callsBefore6 = await connectCalls();
for (let i = 0; i < 4; i++) {
  await setDs({ error: "timeout", rtt_ms: 8100, silent_ms: 300 });
  await sleep(450);
  await setDs({ version: "9.9.9", sessions: 1 }); // réponse lente mais arrivée
  await sleep(450);
}
await setDs({ error: "timeout", rtt_ms: 8100, silent_ms: 500 });
await sleep(900);
check("t6 charge : jamais down", (await st()) === "ok", await segCls());
check("t6 charge : aucun reconnect",
  (await connectCalls()) === callsBefore6,
  "calls=" + (await connectCalls()));

console.log(`${pass} PASS, ${fail} FAIL`);
await b.close();
server.kill();
process.exit(fail ? 1 : 0);
