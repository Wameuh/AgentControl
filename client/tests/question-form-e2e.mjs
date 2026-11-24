// E2E « question_request à schéma » — le vrai payload multi-select de la
// session s-795df974 (3 radios + 4 checks + 2 champs libres) se rend et
// produit la réponse {question_0, question_1:[…]} ; les schémas dégradés
// (oneOf/anyOf avec null/true/false, items non-objet, propriété non-objet,
// schéma sans properties) ne jettent pas ; une question_resolved à
// answer objet affiche du JSON lisible, pas « [object Object] ».
// Usage : npm run build && node tests/question-form-e2e.mjs
import puppeteer from "puppeteer-core";
import { startVitePreview } from "./lib/vite-preview.mjs";

const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";

const MOCK = `(function () {
  const SESS = { id: "s1", conn: "local", agent: "devin", title: "QForm",
    cwd: "/w", created_at: "2026-10-20T10:00:00Z",
    available_modes: [], available_models: [], available_commands: [],
    config_options: [], state: "working" };
  window.__rpc = [];
  function rpc(method, args) {
    window.__rpc.push({ method, args });
    switch (method) {
      case "list_sessions": return [SESS];
      case "list_agents": return [{ id: "devin", name: "Devin" }];
      case "session_info": return { meta: SESS, pending: [], alive: true, last_seq: 10 };
      case "events": return { events: [{ seq: 1, ts: "x", type: "agent_text", text: "hi" }], last_seq: 1 };
      case "respond": return { answered: true };
      case "daemon_status": return { version: "0.11.0", sessions: 1 };
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
      if (cmd === "app_version") return p.then(() => "0.11.0");
      if (cmd === "plugin:event|listen") {
        window.__acListeners[args.event] = args.handler;
        return p.then(() => ++cbId);
      }
      if (cmd === "plugin:event|unlisten") return p.then(() => null);
      if (cmd.startsWith("plugin:")) return p.then(() => null);
      switch (cmd) {
        case "connect": return p.then(() => ({ version: "0.11.0", sessions: 1, servers: 1 }));
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

const { server, base: BASE } = await startVitePreview();
const b = await puppeteer.launch({ executablePath: CHROMIUM, headless: true, args: ["--no-sandbox"] });
const p = await b.newPage();
await p.setViewport({ width: 1280, height: 860 });
const pageErrors = [];
p.on("pageerror", (e) => { pageErrors.push(e.message); console.log("[pageerror]", e.message); });
await p.evaluateOnNewDocument(MOCK);

let pass = 0, fail = 0;
const check = (name, ok, extra = "") => {
  if (ok) { pass++; console.log("PASS", name); }
  else { fail++; console.log("FAIL", name, extra); }
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const click = (sel) => p.evaluate((s) => document.querySelector(s)?.click(), sel);
const pushEvent = (ev) => p.evaluate((e) => {
  const id = window.__acListeners["session-event"];
  window.__TAURI_INTERNALS__.runCallback(id, { event: "session-event", payload: {
    session: "s1", event: e } });
}, ev);
const respondCalls = () => p.evaluate(() =>
  window.__rpc.filter((c) => c.method === "respond").map((c) => c.args));

await p.goto(BASE + "/", { waitUntil: "networkidle0" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
await click("#btn-connect"); await sleep(300);
await p.select("#c-kind", "local");
await click("#c-go"); await sleep(1200);
await p.evaluate(() => document.querySelector(".sess")?.click());
await sleep(1200);

// --- Schéma réel (session s-795df974, qr-56058b97) -------------------------
const QUESTION = {
  seq: 2, ts: "2026-11-24T15:52:15.559Z", type: "question_request",
  request_id: "qr-56058b97", message: "Please answer the following questions.",
  schema: { properties: {
    question_0: { description: "Quel type de projet fictif voulez-vous que je planifie ?",
      oneOf: [
        { const: "Application mobile", description: "Plan fictif pour une app mobile de suivi de tâches", title: "Application mobile" },
        { const: "API backend", description: "Plan fictif pour une API REST avec base de données", title: "API backend" },
        { const: "Site web vitrine", description: "Plan fictif pour un site statique pour une PME", title: "Site web vitrine" }],
      title: "Projet", type: "string" },
    question_0_custom: { description: "Type your own answer (optional).", title: "Other", type: "string" },
    question_1: { description: "Quels éléments voulez-vous inclure dans le plan fictif ?",
      items: { anyOf: [
        { const: "Étapes détaillées", description: "Liste d'étapes numérotées", title: "Étapes détaillées" },
        { const: "Risques", description: "Section risques", title: "Risques" },
        { const: "Tests", description: "Section tests", title: "Tests" },
        { const: "Calendrier", description: "Estimation des délais", title: "Calendrier" }] },
      title: "Contenu", type: "array" },
    question_1_custom: { description: "Type your own answer (optional).", title: "Other", type: "string" },
  }, type: "object" },
};
await pushEvent(QUESTION);
await sleep(500);

const formInfo = await p.evaluate(() => {
  const f = document.querySelector("#events .q-form");
  return {
    form: !!f,
    radios: document.querySelectorAll('#events [data-key="question_0"] input[type=radio]').length,
    checks: document.querySelectorAll('#events [data-key="question_1"] input[type=checkbox]').length,
    free: document.querySelectorAll("#events .q-field input.q-free").length,
  };
});
check("schéma réel → .q-form rendu", formInfo.form);
check("3 radios (oneOf)", formInfo.radios === 3, JSON.stringify(formInfo));
check("4 checks (items.anyOf)", formInfo.checks === 4, JSON.stringify(formInfo));
check("2 champs libres (*_custom)", formInfo.free === 2, JSON.stringify(formInfo));

// Interaction : 1 radio + 2 checks → Répondre → answer = objet exact.
await p.evaluate(() => {
  document.querySelector('#events [data-key="question_0"] input[type=radio]')?.click();
  const checks = [...document.querySelectorAll('#events [data-key="question_1"] input[type=checkbox]')];
  checks[0]?.click(); checks[2]?.click();
  document.querySelector('#events .ev.perm.question .btn-allow')?.click();
});
await sleep(400);
const resp = await respondCalls();
const last = resp[resp.length - 1];
check("respond émis pour qr-56058b97",
  last?.request_id === "qr-56058b97", JSON.stringify(resp));
check("answer = {question_0, question_1:[2 choix]}",
  JSON.stringify(last?.answer) === JSON.stringify({
    question_0: "Application mobile",
    question_1: ["Étapes détaillées", "Tests"],
  }), JSON.stringify(last?.answer));

// --- Schémas dégradés : entrées non-objet dans oneOf/anyOf/items ----------
await pushEvent({
  seq: 3, ts: "x", type: "question_request", request_id: "qr-deg",
  message: "Schéma tordu",
  schema: { properties: {
    // null/true sont des schémas JSON valides — ne doivent pas jeter.
    weird_oneof: { type: "string",
      oneOf: [null, true, { const: "ok", title: "OK" }] },
    weird_items: { type: "array",
      items: { anyOf: [false, { const: 1, title: "Un" }] } },
    // items non-objet + propriété non-objet → champs libres.
    bad_items: { type: "array", items: "nope" },
    not_an_object: true,
  } },
});
await sleep(400);
const deg = await p.evaluate(() => {
  const card = document.querySelector('#events .ev.perm.question[data-req="qr-deg"]');
  return {
    radios: card?.querySelectorAll('[data-key="weird_oneof"] input[type=radio]').length ?? -1,
    checks: card?.querySelectorAll('[data-key="weird_items"] input[type=checkbox]').length ?? -1,
    free: card?.querySelectorAll("input.q-free").length ?? -1,
  };
});
check("oneOf [null,true,{ok}] → 1 radio, pas de crash", deg.radios === 1, JSON.stringify(deg));
check("items.anyOf [false,{1}] → 1 check", deg.checks === 1, JSON.stringify(deg));
check("items non-objet + prop true → 2 champs libres", deg.free === 2, JSON.stringify(deg));

// --- Schéma sans properties → repli saisie JSON libre ----------------------
await pushEvent({
  seq: 4, ts: "x", type: "question_request", request_id: "qr-free",
  message: "Pas de schéma", schema: {},
});
await sleep(400);
check("schema sans properties → .q-input libre (pas de .q-form)",
  await p.evaluate(() => {
    const card = document.querySelector('#events .ev.perm.question[data-req="qr-free"]');
    return !!card?.querySelector(".q-input") && !card.querySelector(".q-form");
  }));

// --- question_resolved à answer objet → JSON lisible -----------------------
await pushEvent({
  seq: 5, ts: "x", type: "question_resolved", request_id: "qr-56058b97",
  answer: { question_0: "Application mobile", question_1: ["Étapes détaillées", "Tests"] },
});
await sleep(400);
const resolvedTxt = await p.evaluate(() =>
  [...document.querySelectorAll("#events .ev.resolved")]
    .map((n) => n.textContent).join("\n"));
check("resolved affiche le JSON de l'answer",
  resolvedTxt.includes('"question_0"') && resolvedTxt.includes("Application mobile"),
  resolvedTxt);
check("pas de « [object Object] »", !resolvedTxt.includes("[object Object]"), resolvedTxt);

check("zéro pageerror sur tout le scénario", pageErrors.length === 0,
  pageErrors.join(" | "));

await b.close();
server.kill();
console.log(`\n${pass} PASS, ${fail} FAIL`);
process.exit(fail ? 1 : 0);
