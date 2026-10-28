// Smoke DOM — arborescence de groupes (chemins "a/b") : rendu imbriqué,
// collapse par chemin, filtre, menu ctx (sous-groupe), drag&drop session
// et groupe (valide + refusé), rename/dissolve en sous-arbre.
// Usage : node tests/grptree-smoke.mjs   (après npm run build)
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";

const EXE = process.env.CHROME_BIN ?? "/usr/bin/chromium";
const server = spawn("npx", ["vite", "preview", "--port", "4198", "--strictPort"], { stdio: "ignore" });
await new Promise((r) => setTimeout(r, 1800));
const b = await puppeteer.launch({ executablePath: EXE, headless: true, args: ["--no-sandbox"] });
const mock = readFileSync("tests/tauri-mock.js", "utf8");
// Patch du mock : pré-seede des chemins hiérarchiques dans list_sessions
// (le mock répond group:"Serveur pi" en dur) et laisse passer les
// mutations set_session_group (applyAssign du vrai code).
const patch = `(() => {
  const inv = window.__TAURI_INTERNALS__.invoke;
  const MAP = { s1: "Prod/Web", s4: "Prod", s3: "Serveur pi/Sub" };
  window.__TAURI_INTERNALS__.invoke = (cmd, args) => {
    if (cmd === "call" && args?.method === "set_session_group") {
      MAP[args.params?.session ?? args.session] = (args.params ?? args).group;
    }
    return inv(cmd, args).then((r) =>
      cmd === "call" && args?.method === "list_sessions" && Array.isArray(r)
        ? r.map((s) => (s.id in MAP ? { ...s, group: MAP[s.id] || undefined } : s))
        : r);
  };
})();`;

let fails = 0;
const ok = (c, n) => { console.log((c ? "  PASS " : "  FAIL ") + n); if (!c) fails++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const p = await b.newPage();
await p.setViewport({ width: 1280, height: 860 });
await p.evaluateOnNewDocument(mock);
await p.evaluateOnNewDocument(patch);
await p.goto("http://localhost:4198/#desktop", { waitUntil: "domcontentloaded" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
// Groupe vide imbriqué stocké : "Vide/Nested" → "Vide" rendu implicitement.
await p.evaluate(() => localStorage.setItem("ac-groups:local", JSON.stringify(["Vide/Nested"])));
await p.evaluate(() => document.querySelector("#btn-connect")?.click());
await sleep(300);
await p.type("#c-host", "me@pi5.local");
await p.evaluate(() => document.querySelector("#c-go")?.click());
await sleep(900);

// helpers page
const hdr = (path) => p.evaluate((x) => {
  const n = document.querySelector(`.grp-name[title="${CSS.escape(x)}"]`);
  return n ? n.closest(".grp-sub") !== null : false;
}, path);
const hdrHidden = (path) => p.evaluate((x) => {
  const n = document.querySelector(`.grp-name[title="${CSS.escape(x)}"]`);
  const el = n?.closest(".grp-sub");
  return el ? getComputedStyle(el).display === "none" : null;
}, path);
const rowVisible = (txt) => p.evaluate((t) => {
  const el = [...document.querySelectorAll(".sess")].find((s) => s.textContent.includes(t));
  return el ? getComputedStyle(el).display !== "none" : null;
}, txt);
const rowIndent = (txt) => p.evaluate((t) => {
  const el = [...document.querySelectorAll(".sess")].find((s) => s.textContent.includes(t));
  return el ? parseInt(el.style.marginLeft || "0", 10) : null;
}, txt);
const padLeft = (path) => p.evaluate((x) => {
  const n = document.querySelector(`.grp-name[title="${CSS.escape(x)}"]`);
  const el = n?.closest(".grp-sub");
  return el ? parseInt(getComputedStyle(el).paddingLeft, 10) : null;
}, path);
const clickHdr = (path) => p.evaluate((x) => {
  document.querySelector(`.grp-name[title="${CSS.escape(x)}"]`)?.closest(".grp-sub")?.click();
}, path);
const rclkHdr = (path) => p.evaluate((x) => {
  document.querySelector(`.grp-name[title="${CSS.escape(x)}"]`)?.closest(".grp-sub")
    ?.dispatchEvent(new MouseEvent("contextmenu", { clientX: 200, clientY: 300, bubbles: true, cancelable: true }));
}, path);
const drop = (payload, targetPath) => p.evaluate(({ d, tp }) => {
  const dt = new DataTransfer();
  dt.setData("application/x-ac-session", JSON.stringify(d));
  const sel = tp === null ? ".grp-head"
    : `.grp-name[title="${CSS.escape(tp)}"]`;
  const t = tp === null
    ? [...document.querySelectorAll(".grp-head")].find((h) => h.textContent.includes("local"))
    : document.querySelector(sel)?.closest(".grp-sub");
  t?.dispatchEvent(new DragEvent("drop", { dataTransfer: dt, bubbles: true, cancelable: true }));
}, { d: payload, tp: targetPath });
const stored = () => p.evaluate(() => JSON.parse(localStorage.getItem("ac-groups:local") ?? "[]"));
const lsGet = (k) => p.evaluate((x) => localStorage.getItem(x), k);

// ---------- A. Rendu initial ----------
ok(await hdr("Prod"), "A1 header Prod (racine)");
ok(await hdr("Prod/Web"), "A2 header Prod/Web (imbriqué)");
ok(await hdr("Vide") && await hdr("Vide/Nested"), "A3 ancêtre implicite Vide + Vide/Nested (stocké)");
ok(await hdr("Serveur pi") && await hdr("Serveur pi/Sub"), "A4 legacy plat + sous-groupe mélangés");
ok((await padLeft("Prod")) === 18, "A5 indent Prod = 18px");
ok((await padLeft("Prod/Web")) === 32, "A6 indent Prod/Web = 32px");
ok((await rowIndent("Refactor")) === 28, "A7 session gDepth2 → 28px");
ok((await rowIndent("Reduce context")) === 14, "A8 session gDepth1 → 14px");

// ---------- B. Collapse ----------
await clickHdr("Prod"); await sleep(400);
ok((await lsGet("ac-grp-local::Prod")) === "1", "B1 clé collapse par chemin complet");
ok(await hdrHidden("Prod/Web") === true, "B2 header enfant masqué sous parent plié");
ok(await rowVisible("Refactor") === false && await rowVisible("Reduce context") === false, "B3 sessions du sous-arbre masquées");
ok(await hdrHidden("Serveur pi") === false, "B4 arbre voisin intact");
await clickHdr("Prod"); await sleep(400);
ok(await hdrHidden("Prod/Web") === false, "B5 repli/dépli restaure");

// ---------- C. Filtre ----------
await p.type("#sess-filter-input", "web"); await sleep(300);
ok(await hdr("Prod") && await hdr("Prod/Web") && !(await hdr("Infra")), "C1 filtre 'web' → chaîne ancêtres");
ok((await p.evaluate(() => document.querySelectorAll(".sess:not([style*='none'])").length)) === 1, "C2 seule la session matchée visible");
await p.evaluate(() => { const i = document.querySelector("#sess-filter-input"); i.value = ""; i.dispatchEvent(new Event("input")); });
await sleep(300);

// ---------- D. Menu ctx : Nouveau sous-groupe ----------
await rclkHdr("Prod"); await sleep(300);
const btns = await p.evaluate(() => [...document.querySelectorAll("#sctx button")].map((b) => b.textContent));
ok(btns.some((x) => /sub-group/i.test(x)), "D1 entrée « New sub-group » présente");
ok(btns.length === 4, "D2 menu = 4 entrées (sess, sub-group, rename, dissolve)");
await p.evaluate(() => [...document.querySelectorAll("#sctx button")].find((b) => /sub-group/i.test(b.textContent))?.click());
await sleep(400);
ok((await p.evaluate(() => document.querySelector("#g-name")?.value)) === "Prod/", "D3 dlg prérempli « Prod/ »");
await p.type("#g-name", "API");
await p.evaluate(() => document.querySelector("#g-ok")?.click());
await sleep(500);
ok(await hdr("Prod/API"), "D4 sous-groupe Prod/API créé");
ok((await stored()).includes("Prod/API") && (await stored()).includes("Prod"), "D5 chemins stockés (ancêtre présent)");

// ---------- E. Drag groupe → autre groupe ----------
await drop({ kind: "group", path: "Prod/Web", conn: "local" }, "Vide");
await sleep(700);
ok(await hdr("Vide/Web") && !(await hdr("Prod/Web")), "E1 Prod/Web déplacé sous Vide");
ok(await rowVisible("Refactor"), "E2 session du sous-arbre suit le déplacement");
ok((await stored()).includes("Vide/Web") && !(await stored()).includes("Prod/Web"), "E3 chemins stockés remappés");

// ---------- F. Drag groupe → refusé dans son sous-arbre ----------
await drop({ kind: "group", path: "Prod", conn: "local" }, "Prod/API");
await sleep(500);
ok(await hdr("Prod") && await hdr("Prod/API"), "F1 drop dans son propre sous-arbre refusé");
ok(!(await stored()).includes("Prod/API/Prod"), "F2 aucun chemin absurde créé");

// ---------- G. Drag session → sous-groupe + dégroupage ----------
await drop({ kind: "session", id: "s2", conn: "local" }, "Vide/Web");
await sleep(700);
ok((await rowIndent("Fix login")) === 28, "G1 session déposée dans sous-groupe (28px)");
await drop({ kind: "session", id: "s2", conn: "local" }, null);
await sleep(700);
ok((await rowIndent("Fix login")) === 0, "G2 session déposée à la racine (dégroupée)");

// ---------- H. Rename propage le préfixe ----------
await rclkHdr("Prod"); await sleep(300);
await p.evaluate(() => [...document.querySelectorAll("#sctx button")].find((b) => /rename/i.test(b.textContent))?.click());
await sleep(400);
ok((await p.evaluate(() => document.querySelector("#g-name")?.value)) === "Prod", "H1 rename prérempli du chemin complet");
await p.evaluate(() => { const i = document.querySelector("#g-name"); i.value = "Root/Prod"; });
await p.evaluate(() => document.querySelector("#g-ok")?.click());
await sleep(700);
ok(await hdr("Root") && await hdr("Root/Prod") && await hdr("Root/Prod/API"), "H2 sous-arbre renommé avec descendants");
ok((await stored()).includes("Root/Prod/API"), "H3 chemins stockés descendants migrés");
ok(await rowVisible("Reduce context"), "H4 session du nœud suit");

// ---------- I. Dissolve en sous-arbre ----------
await rclkHdr("Serveur pi"); await sleep(300);
await p.evaluate(() => [...document.querySelectorAll("#sctx button")].find((b) => /dissolve/i.test(b.textContent))?.click());
await sleep(200);
const armed = await p.evaluate(() => [...document.querySelectorAll("#sctx button")].find((b) => /ungroup|dégroup/i.test(b.textContent))?.textContent);
ok(/1 session/.test(armed ?? ""), `I1 armement compte sous-arbre (${armed})`);
await p.evaluate(() => [...document.querySelectorAll("#sctx button")].find((b) => /ungroup|dégroup/i.test(b.textContent))?.click());
await sleep(700);
ok(!(await hdr("Serveur pi")) && !(await hdr("Serveur pi/Sub")), "I2 sous-arbre dissous (headers partis)");
ok(await rowVisible("Write unit tests"), "I3 session dégroupée reste dans la liste");
ok(!(await stored()).some((g) => g.startsWith("Serveur pi")), "I4 chemins stockés purgés");

console.log(`\n${fails === 0 ? "SMOKE OK" : "SMOKE FAIL"} — ${fails} échec(s)`);
await b.close(); server.kill();
process.exit(fails ? 1 : 0);
