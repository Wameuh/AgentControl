// Repro #74 — la sonde keepalive ne doit tuer une conn que sur mort
// PROUVÉE (transport silencieux ou erreur de lien), pas sur lenteur.
// La page boote avec une conn « local » ; __ds pilote la réponse
// fusionnée daemon_status et le watchdog est raccourci (__acWdMs).
// Assertions : classe du segment conn (ok/warn/bad), compteur de
// connect (les kills déclenchent un retry), log ui capturé.
// Usage : npm run build && node tests/conn-flap-e2e.mjs
import puppeteer from "puppeteer-core";
import { startVitePreview } from "./lib/vite-preview.mjs";

const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";

const MOCK = `(function () {
  window.__acWdMs = 300;
  window.__logs = [];
  window.__connectCalls = 0;
  // Sonde pilotée par le test : {local: réponse|{error,rtt_ms,silent_ms}}.
  const OK = { version: "9.9.9", sessions: 1 };
  window.__ds = { local: OK, servers: 1, sessions: 1 };
  // list_sessions piloté (conn_error périmé post-reconnect).
  window.__listSessions = [];
  // Résolvers de connect en attente — le test les relâche à la main.
  window.__pendingConnects = [];
  window.__connectHang = false;
  window.__connectResult = OK;
  function rpc(method, args) {
    switch (method) {
      case "daemon_status":
        return args.conn ? OK : window.__ds;
      case "list_sessions": return window.__listSessions;
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
          window.__logs.push("evt|connect-call #" + window.__connectCalls);
          if (window.__connectHang)
            return new Promise((res) => window.__pendingConnects.push(res));
          return p.then(() => window.__connectResult);
        case "call": return p.then(() => rpc(args.method, args.params ?? {}));
        case "subscribe": return p.then(() => null);
        case "saved_connections": return p.then(() => [{ kind: "local" }]);
        case "cli_conn": return p.then(() => null);
        case "other_instances": return p.then(() => []);
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

const { server, base: BASE } = await startVitePreview();
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

await p.goto(BASE + "/", { waitUntil: "networkidle0" });
await sleep(600);

// t0 — la conn saved auto-connectée est ok, segment vert.
check("t0 conn auto ok", (await st()) === "ok", await segCls());
const calls0 = await connectCalls();
// connect figé pour TOUTE la suite : un retry qui résoudrait tout seul
// produit des cycles down→ok↔down cachés et rend les assertions racy —
// la récupération ne se fait qu'aux drains explicites ci-dessous.
await p.evaluate(() => { window.__connectHang = true; });
const drain = async () => p.evaluate(() => {
  for (const r of window.__pendingConnects.splice(0)) r(window.__connectResult);
});

// t1 — sonde timeout MAIS transport vivant (silent 400 ms ≪ 8 s) :
// 5 ticks sans strike, sans kill, sans reconnect — le flap est coupé.
await setDs({ error: "timeout", kind: "timeout", rtt_ms: 8010, silent_ms: 400 });
await sleep(1600);
check("t1 lenteur+trafic → ok", (await st()) === "ok", await segCls());
check("t1 aucun retry", (await connectCalls()) === calls0,
  "calls=" + (await connectCalls()));
check("t1 log transport vivant",
  (await logs("transport alive")).length >= 1);

// t2 — timeout + SILENCE radio (silent 30 s) : blackhole suspect →
// strike 1 (encore ok) puis strike 2 → down. Tick sain d'abord :
// compteurs repartis à zéro (les excusés de t1 ont pu toucher le
// plafond et léguer un strike).
await setDs({ version: "9.9.9", sessions: 1 });
await sleep(400);
const l2 = await logLen();
await setDs({ error: "timeout", kind: "timeout", rtt_ms: 8010, silent_ms: 30000 });
check("t2 strike1 loggé", await waitLog("sonde ko ×1", l2));
check("t2 strike1 encore ok", (await st()) === "ok", await segCls());
check("t2 strike2 loggé", await waitLog("sonde ko ×2", l2));
check("t2 strike2 → down", (await st()) === "bad", await segCls());
check("t2 log raison de mort",
  (await logs("conn local down — sonde: timeout ×2")).length === 1,
  JSON.stringify(await logs("conn local down")));

// t3 — retry planifié (backoff ~2 s) : connect figé → reconnecting ;
// sonde guérie + connect relâché → ok + télémétrie de retour.
check("t3 retry lancé (reconnecting)", await waitSeg("warn", 8000));
const calls1 = await connectCalls();
check("t3 retry = 1 connect", calls1 === calls0 + 1, "calls=" + calls1);
await setDs({ version: "9.9.9", sessions: 1 }); // démon guéri
await drain();
check("t3 reconnect → ok", await waitSeg("ok"));
check("t3 log reconnect", (await logs("reconnect #")).length >= 1);

// t4 — erreur de transport (« connexion fermée ») : mort réelle →
// strikes normaux → down en 2 ticks.
const l4 = await logLen();
await setDs({ error: "connexion fermée", kind: "error", rtt_ms: 12 });
check("t4 err strike1", await waitLog("connexion fermée", l4));
check("t4 err strike1 ok", (await st()) === "ok", await segCls());
check("t4 err strike2", await waitLog("sonde ko ×2", l4));
check("t4 err strike2 down", (await st()) === "bad", await segCls());
await setDs({ version: "9.9.9", sessions: 1 }); // guérir avant le drain
const l5 = await logLen(); // avant le drain — « de retour » comptera
await drain();

// t5 — sonde redevenue saine → markAlive efface les strikes : un err
// isolé repart à ×1 (compteur reset, pas cumul inter-pannes).
check("t5 retour sonde saine", await waitLog("back (", l5));
check("t5 sonde saine → ok", (await st()) === "ok", await segCls());
const l5b = await logLen();
await setDs({ error: "connexion fermée", kind: "error", rtt_ms: 12 });
check("t5 err isolée repart à ×1", await waitLog("sonde ko ×1", l5b));
check("t5 err isolée → ok", (await st()) === "ok", await segCls());
// timeout SANS silent_ms (vieux démon) → conservé en strike (mort non
// prouvée vivante) : le ×2 ci-dessous doit tomber.
// Pas de `kind` ici : repli sur le littéral error === "timeout" —
// couvre les vieux démons / bridges sans champ structuré.
const l5c = await logLen();
await setDs({ error: "timeout", rtt_ms: 8000 });
check("t5 timeout sans silent → ×2", await waitLog("sonde ko ×2", l5c));
check("t5 strike2 → down", (await st()) === "bad", await segCls());
await setDs({ version: "9.9.9", sessions: 1 }); // guérir avant le drain
await drain();

// t6 — charge simulée : « démon saturé » (timeout mais trafic vivant)
// entrecoupé de réponses saines → jamais down, jamais de retry.
// waitSeg("ok") : le retry de t5 est soldé (connect relâché → markAlive,
// timer purgé) → le compteur est stable avant de mesurer.
check("t6 conn revenue ok", await waitSeg("ok"));
const callsBefore6 = await connectCalls();
for (let i = 0; i < 4; i++) {
  await setDs({ error: "timeout", kind: "timeout", rtt_ms: 8100, silent_ms: 300 });
  await sleep(450);
  await setDs({ version: "9.9.9", sessions: 1 }); // réponse lente mais arrivée
  await sleep(450);
}
await setDs({ error: "timeout", kind: "timeout", rtt_ms: 8100, silent_ms: 500 });
await sleep(900);
check("t6 charge : jamais down", (await st()) === "ok", await segCls());
check("t6 charge : aucun reconnect",
  (await connectCalls()) === callsBefore6,
  "calls=" + (await connectCalls()));
await drain();

// t7 — borne anti-masquage : des timeouts excusés SANS FIN (RPC mort
// en permanence alors que les pushes passent) finissent par compter —
// 6 excusés → strike, 6 de plus → down. L'exemption reste bornée.
// Tick sain d'abord : le compteur « ignoré » repart à zéro (il peut
// traîner quelques excusés depuis la fin de t6).
await setDs({ version: "9.9.9", sessions: 1 });
await sleep(400);
const l7 = await logLen();
await setDs({ error: "timeout", kind: "timeout", rtt_ms: 8100, silent_ms: 200 });
check("t7 excusés ×5", await waitLog("ignored ×5", l7, 6000));
check("t7 plafond → strike", await waitLog("durably", l7, 6000));
check("t7 strike1 encore ok", (await st()) === "ok", await segCls());
check("t7 2ᵉ salve → strike2", await waitLog("sonde ko ×2", l7, 9000));
check("t7 RPC bloqué durable → down", (await waitSeg("bad", 3000)) || (await st()) === "warn",
  await segCls());
await setDs({ version: "9.9.9", sessions: 1 });
await drain();

// t8 — race post-reconnect : résultats de l'ANCIENNE gen ne doivent
// PAS tuer la conn fraîche (repro du flap « reconnect ok → sonde ko
// → down — conn-dead (EOF transport) »). Connect avec _ac_gen:2 ;
// sonde / conn_error / conn-dead stampés gen:1 → ignorés ; gen:2 tue.
check("t8 conn revenue ok", await waitSeg("ok"));
await p.evaluate(() => {
  window.__connectResult = { version: "9.9.9", sessions: 1, _ac_gen: 2 };
});
// Forcer un cycle down→reconnect pour poser la gen via noteGen.
const l8 = await logLen();
await setDs({ error: "connexion fermée", kind: "error", rtt_ms: 12, gen: 2 });
check("t8 prep strike1", await waitLog("sonde ko ×1", l8));
check("t8 prep strike2", await waitLog("sonde ko ×2", l8));
check("t8 prep down", await waitSeg("bad"));
await setDs({ version: "9.9.9", sessions: 1 });
await drain();
check("t8 reconnect gen2", await waitSeg("ok"));
check("t8 log reconnect gen2", (await logs("reconnect #")).length >= 1);
// Watchdog succès avec `_ac_gen` plus haute → noteGen monotone
// resynchronise (filet anti-régression dialogue vs retry).
await setDs({ version: "9.9.9", sessions: 1, _ac_gen: 3 });
await sleep(500);
const calls8 = await connectCalls();
const l8b = await logLen();
// Sonde périmée (gen 1) : pas de strike, pas de down.
await setDs({
  error: "connexion fermée", kind: "error",
  rtt_ms: 2760, silent_ms: 4436, gen: 1,
});
check("t8 sonde périmée loguée", await waitLog("stale probe", l8b));
check("t8 sonde périmée → encore ok", (await st()) === "ok", await segCls());
check("t8 sonde périmée : aucun retry",
  (await connectCalls()) === calls8, "calls=" + (await connectCalls()));
await setDs({ version: "9.9.9", sessions: 1, _ac_gen: 3 });
// conn_error list_sessions périmé → markDead ignoré.
await p.evaluate(() => {
  window.__listSessions = [{ conn: "local", conn_error: true, gen: 1 }];
});
await sleep(400);
check("t8 conn_error périmé → ok", (await st()) === "ok", await segCls());
await p.evaluate(() => { window.__listSessions = []; });
// conn-dead périmé (gen 2 < 3 resync) → ignoré.
const l8c = await logLen();
await p.evaluate(() => {
  const id = window.__acListeners["conn-dead"];
  window.__TAURI_INTERNALS__.runCallback(id, {
    payload: { conn: "local", why: "eof distant", gen: 2 },
  });
});
check("t8 conn-dead périmé logué", await waitLog("stale conn-dead", l8c));
check("t8 conn-dead périmé → ok", (await st()) === "ok", await segCls());
// conn-dead frais (gen 3) → down légitime. La sonde passe en échec
// AVANT le conn-dead : sinon le watchdog sain la répare au tick
// suivant (~300 ms) et waitSeg rate la fenêtre « bad » — flaky.
await setDs({ error: "connexion fermée", kind: "error", rtt_ms: 5, gen: 3 });
const l8d = await logLen();
await p.evaluate(() => {
  const id = window.__acListeners["conn-dead"];
  window.__TAURI_INTERNALS__.runCallback(id, {
    payload: { conn: "local", why: "eof distant (test)", gen: 3 },
  });
});
check("t8 conn-dead frais → down", await waitSeg("bad"));
check("t8 why frais",
  (await logs("eof distant (test)")).length >= 1,
  JSON.stringify(await logs("conn local down")));
await setDs({ version: "9.9.9", sessions: 1, _ac_gen: 3 });
await drain();
check("t8b conn revenue ok", await waitSeg("ok"));
// gen > cur (UI en retard) : signal live, pas périmé — reconverge.
await p.evaluate(() => {
  window.__connectResult = { version: "9.9.9", sessions: 1, _ac_gen: 4 };
});
const l8e = await logLen();
await setDs({ error: "connexion fermée", kind: "error", rtt_ms: 12, gen: 4 });
check("t8b prep down", await waitLog("sonde ko ×2", l8e));
check("t8b prep bad", await waitSeg("bad"));
await setDs({ version: "9.9.9", sessions: 1, _ac_gen: 4 });
await drain();
check("t8b reconnect", await waitSeg("ok"));
// UI connaît 4 ; un conn-dead stampé 6 (map plus haute, noteGen raté)
// doit tuer — pas être filtré comme périmé.
const l8f = await logLen();
await p.evaluate(() => {
  const id = window.__acListeners["conn-dead"];
  window.__TAURI_INTERNALS__.runCallback(id, {
    payload: { conn: "local", why: "eof gen>cur", gen: 6 },
  });
});
check("t8b gen>cur → down", await waitSeg("bad"));
check("t8b gen>cur why",
  (await logs("eof gen>cur")).length >= 1,
  JSON.stringify(await logs("conn local down")));
await setDs({ version: "9.9.9", sessions: 1, _ac_gen: 6 });
await drain();

// t9 — régression #270 : un marqueur `conn_error` de list_sessions ne
// doit PLUS tuer la conn instantanément. La sonde lourde paginée
// (timeout 20 s) peut expirer sur un démon lent post-reconnect alors
// que le transport est vivant — l'ancien markDead direct produisait le
// cycle down/reconnect → badge « déconnecté » persistant.
// t9a — transport vivant (silent 200 ms) : excuse « lent mais vivant »,
// jamais de strike, jamais de retry. La session seedée vérifie le
// repli cache : conn « ok » + conn_error → lignes grisées offline,
// pas disparues.
check("t9a conn revenue ok", await waitSeg("ok"));
const calls9 = await connectCalls();
const SESS9 = { conn: "local", id: "s-t9", agent: "devin", title: "t9",
  cwd: "/tmp", created_at: "2026-01-01T00:00:00Z", state: "working",
  available_modes: [], available_commands: [], available_models: [] };
await p.evaluate((s) => { window.__listSessions = [s]; }, SESS9);
await sleep(400);
check("t9 seed session rendue",
  await p.evaluate(() => !!document.querySelector("#session-list .sess")));
const l9a = await logLen();
await p.evaluate(() => {
  window.__listSessions = [{ conn: "local", conn_error: true,
    _ac_transport: { silent_ms: 200, dead: false, rx_lines: 40 } }];
});
check("t9a excuse transport vivant", await waitLog("transport alive", l9a, 4000));
await sleep(500);
check("t9a conn_error vivant → ok", (await st()) === "ok", await segCls());
check("t9a badge sidebar ok",
  (await p.evaluate(() =>
    document.querySelector("#session-list .grp-head .conn-st")?.className ?? "none"))
    .includes("ok"));
check("t9a session conservée grisée",
  await p.evaluate(() => {
    const r = document.querySelector("#session-list .sess");
    return !!r && r.className.includes("offline");
  }));
check("t9a conn_error vivant : aucun retry",
  (await connectCalls()) === calls9, "calls=" + (await connectCalls()));
// Sonde redevenue saine → session resservie live (hors offline).
await p.evaluate((s) => { window.__listSessions = [s]; }, SESS9);
await sleep(400);
check("t9a session de nouveau live",
  await p.evaluate(() => {
    const r = document.querySelector("#session-list .sess");
    return !!r && !r.className.includes("offline");
  }));

// t9b — socket morte (dead:true) : un SEUL marker ne déclare plus down
// non plus — strike ×1, down seulement au 2ᵉ groupé. L'ancien code
// passait en bad dès le premier marker.
const l9b = await logLen();
await p.evaluate(() => {
  window.__listSessions = [{ conn: "local", conn_error: true,
    _ac_transport: { dead: true, silent_ms: 60000 } }];
});
check("t9b strike1 loggé", await waitLog("sonde ko ×1", l9b, 4000));
await sleep(300);
check("t9b un marker ≠ down", (await st()) === "ok", await segCls());
check("t9b session toujours servie",
  await p.evaluate(() => !!document.querySelector("#session-list .sess")));
check("t9b marker mort : aucun retry",
  (await connectCalls()) === calls9, "calls=" + (await connectCalls()));
await p.evaluate((s) => { window.__listSessions = [s]; }, SESS9);
await setDs({ version: "9.9.9", sessions: 1, _ac_gen: 6 });
await sleep(500); // tick watchdog : strikes/ignored purgés

console.log(`${pass} PASS, ${fail} FAIL`);
if (fail) {
  const all = await p.evaluate(() => window.__logs);
  console.log("== logs =="); for (const l of all) console.log(l);
}
await b.close();
server.kill();
process.exit(fail ? 1 : 0);
