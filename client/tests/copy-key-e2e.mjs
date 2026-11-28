// E2E « Ctrl+C copie la sélection » (#370) — WebKitGTK n'exécute pas le
// copier/select-all natif sur du contenu NON éditable : le client branche
// keydown global → copyText + Ctrl+A scopé à #events. On vérifie :
//   - Ctrl+C sur une sélection dans .md → clipboard.writeText(texte)
//   - Ctrl+Insert (variante) idem
//   - sélection hors #events (sidebar) → copiée quand même (pas de borne)
//   - target=input (composer) → PAS intercepté
//   - sélection collapsée → aucun appel clipboard
//   - Ctrl+A avec ancre dans #events → tout le fil sélectionné ;
//     ancre hors #events → sélection inchangée
// Usage : npm run build && node tests/copy-key-e2e.mjs
import puppeteer from "puppeteer-core";
import { startVitePreview } from "./lib/vite-preview.mjs";

const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";

const MOCK = `(function () {
  const J = [
    { seq: 1, ts: "x", type: "agent_text", text: "Premier **message** pour le test" },
    { seq: 2, ts: "x", type: "agent_text", text: "Deuxième message ici" },
    { seq: 3, ts: "x", type: "agent_text", text: "Troisième et dernier" },
  ];
  const SESS = { id: "s1", conn: "local", agent: "devin", title: "CopyTest",
    cwd: "/home/agent/api", created_at: "2026-10-20T10:00:00Z",
    available_modes: [], available_models: [], available_commands: [],
    config_options: [], state: "idle" };
  function rpc(method, args) {
    switch (method) {
      case "list_sessions": return [SESS];
      case "list_agents": return [{ id: "devin", name: "Devin" }];
      case "session_info": return { meta: SESS, pending: [], alive: true, last_seq: J.length };
      case "events": {
        const evs = J.filter((e) =>
          (args.after_seq == null || e.seq > args.after_seq) &&
          (args.before_seq == null || e.seq < args.before_seq));
        return { events: evs, last_seq: J.length };
      }
      case "daemon_status": return { version: "0.6.0", sessions: 1 };
      case "pr_list": return [];
      default: return null;
    }
  }
  let cbId = 0;
  const callbacks = new Map();
  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: "main" }, currentWebview: { label: "main" } },
    transformCallback(cb) { const id = ++cbId; callbacks.set(id, cb); return id; },
    runCallback(id, data) { callbacks.get(id)?.(data); },
    convertFileSrc(p) { return p; },
    invoke(cmd, args) {
      const p = Promise.resolve();
      if (cmd === "platform") return p.then(() => "linux");
      if (cmd === "app_version") return p.then(() => "0.6.0");
      if (cmd === "plugin:event|listen") return p.then(() => ++cbId);
      if (cmd === "plugin:event|unlisten") return p.then(() => null);
      if (cmd.startsWith("plugin:")) return p.then(() => null);
      switch (cmd) {
        case "connect": return p.then(() => ({ version: "0.6.0", sessions: 1, servers: 1 }));
        case "call": return p.then(() => rpc(args.method, args.params ?? {}));
        case "subscribe": return p.then(() => null);
        case "saved_connections": case "ssh_hosts": return p.then(() => []);
        case "other_instances": return p.then(() => []);
        case "ssh_pubkey": return p.then(() => "ssh-ed25519 AAAA x");
        case "check_update": return p.then(() => { throw new Error("offline"); });
        default: return p.then(() => null);
      }
    },
  };
  // Clipboard espion : navigator.clipboard peut être non configurable —
  // on tente defineProperty puis repli sur l'override de writeText.
  window.__copied = null;
  window.__copyCalls = 0;
  const spy = (t) => { window.__copied = t; window.__copyCalls++; return Promise.resolve(); };
  try {
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText: spy },
    });
  } catch {
    try { navigator.clipboard.writeText = spy; } catch {}
  }
})();`;

const { server, base: BASE } = await startVitePreview();
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
const clip = () => p.evaluate(() => ({ t: window.__copied, n: window.__copyCalls }));
const resetClip = () => p.evaluate(() => { window.__copied = null; window.__copyCalls = 0; });
// Sélectionne [start,end) du premier nœud texte trouvé sous `rootSel`.
const selectText = (rootSel, start, end) => p.evaluate((rs, a, b2) => {
  const root = document.querySelector(rs);
  if (!root) return null;
  const it = document.createNodeIterator(root, NodeFilter.SHOW_TEXT);
  let tn;
  while ((tn = it.nextNode())) {
    if (tn.textContent.length > a) break;
  }
  if (!tn) return null;
  const r = document.createRange();
  r.setStart(tn, a);
  r.setEnd(tn, Math.min(b2, tn.length));
  const sel = getSelection();
  sel.removeAllRanges();
  sel.addRange(r);
  return sel.toString();
}, rootSel, start, end);
const key = (k, opts = {}, targetSel = null) => p.evaluate((key_, o, ts) => {
  const t = ts ? document.querySelector(ts) : document.body;
  t.dispatchEvent(new KeyboardEvent("keydown", { key: key_, bubbles: true, cancelable: true, ...o }));
}, k, opts, targetSel);

await p.goto(BASE + "/", { waitUntil: "networkidle0" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
await click("#btn-connect"); await sleep(300);
await p.type("#c-host", "local"); await click("#c-go"); await sleep(1200);
await p.evaluate(() => document.querySelector(".sess")?.click());
await p.waitForSelector("#events .ev.msg.agent .md", { timeout: 10000 });
await sleep(300);

// 1. Ctrl+C sur une sélection dans .md → clipboard.writeText(texte).
const picked = await selectText("#events .ev.msg.agent .md", 0, 11);
check("sélection .md posée", !!picked && picked.length > 0, String(picked));
await resetClip();
await key("c", { ctrlKey: true }, "#events .md");
await sleep(150);
let c = await clip();
check("Ctrl+C → writeText(texte sélectionné)", c.n === 1 && c.t === picked, JSON.stringify(c));

// 2. Ctrl+Insert (variante copy) → même chemin.
await resetClip();
await key("Insert", { ctrlKey: true }, "#events .md");
await sleep(150);
c = await clip();
check("Ctrl+Insert → writeText", c.n === 1 && c.t === picked, JSON.stringify(c));

// 3. Sélection hors #events (ligne session de la sidebar, non éditable)
//    → copiée quand même : la cible non éditable suffit, pas de borne
//    à #events.
const side = await p.evaluate(() => {
  const el = document.querySelector(".sess");
  // Les lignes session sont user-select:none (comportement UI) — on
  // force la sélection pour isoler le test du handler clavier.
  el.style.setProperty("user-select", "text");
  const r = document.createRange();
  r.selectNodeContents(el);
  const sel = getSelection();
  sel.removeAllRanges();
  sel.addRange(r);
  return sel.toString();
});
check("sélection sidebar posée", !!side && side.includes("CopyTest"), String(side).slice(0, 60));
await resetClip();
await key("c", { ctrlKey: true }, ".sess");
await sleep(150);
c = await clip();
check("Ctrl+C hors #events → writeText", c.n === 1 && c.t === side, JSON.stringify({ c, side }));

// 4. Target = input éditable (composer) → PAS intercepté : le copier
//    natif reste maître, aucun appel writeText de notre handler.
await selectText("#events .ev.msg.agent .md", 0, 11); // sélection active
await resetClip();
await key("c", { ctrlKey: true }, "#input");
await sleep(150);
c = await clip();
check("Ctrl+C target=input → non intercepté", c.n === 0, JSON.stringify(c));

// 5. Sélection collapsée → rien à copier, aucun appel.
await p.evaluate(() => getSelection().removeAllRanges());
await resetClip();
await key("c", { ctrlKey: true }, "#events .md");
await sleep(150);
c = await clip();
check("Ctrl+C sans sélection → aucun appel", c.n === 0, JSON.stringify(c));

// 6. Ctrl+A avec ancre dans #events → tout le fil sélectionné.
await selectText("#events .ev.msg.agent .md", 0, 5);
await key("a", { ctrlKey: true }, "#events .md");
await sleep(150);
const all = await p.evaluate(() => {
  const sel = getSelection();
  return {
    txt: sel.toString(),
    inside: !!sel.anchorNode && document.getElementById("events").contains(sel.anchorNode),
  };
});
check("Ctrl+A fil → ancre dans #events", all.inside);
check("Ctrl+A fil → toute la discussion sélectionnée",
  all.txt.includes("Premier") && all.txt.includes("Deuxième") && all.txt.includes("Troisième"),
  all.txt.slice(0, 80));

// 7. Ctrl+A avec ancre hors #events → sélection inchangée (défaut préservé).
await p.evaluate(() => {
  const el = document.querySelector(".sess");
  el.style.setProperty("user-select", "text");
  const r = document.createRange();
  r.selectNodeContents(el);
  const sel = getSelection();
  sel.removeAllRanges();
  sel.addRange(r);
});
const before = await p.evaluate(() => getSelection().toString());
await key("a", { ctrlKey: true }, ".sess");
await sleep(150);
const after = await p.evaluate(() => getSelection().toString());
check("Ctrl+A hors fil → sélection inchangée", after === before, JSON.stringify({ before, after }));

console.log(`\n${pass} PASS, ${fail} FAIL`);
await b.close(); server.kill();
process.exit(fail ? 1 : 0);
