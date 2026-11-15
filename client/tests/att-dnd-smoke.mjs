// E2E « drag & drop du composer » — la détection accepte « Files » ET
// les drag URI (text/uri-list / x-special/gnome-copied-files) : le
// gestionnaire de fichiers Linux n'expose pas le type Files et ne
// fournit souvent aucun File object (bug #257 — le path s'insérait en
// texte). Au drop : files non vides → addFiles ; sinon les URI file://
// partent via clipboard_file_* ; une uri-list SANS file:// (lien
// http(s)…) n'est PAS interceptée — le drop retombe sur l'insertion
// texte par défaut.
// Usage : npm run build && node tests/att-dnd-smoke.mjs
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";

const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";

// Session unique + RPC enregistrés dans window.__rpc ; clipboard_file_*
// et create_path/write_file mockés pour que l'upload aboutisse sans
// vrai fichier — window.__CLIP trace ce qui a été demandé.
const MOCK = `(function () {
  const SESS = { id: "s1", conn: "ssh:local:22", agent: "devin", title: "DnD",
    cwd: "/w", created_at: "2026-10-20T10:00:00Z",
    available_modes: [], available_models: [], available_commands: [],
    config_options: [], state: "idle" };
  const CAPS = { version: "0.10.0", sessions: 1, files: true,
    agents: ["devin"], self_update: false, first_post: false,
    terminal: true, components: false, orchestrator: false,
    pinned: false, queue_steer: true, web: null,
    host: { cpu: 1, mem_used_mb: 100, mem_total_mb: 1024, load1: 0.1, rss_mb: 10 } };
  window.__CLIP = { stat: [], read: [], created: [], written: [] };
  window.__rpc = [];
  function rpc(method, args) {
    window.__rpc.push({ method, args });
    const p = args ?? {};
    switch (method) {
      case "list_sessions": return [SESS];
      case "list_agents": return [{ id: "devin", name: "Devin" }];
      case "session_info": return { meta: SESS, pending: [], alive: true, last_seq: 1 };
      case "events": return { events: [{ seq: 1, ts: "x", type: "agent_text", text: "hi" }], last_seq: 1 };
      case "queue_list": return { queue: [] };
      case "daemon_status": return CAPS;
      case "create_path":
        window.__CLIP.created.push(p.path);
        return { path: "/w/" + p.path };
      case "write_file":
        window.__CLIP.written.push(p.path);
        return null;
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
      if (cmd === "app_version") return p.then(() => "0.10.0");
      if (cmd === "plugin:event|listen") return p.then(() => ++cbId);
      if (cmd === "plugin:event|unlisten") return p.then(() => null);
      if (cmd.startsWith("plugin:")) return p.then(() => null);
      switch (cmd) {
        case "connect": return p.then(() => CAPS);
        case "call": return p.then(() => rpc(args.method, args.params ?? {}));
        case "subscribe": return p.then(() => null);
        case "saved_connections": case "ssh_hosts": return p.then(() => []);
        // Garde « seconde instance » du boot : null résolu ≠ rejeté →
        // otherInstances.length plantrait sans ce stub.
        case "other_instances": return p.then(() => []);
        case "ssh_pubkey": return p.then(() => "ssh-ed25519 AAAA x");
        case "clipboard_file_stat":
          window.__CLIP.stat.push(args.uri);
          return p.then(() => ({ name: "pic.png", size: 4 }));
        case "clipboard_file_read":
          window.__CLIP.read.push(args);
          return p.then(() => btoa("data"));
        case "check_update": return p.then(() => { throw new Error("offline"); });
        default: return p.then(() => null);
      }
    },
  };
})();`;

const server = spawn("npx", ["vite", "preview", "--port", "4201", "--strictPort"], { stdio: "ignore" });
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

await p.goto("http://localhost:4201/", { waitUntil: "networkidle0" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
// Sans connexion, #conn-view est l'écran d'accueil déjà ouvert — NE PAS
// cliquer #btn-connect (toggle → fermerait la vue).
await p.waitForFunction(
  () => {
    const v = document.getElementById("conn-view");
    return v && !v.hidden;
  },
  { timeout: 10000 },
);
await sleep(300);
await p.type("#c-host", "local");
await p.evaluate(() => document.querySelector("#c-go")?.click());
await sleep(1200);
await p.evaluate(() => document.querySelector(".sess")?.click());
await sleep(1200);

// Dispatch d'un DragEvent sur #chat (ou une cible précise via `on`) ;
// retourne false si preventDefault. `on` permet de viser #input pour
// le cas « drop sur zone éditable ».
const drag = (kind, mime, data, withFile = false, on = "#chat") => p.evaluate(
  ({ k, m, d, wf, sel }) => {
    const dt = new DataTransfer();
    if (wf) dt.items.add(new File(["hello"], "note.txt", { type: "text/plain" }));
    else if (m) dt.setData(m, d);
    const el = document.querySelector(sel);
    return el.dispatchEvent(new DragEvent(k, { dataTransfer: dt, bubbles: true, cancelable: true }));
  },
  { k: kind, m: mime, d: data, wf: withFile, sel: on },
);
const chips = () => p.evaluate(() => document.querySelectorAll("#attach-bar .att-chip").length);
const clip = () => p.evaluate(() => window.__CLIP);
const FILE_URI = "file:///home/dev/pics/pic.png";
const LINK_URI = "https://example.com/page";

// ---------- A. Drag URI file:// (gestionnaire Linux) ----------
await drag("dragenter", "text/uri-list", FILE_URI);
await drag("dragover", "text/uri-list", FILE_URI);
const hint = await p.evaluate(() => ({
  hint: !document.querySelector("#drop-hint").hidden,
  cls: document.querySelector("#chat").classList.contains("att-drop"),
}));
check("A1 drop-hint affiché + classe att-drop", hint.hint && hint.cls, JSON.stringify(hint));
const rA = await drag("drop", "text/uri-list", FILE_URI);
check("A2 drop file:// preventDefault", rA === false);
await sleep(700);
let c = await clip();
check("A3 URI file:// routée vers clipboard_file_stat", c.stat.includes(FILE_URI), JSON.stringify(c.stat));
check("A4 pièce uploadée via create_path", c.created.some((x) => x.includes("pic.png")), JSON.stringify(c.created));
check("A5 tranches lues via clipboard_file_read", c.read.length > 0);
check("A6 chip visible", (await chips()) === 1, String(await chips()));

// ---------- B. Drag URI http(s) — PAS un fichier ----------
const rB = await drag("dragover", "text/uri-list", LINK_URI);
check("B1 dragover lien preventDefault (highlight optimiste)", rB === false);
// Drop hors zone éditable : intercepté — sinon le défaut UA naviguerait
// la webview vers l'URL (régression review #335).
const rB2 = await drag("drop", "text/uri-list", LINK_URI);
check("B2 drop lien sur #chat (non-éditable) preventDefault", rB2 === false);
// Drop directement sur le textarea : non intercepté → insertion texte.
const rB3 = await drag("drop", "text/uri-list", LINK_URI, false, "#input");
check("B3 drop lien sur #input (éditable) NON preventDefault", rB3 === true);
// Drop sur une checkbox des settings (enfant de #chat, non-éditable) :
// intercepté — un input non-textuel ne peut pas recevoir de texte, le
// défaut UA y naviguerait aussi (review #335, round 2).
const rB4 = await drag("drop", "text/uri-list", LINK_URI, false, "#s-bganim");
check("B4 drop lien sur checkbox settings preventDefault", rB4 === false);
c = await clip();
check("B5 aucune lecture clipboard pour un lien", c.stat.length === 1, JSON.stringify(c.stat));
// Drop de lien après un dragenter : endDrag() doit masquer le hint —
// aucun dragleave n'est émis après un drop (review #335 round 4).
await drag("dragenter", "text/uri-list", LINK_URI);
const hintBefore = await p.evaluate(() => !document.querySelector("#drop-hint").hidden);
await drag("drop", "text/uri-list", LINK_URI, false, "#input");
const hintAfter = await p.evaluate(() => ({
  hint: !document.querySelector("#drop-hint").hidden,
  cls: document.querySelector("#chat").classList.contains("att-drop"),
}));
check("B6 drop lien → hint masqué + att-drop retiré",
  hintBefore && !hintAfter.hint && !hintAfter.cls,
  JSON.stringify({ hintBefore, hintAfter }));

// ---------- C. x-special/gnome-copied-files ----------
const rC = await drag("drop", "x-special/gnome-copied-files", FILE_URI);
check("C1 drop gnome-copied-files preventDefault", rC === false);
await sleep(700);
c = await clip();
check("C2 file:// via gnome-copied-files routée aussi",
  c.stat.filter((u) => u === FILE_URI).length === 2, JSON.stringify(c.stat));

// ---------- D. File object (chemin navigateur inchangé) ----------
const rD = await drag("drop", null, null, true);
check("D1 drop File preventDefault", rD === false);
await sleep(700);
check("D2 chip File ajouté (chemin addFiles)", (await chips()) === 3, String(await chips()));

await b.close();
server.kill();
console.log(fail ? `\n${fail} FAIL / ${pass + fail}` : `\nall ok (${pass})`);
process.exit(fail ? 1 : 0);
