// E2E « feedback immédiat sur respond » (#371) : à l'appui sur Allow/Reject
// (ou Answer/Ignore), la carte passe en .pending et les contrôles se
// désactivent AVANT le retour du RPC — pas d'impression de freeze, pas de
// double-envoi pendant le roundtrip. RPC en échec → .pending retiré,
// contrôles réactivés, jamais de .done. Succès → .done.
// Usage : npm run build && node tests/respond-pending-e2e.mjs
import puppeteer from "puppeteer-core";
import { startVitePreview } from "./lib/vite-preview.mjs";

const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";

const MOCK = `(function () {
  const SESS = { id: "s1", conn: "local", agent: "devin", title: "Pend",
    cwd: "/w", created_at: "2026-10-20T10:00:00Z",
    available_modes: [], available_models: [], available_commands: [],
    config_options: [], state: "working" };
  window.__rpc = [];
  // Portillon de test : window.__respond=args=>Promise — par défaut le
  // respond répond tout de suite.
  window.__respond = null;
  function rpc(method, args) {
    window.__rpc.push({ method, args });
    switch (method) {
      case "list_sessions": return [SESS];
      case "list_agents": return [{ id: "devin", name: "Devin" }];
      case "session_info": return { meta: SESS, pending: [], alive: true, last_seq: 10 };
      case "events": return { events: [{ seq: 1, ts: "x", type: "agent_text", text: "hi" }], last_seq: 1 };
      case "respond":
        return window.__respond ? window.__respond(args)
          : Promise.resolve({ answered: true });
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
const cardState = (req) => p.evaluate((r) => {
  const card = document.querySelector(`#events .ev.perm[data-req="${r}"]`);
  if (!card) return null;
  return {
    pending: card.classList.contains("pending"),
    done: card.classList.contains("done"),
    disabled: [...card.querySelectorAll("button, input")].every((c) => c.disabled),
    spin: !!card.querySelector(".spin"),
  };
}, req);

await p.goto(BASE + "/", { waitUntil: "networkidle0" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
await click("#btn-connect"); await sleep(300);
await p.select("#c-kind", "local");
await click("#c-go"); await sleep(1200);
await p.evaluate(() => document.querySelector(".sess")?.click());
await sleep(1200);

const PERM = {
  seq: 2, ts: "x", type: "permission_request", request_id: "pr-1",
  title: "Autoriser la commande ?", kind: "execute",
  options: [
    { option_id: "opt-allow", name: "Autoriser", kind: "allow_once" },
    { option_id: "opt-deny", name: "Refuser", kind: "reject_once" },
  ],
};
await pushEvent(PERM);
await sleep(400);

// --- Répond différé : état pending immédiat + verrou ----------------------
await p.evaluate(() => {
  window.__release = null;
  window.__respond = () => new Promise((r) => { window.__release = r; });
});
await click('#events .ev.perm[data-req="pr-1"] .perm-opts .btn-allow');
await sleep(200);
let st = await cardState("pr-1");
check("clic Allow → carte .pending immédiate", st?.pending === true, JSON.stringify(st));
check("boutons + inputs désactivés pendant l'envoi", st?.disabled === true, JSON.stringify(st));
check("spinner sur le bouton cliqué", st?.spin === true, JSON.stringify(st));
check("pas de .done pendant le roundtrip", st?.done === false, JSON.stringify(st));

// Second clic (même désactivé, et verrou `pending` côté renderer) : rien.
await click('#events .ev.perm[data-req="pr-1"] .perm-opts .btn-reject');
await click('#events .ev.perm[data-req="pr-1"] .perm-opts .btn-allow');
await sleep(200);
check("double-clic → un seul respond", (await respondCalls()).length === 1,
  JSON.stringify(await respondCalls()));

// Résolution du RPC → .done, .pending retiré.
await p.evaluate(() => window.__release({ answered: true }));
await sleep(400);
st = await cardState("pr-1");
check("respond OK → .done, .pending retiré",
  st?.done === true && st?.pending === false, JSON.stringify(st));

// --- Répond rejeté : retour à l'état actif, jamais .done ------------------
await pushEvent({ ...PERM, seq: 3, request_id: "pr-2" });
await sleep(400);
await p.evaluate(() => {
  window.__respond = () => Promise.reject(new Error("boom"));
});
await click('#events .ev.perm[data-req="pr-2"] .perm-opts .btn-reject');
await sleep(400);
st = await cardState("pr-2");
check("respond KO → .pending retiré, pas de .done",
  st?.pending === false && st?.done === false, JSON.stringify(st));
check("respond KO → contrôles réactivés", st?.disabled === false, JSON.stringify(st));

// Après l'échec, un nouveau clic repart (demande peut-être encore vive).
await p.evaluate(() => { window.__respond = null; });
await click('#events .ev.perm[data-req="pr-2"] .perm-opts .btn-allow');
await sleep(400);
st = await cardState("pr-2");
check("retry après échec → respond renvoyé puis .done",
  st?.done === true && (await respondCalls()).filter((c) => c.request_id === "pr-2").length === 2,
  JSON.stringify(await respondCalls()));

// --- Question à saisie libre : même verrou -------------------------------
await pushEvent({
  seq: 4, ts: "x", type: "question_request", request_id: "qr-1",
  message: "Une question ?",
});
await sleep(400);
await p.evaluate(() => {
  window.__release = null;
  window.__respond = () => new Promise((r) => { window.__release = r; });
});
await p.evaluate(() => {
  const card = document.querySelector('#events .ev.perm.question[data-req="qr-1"]');
  card.querySelector(".q-input").value = "42";
  card.querySelector(".btn-allow").click();
});
await sleep(200);
st = await cardState("qr-1");
check("question Answer → .pending + contrôles désactivés",
  st?.pending === true && st?.disabled === true, JSON.stringify(st));
await click('#events .ev.perm.question[data-req="qr-1"] .btn-reject');
await sleep(200);
check("question : Ignore pendant l'envoi → pas de second respond",
  (await respondCalls()).filter((c) => c.request_id === "qr-1").length === 1,
  JSON.stringify(await respondCalls()));
await p.evaluate(() => window.__release({ answered: true }));
await sleep(400);
st = await cardState("qr-1");
check("question respond OK → .done", st?.done === true, JSON.stringify(st));

check("zéro pageerror sur tout le scénario", pageErrors.length === 0,
  pageErrors.join(" | "));

await b.close();
server.kill();
console.log(`\n${pass} PASS, ${fail} FAIL`);
process.exit(fail ? 1 : 0);
