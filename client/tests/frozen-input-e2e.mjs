// E2E « zone de saisie gelée » — repro du bug + filets de sécurité.
//  1. new_session lent : dlg-new modal → #input inerte (attendu by design),
//     Échap détache → frappe à nouveau possible, la session s'ouvre ensuite.
//  2. ensure_daemon lent : progression inline dans conn-view (plus de
//     dlg-daemon modal) → Échap ferme la vue (op continue en fond).
//  3. Watchdog : inert résiduel retiré ; dialog open-invisible fermé.
// Usage : npm run build && node tests/frozen-input-e2e.mjs
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";

const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";

const MOCK = `(function () {
  const SESS = [{ id: "s1", conn: "local", agent: "devin", title: "Main",
    cwd: "/w", created_at: "2026-10-20T10:00:00Z", available_modes: [],
    available_models: [], available_commands: [], config_options: [],
    state: "idle" }];
  window.__logs = [];
  let newCalls = 0, connCalls = 0;
  const delay = (ms, v) => new Promise((r) => setTimeout(() => r(v), ms));
  function rpc(method, args) {
    switch (method) {
      case "list_sessions": return SESS;
      case "list_agents": return [{ id: "devin", name: "Devin" }];
      case "session_info": {
        const m = SESS.find((s) => s.id === args.session) ?? SESS[0];
        return { meta: m, pending: [], alive: true, last_seq: 1 };
      }
      case "events": return { events: [{ seq: 1, ts: "x", type: "agent_text", text: "hi" }], last_seq: 1 };
      case "queue_list": return { queue: [] };
      case "new_session":
        // Spawn long : la réponse met ~2.5 s — fenêtre du gel reproduit.
        return delay(2500).then(() => {
          const s = { id: "s2", conn: "local", agent: "devin",
            title: args.title ?? "spawned", cwd: args.cwd, created_at: "x",
            available_modes: [], available_models: [], available_commands: [],
            config_options: [], state: "starting" };
          SESS.push(s);
          newCalls++;
          return { session: s.id };
        });
      case "daemon_status": return { version: "0.6.0", sessions: SESS.length };
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
      if (cmd === "app_version") return p.then(() => "0.6.0");
      if (cmd === "plugin:event|listen") {
        window.__acListeners[args.event] = args.handler;
        return p.then(() => ++cbId);
      }
      if (cmd === "plugin:event|unlisten") return p.then(() => null);
      if (cmd === "log_write") {
        window.__logs.push(args.level + ":" + args.msg);
        return p.then(() => null);
      }
      if (cmd.startsWith("plugin:")) return p.then(() => null);
      switch (cmd) {
        case "connect":
          // 1er essai SSH = démon absent → bootstrap install ; les suivants OK.
          connCalls++;
          return p.then(() => {
            if (args.cfg.kind === "ssh" && !window.__sshTried) {
              window.__sshTried = true;
              throw "connection refused";
            }
            return { version: "0.6.0", sessions: 1, servers: 1 };
          });
        case "ensure_daemon": return delay(3000).then(() => "installed:0.6.0");
        case "call": return p.then(() => rpc(args.method, args.params ?? {}));
        case "subscribe": return p.then(() => null);
        case "saved_connections": case "ssh_hosts": return p.then(() => []);
        case "ssh_pubkey": return p.then(() => "ssh-ed25519 AAAA x");
        case "check_update": return p.then(() => { throw new Error("offline"); });
        case "get_version": return p.then(() => "0.6.0");
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
p.on("pageerror", (e) => console.log("[pageerror]", e.message));
await p.evaluateOnNewDocument(MOCK);

let pass = 0, fail = 0;
const check = (name, ok, extra = "") => {
  if (ok) { pass++; console.log("PASS", name); }
  else { fail++; console.log("FAIL", name, extra); }
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const click = (sel) => p.evaluate((s) => document.querySelector(s)?.click(), sel);
const typeInput = async (txt) => {
  await p.evaluate((t) => {
    const i = document.getElementById("input");
    i.focus();
    i.value = t;
    i.dispatchEvent(new Event("input", { bubbles: true }));
  }, txt);
};
const inputVal = () => p.evaluate(() => document.getElementById("input").value);
const dlgOpen = (id) => p.evaluate((i) => !!document.querySelector(`#${i}[open]`), id);
const typedDuringModal = async (id) => {
  // Une frappe trusted sur un élément inerte ne le remplit pas —
  // équivalent du « je tape et rien ne se passe » du rapport.
  const before = await inputVal();
  await p.evaluate(() => document.getElementById("input").focus());
  await p.keyboard.type("x");
  const after = await inputVal();
  return { before, after, blocked: after === before };
};

await p.goto("http://localhost:4194/", { waitUntil: "networkidle0" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
await click("#btn-connect"); await sleep(300);
await p.evaluate(() => {
  (document.getElementById("c-kind")).value = "tcp";
  document.getElementById("c-kind").dispatchEvent(new Event("change"));
});
await p.type("#c-host", "127.0.0.1:7331");
await click("#c-go"); await sleep(1200);
await p.evaluate(() => document.querySelector(".sess")?.click());
await sleep(1000);

// --- 1. dlg-new pendant un spawn lent --------------------------------------
await click("#btn-new-session");
await p.waitForSelector("#dlg-new[open]", { timeout: 10000 });
await p.evaluate(() => { document.getElementById("f-cwd").value = "/w/proj"; });
await click("#f-create");
await sleep(400); // new_session mocké à 2.5 s — le dialogue est en attente.

check("spawn lent : dlg-new ouvert (modal)", await dlgOpen("dlg-new"));
check("statut « création longue » affiché", await p.evaluate(
  () => !document.getElementById("f-status").hidden));
let r = await typedDuringModal("dlg-new");
check("spawn en cours : #input inerte (attendu)", r.blocked, JSON.stringify(r));

await p.keyboard.press("Escape");
await sleep(300);
check("Échap : dlg-new détaché", !(await dlgOpen("dlg-new")));
await typeInput("hello");
check("Échap : #input à nouveau accessible", (await inputVal()) === "hello",
  await inputVal());

// La session spawnée arrive quand même (créée côté démon) : sidebar + ouverture.
await sleep(2600);
const side = await p.evaluate(() =>
  [...document.querySelectorAll("#session-list .sess")].map((e) => e.textContent));
check("session spawnée apparue après détachement", side.some((x) => /proj/i.test(x)),
  JSON.stringify(side));

// --- 2. progression inline pendant ensure_daemon lent → Échap détache ----
await click("#btn-connect"); await sleep(300);
check("vue connexion ouverte (overlay #chat, pas dialog)",
  await p.evaluate(() => !document.getElementById("conn-view").hidden
    && !document.querySelector("dialog#dlg-conn")));
await p.evaluate(() => {
  document.getElementById("c-kind").value = "ssh";
  document.getElementById("c-kind").dispatchEvent(new Event("change"));
});
await p.evaluate(() => { document.getElementById("c-host").value = "me@remote"; });
await click("#c-go");
await sleep(700);
// connect ssh échoue → confirmation d'install (dlg-dupdate).
check("install démon proposée", await dlgOpen("dlg-dupdate"));
await click("#dupdate-go");
await sleep(600);
check("progression inline visible (#cv-progress, pas dlg-daemon)",
  await p.evaluate(() => !document.getElementById("cv-progress").hidden
    && !document.querySelector("#dlg-daemon")));
r = await typedDuringModal("dlg-daemon");
check("ensure_daemon : #input inerte sous la vue", r.blocked, JSON.stringify(r));
await p.keyboard.press("Escape");
await sleep(300);
check("Échap : vue connexion fermée (op détachée)",
  await p.evaluate(() => document.getElementById("conn-view").hidden));
await typeInput("bg");
check("Échap : #input à nouveau accessible", (await inputVal()) === "bg",
  await inputVal());
// ensure_daemon continue → retry connect → connexion aboutie — la vue
// reste fermée (déjà close, rien à re-pop) et le bind réussit.
await sleep(3500);
check("opération détachée menée à terme (sessions listées)",
  await p.evaluate(() => document.querySelectorAll("#session-list .sess").length > 0));

// --- 3. Watchdog ------------------------------------------------------------
await p.evaluate(() => { document.getElementById("events").inert = true; });
await sleep(2600);
check("watchdog : inert résiduel retiré", await p.evaluate(
  () => !document.getElementById("events").inert));
const logs = await p.evaluate(() => window.__logs.join("\n"));
check("watchdog : réparation loguée", /inert résiduel/.test(logs),
  logs.split("\n").filter((l) => l.includes("watchdog")).join(" | "));

await p.evaluate(() => {
  const d = document.getElementById("dlg-about");
  d.showModal();
  d.style.display = "none"; // open mais invisible = gel silencieux
});
await sleep(2600);
check("watchdog : dialog invisible fermé", !(await dlgOpen("dlg-about")));
const logs2 = await p.evaluate(() => window.__logs.join("\n"));
check("watchdog : dialog invisible logué", /invisible → fermé/.test(logs2));
check("modal open logué dans client.log", /modal \+#dlg-about/.test(logs2));

console.log(`\n${pass} PASS, ${fail} FAIL`);
await b.close(); server.kill();
process.exit(fail ? 1 : 0);
