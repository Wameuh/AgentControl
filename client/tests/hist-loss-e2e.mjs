// Repro #61 — « l'historique d'une session disparaît à l'affichage ».
// La session stream un flux mixte (textes, pensées, tool calls, updates,
// usage_update masqués, status) puis des triggers suspects sont injectés
// un à un ; un MutationObserver compte les suppressions de nœuds .ev et
// les attribue au trigger qui les a provoquées.
// Usage : npm run build && node tests/hist-loss-e2e.mjs
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";

const CHROMIUM =
  process.env.CHROMIUM ??
  "/home/dev/.cache/puppeteer/chrome/linux-156.0.8078.47/chrome-linux64/chrome";

// Journal + contrôle total des pushes. window.__push(ev) émet un
// session-event ; __J est le journal servi par events/session_info.
const MOCK = `(function () {
  const J = [];
  for (let s = 1; s <= 60; s++) {
    J.push({ seq: s, ts: "x", type: s % 2 ? "user_message" : "agent_text",
             text: "hist " + s });
  }
  window.__J = J;
  const SESS = { id: "s1", conn: "local", agent: "devin", title: "HistLoss",
    cwd: "/x", created_at: "2026-10-20T10:00:00Z", native_session_id: "n1",
    available_modes: [], available_models: [], available_commands: [],
    config_options: [], state: "working" };
  const OTHER = { ...SESS, id: "s2", title: "Autre" };
  window.__missingCurrent = false;
  window.__lastSeq = J.length;
  function rpc(method, args) {
    switch (method) {
      case "list_sessions":
        return window.__missingCurrent ? [OTHER] : [SESS, OTHER];
      case "list_agents": return [{ id: "devin", name: "Devin" }];
      case "session_info": {
        const m = args.session === "s2" ? OTHER : SESS;
        // last_seq ≥ contenu du journal ET ≥ tout seq poussé — sinon
        // staleCache (lastSeq > last_seq) croirait à un reset.
        const r = { meta: m, pending: [], alive: true,
          last_seq: Math.max(J.length, window.__lastSeq) };
        // Délai simulé : fenêtre pendant laquelle des pushes live peuvent
        // s'intercaler dans le rechargement de open().
        return window.__delayInfo
          ? new Promise((res) => setTimeout(() => res(r), 700))
          : r;
      }
      case "events": {
        const lim = args.limit ?? 200;
        let pool = J;
        if (args.before_seq) pool = J.filter((e) => e.seq < args.before_seq);
        else if (args.after_seq) pool = J.filter((e) => e.seq > args.after_seq);
        return { events: pool.slice(-lim), last_seq: window.__lastSeq };
      }
      case "daemon_status": return { version: "9.9.9", sessions: 2 };
      case "pr_list": return [];
      default: return null;
    }
  }
  let cbId = 0;
  const callbacks = new Map();
  window.__acListeners = {};
  window.__push = (session, event) => {
    const id = window.__acListeners["session-event"];
    if (id == null) return "no-listener";
    window.__TAURI_INTERNALS__.runCallback(id, {
      event: "session-event", payload: { session, event } });
    return "ok";
  };
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
        case "connect": return p.then(() => ({ version: "9.9.9", sessions: 2, servers: 1 }));
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
p.on("console", (m) => { if (m.type() === "error") console.log("[page]", m.text()); });
p.on("pageerror", (e) => console.log("[pageerror]", e.message));
await p.evaluateOnNewDocument(MOCK);

// Compteur de suppressions de nœuds .ev attribué au trigger courant.
await p.evaluateOnNewDocument(() => {
  window.__removed = {};   // trigger -> [seqs]
  window.__added = {};     // trigger -> nb
  window.__trigger = "boot";
  window.__armed = false;
  const arm = () => {
    if (window.__armed) return;
    const el = document.getElementById("events");
    if (!el) return;
    window.__armed = true;
    new MutationObserver((muts) => {
      const t = window.__trigger;
      for (const m of muts) {
        for (const n of m.removedNodes) {
          if (n.nodeType === 1) {
            if (n.classList?.contains("ev"))
              (window.__removed[t] ??= []).push(n.dataset.seq ?? "?");
            n.querySelectorAll?.(".ev").forEach((x) =>
              (window.__removed[t] ??= []).push(x.dataset.seq ?? "?"));
          }
        }
        for (const n of m.addedNodes) {
          if (n.nodeType === 1) {
            if (n.classList?.contains("ev")) window.__added[t] = (window.__added[t] ?? 0) + 1;
            window.__added[t] = (window.__added[t] ?? 0) +
              (n.querySelectorAll?.(".ev").length ?? 0);
          }
        }
      }
    }).observe(el, { childList: true, subtree: true });
  };
  window.__arm = arm;
});

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let pass = 0, fail = 0;
const check = (name, ok, extra = "") => {
  if (ok) { pass++; console.log("PASS", name); }
  else { fail++; console.log("FAIL", name, extra); }
};

const st = () => p.evaluate(() => {
  const evs = [...document.querySelectorAll("#events .ev")];
  return {
    n: evs.length,
    visible: evs.filter((e) => !e.hidden).length,
    first: evs[0]?.dataset.seq, last: evs[evs.length - 1]?.dataset.seq,
    cur: window.__appDebug ? "?" : undefined,
  };
});
const removed = () => p.evaluate(() => window.__removed);
const added = () => p.evaluate(() => window.__added);
const ev = (t, extra = {}) => ({ seq: ++window.__lastSeq, ts: "x", type: t, ...extra });
const push = (session, e) => p.evaluate((s, e) =>
  window.__push(s, e), session, e);
const trig = (t) => p.evaluate((t) => { window.__trigger = t; }, t);

await p.goto("http://localhost:4193/", { waitUntil: "networkidle0" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
await p.evaluate(() => window.__arm());
await p.evaluate(() => document.querySelector("#btn-connect")?.click());
await sleep(300);
await p.type("#c-host", "local");
await p.evaluate(() => document.querySelector("#c-go")?.click());
await sleep(1200);
// Cliquer la session « HistLoss » (s1) — la sidebar trie par titre,
// « Autre » (s2) passerait sinon en premier.
await p.evaluate(() => {
  const rows = [...document.querySelectorAll(".sess")];
  const row = rows.find((r) => r.textContent?.includes("HistLoss"));
  (row ?? rows[0])?.click();
});
await sleep(1500);

let s = await st();
console.log("open:", JSON.stringify(s));
check("open : historique rendu", s.n >= 55, JSON.stringify(s));

// --- Trigger 1 : burst live mixte (stream actif réaliste) ---
await trig("t1-burst");
for (let i = 0; i < 30; i++) {
  const e = await p.evaluate((i) => ({
    seq: ++window.__lastSeq, ts: "x",
    type: i % 6 === 0 ? "agent_thought" : i % 3 === 0 ? "tool_call" : "agent_text",
    text: "live " + i,
    call_id: i % 3 === 0 ? "c" + i : undefined,
    title: "tool " + i, status: "running",
  }), i);
  await push("s1", e);
}
await sleep(500);
s = await st();
check("t1 : stream appendé", s.n >= 85, JSON.stringify(s));

// --- Trigger 2 : session-event pour une AUTRE session ---
await trig("t2-other");
await push("s2", await p.evaluate(() => ({
  seq: ++window.__lastSeq, ts: "x", type: "agent_text", text: "autre session" })));
await sleep(300);
s = await st();
const r2 = (await removed())["t2-other"];
check("t2 : event autre session ignoré", !r2 && s.n >= 85, JSON.stringify({ r2, n: s.n }));

// --- Trigger 3 : status_changed (refresh sidebar, session présente) ---
await trig("t3-status");
await push("s1", await p.evaluate(() => ({ seq: ++window.__lastSeq, ts: "x",
  type: "status_changed", state: "idle" })));
await sleep(400);
s = await st();
check("t3 : status sysline appendée", s.n >= 86, JSON.stringify(s));

// --- Trigger 4 : session disparue de la liste (flap refresh) → markGone ? ---
await trig("t4-missing");
await p.evaluate(() => { window.__missingCurrent = true; });
await push("s1", await p.evaluate(() => ({ seq: ++window.__lastSeq, ts: "x",
  type: "status_changed", state: "working" })));
await sleep(800); // refresh sidebar async
s = await st();
const r4 = (await removed())["t4-missing"];
check("t4 : session absente liste → vue conservée (markGone)",
  s.n >= 86 && !r4, JSON.stringify({ r4, n: s.n }));

// --- Trigger 5 : events live après markGone (current null) ---
await trig("t5-after-gone");
const r5a = (await p.evaluate(() => window.__lastSeq));
await push("s1", await p.evaluate(() => ({ seq: ++window.__lastSeq, ts: "x",
  type: "agent_text", text: "apres gone" })));
await sleep(300);
s = await st();
const a5 = (await added())["t5-after-gone"] ?? 0;
console.log("t5 : après markGone — events ajoutés:", a5, "état:", JSON.stringify(s));

// --- Trigger 6 : session_cleared — le wipe « officiel » ---
await trig("t6-cleared");
await push("s1", { seq: 999, ts: "x", type: "session_cleared" });
await sleep(400);
s = await st();
console.log("t6 : après session_cleared:", JSON.stringify(s));

// Le « nouveau cycle » pousse derrière — seqs monotones (le journal
// mocké ne reset jamais : __lastSeq ne doit PAS redescendre sous les
// seqs affichés, sinon session_info devient incohérent et staleCache
// croirait à un reset au prochain open). current étant null (markGone),
// ces pushes sont de toute façon filtrés — observationnel.
await trig("t6b-newcycle");
await p.evaluate(() => { window.__missingCurrent = false; });
for (let i = 1; i <= 5; i++) {
  await push("s1", await p.evaluate(() => ({ seq: ++window.__lastSeq,
    ts: "x", type: "agent_text", text: "new cycle" })));
}
await sleep(400);
s = await st();
console.log("t6b : après nouveau cycle:", JSON.stringify(s));

// --- Trigger 7 : replay/out-of-order (seq déjà rendu) — filtré aussi
// (current null) : observationnel. ---
await trig("t7-replay");
await push("s1", { seq: 10, ts: "x", type: "agent_text", text: "replay 10" });
await sleep(300);
s = await st();
console.log("t7 : après replay:", JSON.stringify(s));

// --- Trigger 8 : re-clic sur la session affichée (même chemin que
// reopenIfConn après une reconnexion) — le DOM vivant ne doit PAS
// être vidé : stash + restore instantané, aucun .ev retiré.
// La session est "gone" depuis t4 : on la remet dans la liste, on la
// rouvre (rechargement légitime), PUIS le re-clic est le vrai test. ---
await trig("t8-recover");
await p.evaluate(() => { window.__missingCurrent = false; });
await push("s2", { seq: await p.evaluate(() => ++window.__lastSeq), ts: "x",
  type: "status_changed", state: "idle" }); // → sidebar.refresh()
await sleep(600);
const clickS1 = () => p.evaluate(() => {
  const rows = [...document.querySelectorAll(".sess")];
  const row = rows.find((r) => r.textContent?.includes("HistLoss"));
  (row ?? rows[0])?.click();
});
await clickS1();
await sleep(1500);
s = await st();
console.log("t8a : réouverture après gone:", JSON.stringify(s));
await trig("t8-reclick");
const n8a = s.n;
// Marqueur d'identité : le stash détache les nœuds (l'observer compte
// donc des « removals ») mais restore réinstalle les MÊMES nœuds — un
// wipe + rechargement en créerait de nouveaux sans l'attribut.
await p.evaluate(() => {
  document.querySelector("#events .ev")?.setAttribute("data-keep", "1");
});
await clickS1();
await sleep(1500);
s = await st();
const kept = await p.evaluate(() =>
  document.querySelectorAll("#events .ev[data-keep]").length);
check("t8 : re-clic même session → DOM conservé (pas de wipe)",
  kept === 1 && s.n === n8a && s.n > 0,
  JSON.stringify({ kept, n: s.n, n8a }));

// --- Trigger 9 : event live PENDANT un open() (session_info lente) —
// tamponné puis déversé une fois, en fin de vue, pas de doublon. ---
await trig("t9-live-open");
const n9a = (await st()).n;
const seq9 = await p.evaluate(() => ++window.__lastSeq);
await p.evaluate(() => { window.__delayInfo = true; });
await p.evaluate(() => {
  const rows = [...document.querySelectorAll(".sess")];
  const row = rows.find((r) => r.textContent?.includes("HistLoss"));
  (row ?? rows[0])?.click();
});
await sleep(250); // open() en vol (session_info 700 ms)
await push("s1", { seq: seq9, ts: "x", type: "agent_text", text: "pendant open" });
await sleep(900); // fin de open()
await p.evaluate(() => { window.__delayInfo = false; });
s = await st();
const dup9 = await p.evaluate((seq) =>
  document.querySelectorAll(`#events .ev[data-seq="${seq}"]`).length, seq9);
check("t9 : event live pendant open → une fois, en queue",
  dup9 === 1 && s.last === String(seq9) && s.n === n9a + 1,
  JSON.stringify({ dup9, last: s.last, n: s.n, n9a }));

// --- Trigger 10 : replay d'un seq déjà rendu → ignoré (pas de doublon).
await trig("t10-replay-dup");
await push("s1", { seq: seq9, ts: "x", type: "agent_text", text: "doublon" });
await sleep(300);
const dup10 = await p.evaluate((seq) =>
  document.querySelectorAll(`#events .ev[data-seq="${seq}"]`).length, seq9);
check("t10 : seq rejoué → pas de doublon", dup10 === 1, `count=${dup10}`);

// --- Trigger 11 : trou de resync — seq manquant inséré à sa place. ---
await trig("t11-hole");
const base11 = await p.evaluate(() => window.__lastSeq);
await push("s1", { seq: base11 + 2, ts: "x", type: "agent_text", text: "newer" });
await sleep(200);
await push("s1", { seq: base11 + 1, ts: "x", type: "agent_text", text: "trou comblé" });
await sleep(300);
await p.evaluate((b) => { window.__lastSeq = b + 2; }, base11);
const order11 = await p.evaluate((b) => {
  const evs = [...document.querySelectorAll("#events .ev")];
  const i2 = evs.findIndex((e) => e.dataset.seq === String(b + 2));
  const i1 = evs.findIndex((e) => e.dataset.seq === String(b + 1));
  return { i1, i2, n: evs.length };
}, base11);
check("t11 : trou resync inséré en position (avant le +2)",
  order11.i1 >= 0 && order11.i1 === order11.i2 - 1, JSON.stringify(order11));

// --- Trigger 12 : session_cleared alors que le journal a du contenu —
// la vue est vidée puis re-peuplée par le filet (jamais laissée vide). ---
await trig("t12-cleared-belt");
await push("s1", { seq: 9999, ts: "x", type: "session_cleared" });
await sleep(1200);
s = await st();
check("t12 : cleared parasite → vue re-peuplée (journal intact)",
  s.n >= 5, JSON.stringify(s));

console.log("\nremoved:", JSON.stringify(await removed()));
console.log(`${pass} PASS, ${fail} FAIL`);
await b.close();
server.kill();
process.exit(fail ? 1 : 0);
