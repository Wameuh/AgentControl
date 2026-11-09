// E2E « rendu Markdown » — viewer de fichiers : bascule rendu/source,
// liens .flink relatifs, coloration des blocs de code, mode édition.
// Mock Tauri avec read_file paginé (format {offset,size,data(base64)}).
// Usage : npm run build && node tests/md-render-e2e.mjs
//   (chromium via $CHROMIUM ou /usr/bin/chromium)
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";

const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";

const MOCK = `(function () {
  const b64 = (s) => btoa(unescape(encodeURIComponent(s)));
  const SESSIONS = [
    { id: "s1", conn: "local", agent: "devin", title: "Refactor API",
      cwd: "/home/agent/api", created_at: "2026-10-20T10:00:00Z",
      available_modes: [], available_models: [], available_commands: [],
      config_options: [], state: "idle" },
  ];
  const FILES = {
    "README.md":
      "# AgentControl\\n\\nDoc du **projet** — voir docs/deep.md et client.ts.\\n\\n" +
      "- alpha\\n- beta\\n\\n" +
      "\`\`\`ts\\nconst n: number = 41 + 1; // compte\\n\`\`\`\\n\\n" +
      "| col | val |\\n|-----|-----|\\n| a   | 1   |\\n\\n> note importante\\n",
    "docs/deep.md":
      "## Deep\\n\\nRetour racine : ../client.ts\\n\\nEt ref README.md\\n",
    "client.ts": "const x = 1;\\n// commentaire\\n",
  };
  const DIRS = {
    "": { dir: "/home/agent/api", entries: [
      { name: "docs", dir: true, size: 0 },
      { name: "README.md", dir: false, size: 300 },
      { name: "bad.md", dir: false, size: 20 },
      { name: "client.ts", dir: false, size: 30 },
    ] },
    "docs": { dir: "/home/agent/api/docs", entries: [
      { name: "deep.md", dir: false, size: 80 },
    ] },
  };
  function rpc(method, args) {
    switch (method) {
      case "list_sessions": return SESSIONS;
      case "list_agents": return [{ id: "devin", name: "Devin" }];
      case "session_info": return { meta: SESSIONS[0], pending: [], alive: true, last_seq: 0 };
      case "events": return { events: [], last_seq: 0 };
      case "list_dir": return DIRS[args.path] ?? { dir: "/home/agent/api/" + args.path, entries: [] };
      case "read_file": {
        if (args.path === "bad.md") throw new Error("permission denied");
        const f = FILES[args.path];
        if (f == null) throw new Error("no such file " + args.path);
        if (args.limit != null) {
          const slice = f.slice(args.offset, args.offset + args.limit);
          return { path: "/home/agent/api/" + args.path, offset: args.offset,
                   size: f.length, data: b64(slice), mtime_ms: 1 };
        }
        return { content: f, truncated: false, path: "/home/agent/api/" + args.path };
      }
      case "write_file": {
        const f = FILES[args.path];
        if (f == null) throw new Error("no such file " + args.path);
        if (args.if_mtime_ms != null && args.if_mtime_ms !== 1) {
          throw new Error("conflit: fichier modifié sur disque");
        }
        const bytes = Uint8Array.from(atob(args.data ?? ""), (c) => c.charCodeAt(0));
        const text = new TextDecoder().decode(bytes);
        FILES[args.path] = f.slice(0, args.offset) + text + f.slice(args.offset + (args.delete_len ?? 0));
        return { path: "/home/agent/api/" + args.path, size: FILES[args.path].length, mtime_ms: 2 };
      }
      case "daemon_status": return { version: "9.9.9", sessions: 1 };
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
      if (cmd === "app_version") return p.then(() => "9.9.9");
      if (cmd.startsWith("plugin:")) return p.then(() => null);
      switch (cmd) {
        case "connect": return p.then(() => ({ version: "9.9.9", sessions: 1, servers: 1 }));
        case "call": return p.then(() => rpc(args.method, args.params ?? args));
        case "subscribe": return p.then(() => null);
        case "saved_connections": return p.then(() => []);
        case "ssh_hosts": return p.then(() => []);
        case "ssh_pubkey": return p.then(() => "ssh-ed25519 AAAA x");
        default: return p.then(() => null);
      }
    },
  };
})();`;

const server = spawn("npx", ["vite", "preview", "--port", "4185", "--strictPort"], { stdio: "ignore" });
await new Promise((r) => setTimeout(r, 1800));
const b = await puppeteer.launch({ executablePath: CHROMIUM, headless: "new", args: ["--no-sandbox"] });
const p = await b.newPage();
await p.setViewport({ width: 1280, height: 860 });
p.on("console", (m) => { if (m.type() === "error") console.log("[page]", m.text()); });
await p.evaluateOnNewDocument(MOCK);

let pass = 0, fail = 0;
const check = (name, ok, extra = "") => {
  if (ok) { pass++; console.log("PASS", name); }
  else { fail++; console.log("FAIL", name, extra); }
};
const vis = (sel) => p.evaluate((s) => {
  const el = document.querySelector(s);
  return el && !el.hidden && el.getClientRects().length > 0;
}, sel);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const click = (sel) => p.evaluate((s) => document.querySelector(s)?.click(), sel);

// Panneau assez large → split côte-à-côte (fv-row) en édition md.
await p.evaluateOnNewDocument(() => localStorage.setItem("ac-filepanel-w", "1100"));
await p.goto("http://localhost:4185/", { waitUntil: "networkidle0" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
await click("#btn-connect"); await sleep(300);
await p.type("#c-host", "me@pi5.local"); await click("#c-go"); await sleep(900);
await p.evaluate(() => [...document.querySelectorAll(".sess")].find((s) => s.textContent.includes("Refactor"))?.click());
await sleep(900);
await click("#btn-files"); await sleep(800);

// Ouvre README.md dans le viewer
await p.evaluate(() => [...document.querySelectorAll(".fnode")].find((n) => n.textContent.includes("README.md"))?.click());
await sleep(700);

check("fv-md visible pour .md", await vis("#fv-md"));
check("file-md affiché (défaut = rendu)", await vis("#file-md"));
check("file-view (source) masqué", !(await vis("#file-view")));
const mdHtml = await p.evaluate(() => document.querySelector("#file-md").innerHTML);
check("rendu contient un titre mdh", mdHtml.includes('class="mdh mdh1"'));
check("rendu contient la table", mdHtml.includes('class="mdt"'));
check("rendu contient le code block", mdHtml.includes('class="md-code"'));
check("rendu contient blockquote", mdHtml.includes("<blockquote>"));
check("code block coloré (syn-kw)", mdHtml.includes('syn-kw'));
const kwColor = await p.evaluate(() => {
  const el = document.querySelector("#file-md .syn-kw");
  return el ? getComputedStyle(el).color : null;
});
check("syn-kw a une couleur", !!kwColor && kwColor !== "rgb(0, 0, 0)", kwColor ?? "absent");
check("flink dans le rendu", mdHtml.includes('class="flink"'));
check("localStorage ac-md-render = 1", (await p.evaluate(() => localStorage.getItem("ac-md-render"))) !== "0");

// Bascule → source
await click("#fv-md"); await sleep(300);
check("source affichée après toggle", await vis("#file-view"));
check("file-md masqué après toggle", !(await vis("#file-md")));
check("pref persistée à 0", (await p.evaluate(() => localStorage.getItem("ac-md-render"))) === "0");
const srcHtml = await p.evaluate(() => document.querySelector("#file-view").textContent);
check("source = texte brut markdown", srcHtml.includes("# AgentControl"));

// Re-bascule → rendu (les deux rendus coexistent, pas de re-fetch)
await click("#fv-md"); await sleep(300);
check("rendu ré-affiché", await vis("#file-md"));
check("pref persistée à 1", (await p.evaluate(() => localStorage.getItem("ac-md-render"))) === "1");

// Lien .flink relatif (docs/deep.md) → ouvre ce fichier
await p.evaluate(() => {
  const el = [...document.querySelectorAll("#file-md .flink")].find((f) => f.dataset.path === "docs/deep.md");
  el?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
});
await sleep(700);
check("flink → docs/deep.md ouvert", (await p.evaluate(() => document.querySelector("#file-path").textContent)) === "docs/deep.md",
  await p.evaluate(() => document.querySelector("#file-path").textContent));

// Lien relatif « .. » depuis docs/ → client.ts à la racine
await p.evaluate(() => {
  const el = [...document.querySelectorAll("#file-md .flink")].find((f) => f.dataset.path === "../client.ts");
  el?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
});
await sleep(700);
check("flink ../client.ts → client.ts (résolution baseDir)", (await p.evaluate(() => document.querySelector("#file-path").textContent)) === "client.ts",
  await p.evaluate(() => document.querySelector("#file-path").textContent));
check("fv-md masqué sur .ts", !(await vis("#fv-md")));
check("file-view source affiché sur .ts", await vis("#file-view"));

// Retour sur deep.md → édition : split source/aperçu live (desktop)
await p.evaluate(() => [...document.querySelectorAll(".fnode.is-dir")].find((n) => n.textContent.includes("docs"))?.click());
await sleep(500);
await p.evaluate(() => [...document.querySelectorAll(".fnode")].find((n) => n.textContent.includes("deep.md"))?.click());
await sleep(700);
check("deep.md ouvert en mode rendu", await vis("#file-md"));
check("fv-edit visible avec label", await vis("#fv-edit") && (await p.evaluate(() => document.querySelector("#fv-edit .fv-lbl")?.textContent.length ?? 0)) > 0);
await click("#fv-edit"); await sleep(300);
// Panneau trop étroit avec l'arbre → masquer la colonne pour atteindre
// la largeur du split côte-à-côte (fv-row).
await click("#btn-tree"); await sleep(200);
check("édition : edwrap visible", await vis("#file-edwrap"));
check("édition : fv-md visible (aperçu togglable)", await vis("#fv-md"));
check("édition : aperçu live visible", await vis("#file-md"));
check("édition : file-view source masqué", !(await vis("#file-view")));
check("split : classes fv-split+fv-row sur #file-main", await p.evaluate(() => {
  const m = document.querySelector("#file-main");
  return m.classList.contains("fv-split") && m.classList.contains("fv-row");
}));

// Frappe → l'aperçu live se met à jour (debounce 150 ms)
await p.click("#file-edit");
await p.keyboard.down("Control"); await p.keyboard.press("End"); await p.keyboard.up("Control");
await p.keyboard.type(" HELLO-SPLIT");
await sleep(500);
const editVal = await p.evaluate(() => document.querySelector("#file-edit").value);
check("frappe : buffer contient HELLO-SPLIT", editVal.includes("HELLO-SPLIT"));
check("aperçu live : rendu contient HELLO-SPLIT", await p.evaluate(() => document.querySelector("#file-md").textContent.includes("HELLO-SPLIT")));
check("dirty : statut « modified »", await p.evaluate(() => document.querySelector("#fv-status").textContent.includes("modified")));

// Échap avec buffer sale → garde dirty (dialog), « rester » = continue
await p.keyboard.press("Escape"); await sleep(300);
check("dirty : dialog de garde affiché", await p.evaluate(() => document.querySelector("#dlg-filedirty").open));
await click("#fd-stay"); await sleep(200);
check("dirty : toujours en édition après « rester »", await vis("#file-edwrap"));
// Le dialogue a volé le focus — le refocus avant la frappe suivante.
await p.click("#file-edit"); await sleep(100);

// Ctrl+Z → undo custom (rafale = un pas)
await p.keyboard.down("Control"); await p.keyboard.press("z"); await p.keyboard.up("Control"); await sleep(200);
check("Ctrl+Z : HELLO-SPLIT annulé", !(await p.evaluate(() => document.querySelector("#file-edit").value.includes("HELLO-SPLIT"))));
await sleep(400);
check("Ctrl+Z : aperçu revenu à l'état initial", !(await p.evaluate(() => document.querySelector("#file-md").textContent.includes("HELLO-SPLIT"))));
// Ctrl+Shift+Z → redo
await p.keyboard.down("Control"); await p.keyboard.down("Shift"); await p.keyboard.press("z"); await p.keyboard.up("Shift"); await p.keyboard.up("Control"); await sleep(200);
check("Ctrl+Shift+Z : HELLO-SPLIT restauré", await p.evaluate(() => document.querySelector("#file-edit").value.includes("HELLO-SPLIT")));
// Ctrl+Z à nouveau pour repartir propre
await p.keyboard.down("Control"); await p.keyboard.press("z"); await p.keyboard.up("Control"); await sleep(200);
check("undo retour état propre → dirty levé", await p.evaluate(() => !document.querySelector("#fv-status").textContent.includes("modified")),
  await p.evaluate(() => JSON.stringify({ st: document.querySelector("#fv-status").textContent, v: document.querySelector("#file-edit").value.slice(0, 60) })));

// Ctrl+S → save_edit (mock write_file)
await p.keyboard.type(" SAVE-ME"); await sleep(100);
await p.keyboard.down("Control"); await p.keyboard.press("s"); await p.keyboard.up("Control"); await sleep(400);
check("Ctrl+S : statut « saved »", await p.evaluate(() => document.querySelector("#fv-status").textContent.includes("saved")), await p.evaluate(() => document.querySelector("#fv-status").textContent));
check("Ctrl+S : dirty levé", !(await p.evaluate(() => document.querySelector("#fv-status").textContent.includes("modified"))));

// Sortie d'édition (buffer sauvegardé → pas de dialog) → rendu restauré
await click("#fv-edit"); await sleep(400);
check("sortie édition : edwrap masqué", !(await vis("#file-edwrap")));
check("sortie édition : split retiré", await p.evaluate(() => !document.querySelector("#file-main").classList.contains("fv-split")));
check("sortie édition : aperçu contient le texte sauvé", await p.evaluate(() => document.querySelector("#file-md").textContent.includes("SAVE-ME")));
check("sortie édition : rendu restauré", await vis("#file-md"));

// Fichier non-md (.ts) : édition sans split ni aperçu
await p.evaluate(() => [...document.querySelectorAll(".fnode")].find((n) => n.textContent.includes("client.ts"))?.click());
await sleep(700);
check("client.ts : fv-edit visible", await vis("#fv-edit"));
await click("#fv-edit"); await sleep(300);
check(".ts édition : edwrap visible", await vis("#file-edwrap"));
check(".ts édition : pas de split", await p.evaluate(() => !document.querySelector("#file-main").classList.contains("fv-split")));
check(".ts édition : fv-md masqué", !(await vis("#fv-md")));
await click("#fv-edit"); await sleep(300);

// read_file en échec sur un .md : l'erreur doit être visible en vue
// source (pas de « … » éternel ni de rendu partiel silencieux).
await p.evaluate(() => [...document.querySelectorAll(".fnode")].find((n) => n.textContent.includes("bad.md"))?.click());
await sleep(700);
check("erreur : vue source affichée", await vis("#file-view"));
check("erreur : file-md masqué", !(await vis("#file-md")));
check("erreur : fv-md masqué", !(await vis("#fv-md")));
const errTxt = await p.evaluate(() => document.querySelector("#file-view").textContent);
check("erreur : message visible", errTxt.includes("permission denied"), errTxt.slice(0, 80));

console.log(`\n${pass} PASS, ${fail} FAIL`);
await b.close(); server.kill();
process.exit(fail ? 1 : 0);
