// E2E « viewer PDF » — pdf.js embarqué : rendu canvas des pages dans
// #file-view, scroll continu, zoom +/-, statut « N pages », fallback
// « ouvrir en externe » au-delà du cap. Mock Tauri avec
// read_file{binary:true} (format démon {size,data(base64)}).
// Usage : npm run build && node tests/pdf-view-e2e.mjs
//   (chromium via $CHROMIUM ou chrome-headless-shell du cache puppeteer)
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const CHROMIUM = process.env.CHROMIUM ?? (() => {
  const dir = join(homedir(), ".cache/puppeteer/chrome-headless-shell");
  if (existsSync(dir)) {
    const v = readdirSync(dir).sort().pop();
    const p = join(dir, v, "chrome-headless-shell-linux64/chrome-headless-shell");
    if (existsSync(p)) return p;
  }
  return "/usr/bin/chromium";
})();

// PDF minimal valide : 2 pages « Hello page 1 » / « Page deux » (Helvetica).
const PDF_B64 =
  "JVBERi0xLjQKMSAwIG9iago8PC9UeXBlL0NhdGFsb2cvUGFnZXMgMiAwIFI+PgplbmRvYmoKMiAwIG9iago8PC9UeXBlL1BhZ2VzL0tpZHNbMyAwIFIgNCAwIFJdL0NvdW50IDI+PgplbmRvYmoKMyAwIG9iago8PC9UeXBlL1BhZ2UvUGFyZW50IDIgMCBSL01lZGlhQm94WzAgMCA2MTIgNzkyXS9SZXNvdXJjZXM8PC9Gb250PDwvRjEgNSAwIFI+Pj4+L0NvbnRlbnRzIDYgMCBSPj4KZW5kb2JqCjQgMCBvYmoKPDwvVHlwZS9QYWdlL1BhcmVudCAyIDAgUi9NZWRpYUJveFswIDAgNjEyIDc5Ml0vUmVzb3VyY2VzPDwvRm9udDw8L0YxIDUgMCBSPj4+Pi9Db250ZW50cyA3IDAgUj4+CmVuZG9iago1IDAgb2JqCjw8L1R5cGUvRm9udC9TdWJ0eXBlL1R5cGUxL0Jhc2VGb250L0hlbHZldGljYT4+CmVuZG9iago2IDAgb2JqCjw8L0xlbmd0aCA0Mz4+CnN0cmVhbQpCVCAvRjEgMjQgVGYgNzIgNzIwIFRkIChIZWxsbyBwYWdlIDEpIFRqIEVUCmVuZHN0cmVhbQplbmRvYmoKNyAwIG9iago8PC9MZW5ndGggNDA+PgpzdHJlYW0KQlQgL0YxIDI0IFRmIDcyIDYyMCBUZCAoUGFnZSBkZXV4KSBUaiBFVAplbmRzdHJlYW0KZW5kb2JqCnhyZWYKMCA4CjAwMDAwMDAwMDAgNjU1MzUgZiAKMDAwMDAwMDAwOSAwMDAwMCBuIAowMDAwMDAwMDU0IDAwMDAwIG4gCjAwMDAwMDAxMTEgMDAwMDAgbiAKMDAwMDAwMDIyMyAwMDAwMCBuIAowMDAwMDAwMzM1IDAwMDAwIG4gCjAwMDAwMDAzOTggMDAwMDAgbiAKMDAwMDAwMDQ4OSAwMDAwMCBuIAp0cmFpbGVyCjw8L1NpemUgOC9Sb290IDEgMCBSPj4Kc3RhcnR4cmVmCjU3NwolJUVPRgo=";

const MOCK = `(function () {
  const SESSIONS = [
    { id: "s1", conn: "local", agent: "devin", title: "Refactor API",
      cwd: "/home/agent/api", created_at: "2026-10-20T10:00:00Z",
      available_modes: [], available_models: [], available_commands: [],
      config_options: [], state: "idle" },
  ];
  const PDF_B64 = "${PDF_B64}";
  const DIRS = {
    "": { dir: "/home/agent/api", entries: [
      { name: "doc.pdf", dir: false, size: 797 },
      { name: "big.pdf", dir: false, size: 20 * 1024 * 1024 },
      { name: "note.txt", dir: false, size: 42 },
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
        if (args.path === "doc.pdf" && args.binary) {
          return { path: "/home/agent/api/doc.pdf", size: 797,
                   truncated: false, data: PDF_B64 };
        }
        if (args.path === "big.pdf" && args.binary) {
          // CAP démon dépassé : pas de data, size + truncated.
          return { path: "/home/agent/api/big.pdf",
                   size: 20 * 1024 * 1024, truncated: true };
        }
        if (args.path === "note.txt") {
          const t = "juste une note\\n";
          return { path: "/home/agent/api/note.txt", offset: 0,
                   size: t.length, data: btoa(t), mtime_ms: 1 };
        }
        throw new Error("no such file " + args.path);
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

const server = spawn("npx", ["vite", "preview", "--port", "4187", "--strictPort"], { stdio: "ignore" });
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
const status = () => p.evaluate(() => document.querySelector("#fv-status").textContent);
const openFile = (frag) => p.evaluate((f) =>
  [...document.querySelectorAll(".fnode")].find((n) => n.textContent.includes(f))?.click(), frag);

await p.goto("http://localhost:4187/", { waitUntil: "networkidle0" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
await click("#btn-connect"); await sleep(300);
await p.type("#c-host", "me@pi5.local"); await click("#c-go"); await sleep(900);
await p.evaluate(() => [...document.querySelectorAll(".sess")].find((s) => s.textContent.includes("Refactor"))?.click());
await sleep(900);
await click("#btn-files"); await sleep(800);

// Ouvre doc.pdf — pdf.js charge (chunk lazy) puis rend les pages.
await openFile("doc.pdf");
await p.waitForSelector(".fv-pdfpage canvas", { timeout: 15000 }).catch(() => {});
await sleep(1200);

check("conteneur .fv-pdf présent", await vis(".fv-pdf"));
const pages = await p.evaluate(() => document.querySelectorAll(".fv-pdfpage").length);
check("2 pages dans le viewer", pages === 2, `pages=${pages}`);
const rendered = await p.evaluate(() =>
  [...document.querySelectorAll(".fv-pdfpage canvas")].filter((c) => c.width > 0).length);
check("≥1 canvas rendu", rendered >= 1, `rendered=${rendered}`);
check("statut « 2 pages »", (await status()).includes("2 pages"), await status());
check("boutons zoom visibles", (await vis("#fv-zin")) && (await vis("#fv-zout")));
check("select encodage masqué", !(await vis("#fv-enc")));

// Zoom avant : le % du statut augmente.
const z0 = parseInt((await status()).match(/(\d+)%/)?.[1] ?? "0", 10);
await click("#fv-zin"); await sleep(900);
const z1 = parseInt((await status()).match(/(\d+)%/)?.[1] ?? "0", 10);
check("zoom + augmente le %", z1 > z0, `${z0}% → ${z1}%`);
await sleep(600);
check("canvas re-rendu après zoom", await p.evaluate(() =>
  [...document.querySelectorAll(".fv-pdfpage canvas")].some((c) => c.width > 0)));

// big.pdf → pas d'aperçu, bouton « ouvrir externe » proposé.
await openFile("big.pdf"); await sleep(800);
check("pas de .fv-pdf pour big.pdf", !(await vis(".fv-pdf")));
check("bouton ouverture externe proposé", await vis(".fv-pdfext"));
check("message « trop volumineux »", await p.evaluate(() =>
  document.querySelector("#file-view").textContent.includes("too large") ||
  document.querySelector("#file-view").textContent.includes("volumineux")));

// Retour à un fichier texte : les contrôles PDF disparaissent.
await openFile("note.txt"); await sleep(800);
check("zoom masqué hors PDF", !(await vis("#fv-zin")));
check("conteneur PDF retiré", !(await p.evaluate(() => document.querySelector(".fv-pdf"))));

server.kill();
await b.close();
console.log(`\n${pass} pass, ${fail} fail`);
process.exit(fail ? 1 : 0);
