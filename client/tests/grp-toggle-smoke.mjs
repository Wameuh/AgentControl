// Smoke DOM — toggle de groupe ciblé (#369) : clic sur un header de
// groupe/connexion = bascule locale pure (display + chevron), SANS RPC
// ni rebuild DOM. Perf : ~140 sessions, le toggle doit être quasi
// instantané (<1 frame) et synchrone. Régression : pli persistant au
// reload, chaîne de plis imbriqués, filtre force déplié, ctx menu et
// drag & drop intacts après un toggle ciblé.
// Usage : node tests/grp-toggle-smoke.mjs   (après npm run build)
import puppeteer from "puppeteer-core";
import { readFileSync } from "node:fs";
import { startVitePreview } from "./lib/vite-preview.mjs";

const EXE = process.env.CHROME_BIN ?? "/usr/bin/chromium";
const CK = "ssh:pi5.local:22";
const { base } = await startVitePreview();
const b = await puppeteer.launch({ executablePath: EXE, headless: true, args: ["--no-sandbox"] });

// 140 sessions : 100 dans « Gros », 30 dans « Gros/Sous », 10 non
// groupées — de quoi mesurer un rebuild complet s'il avait lieu.
const mk = (id, title) => ({
  id, conn: CK, agent: "mock", title, state: "idle",
  cwd: "/tmp", created_at: "2026-10-20T10:00:00Z",
});
const SESS = [];
const MAP = {};
for (let i = 0; i < 100; i++) { SESS.push(mk(`g${i}`, `Gros session ${String(i).padStart(3, "0")}`)); MAP[`g${i}`] = "Gros"; }
for (let i = 0; i < 30; i++) { SESS.push(mk(`s${i}`, `Sous session ${String(i).padStart(2, "0")}`)); MAP[`s${i}`] = "Gros/Sous"; }
for (let i = 0; i < 10; i++) SESS.push(mk(`p${i}`, `Plain session ${i}`));

// Patch du mock : list_sessions répond le jeu généré (group via MAP,
// mutable par set_session_group comme grptree-smoke) et compte les
// RPC pour prouver qu'un toggle n'en déclenche aucun.
const patch = `(() => {
  const inv = window.__TAURI_INTERNALS__.invoke;
  const SESS = ${JSON.stringify(SESS)};
  const MAP = ${JSON.stringify(MAP)};
  window.__rpc = { list_sessions: 0, pr_list: 0 };
  window.__TAURI_INTERNALS__.invoke = (cmd, args) => {
    if (cmd === "call") {
      if (args?.method === "list_sessions") window.__rpc.list_sessions++;
      if (args?.method === "pr_list") window.__rpc.pr_list++;
      if (args?.method === "set_session_group") {
        MAP[args.params?.session ?? args.session] = (args.params ?? args).group;
      }
    }
    return inv(cmd, args).then((r) =>
      cmd === "call" && args?.method === "list_sessions" && Array.isArray(r)
        ? SESS.map((s) => (s.id in MAP ? { ...s, group: MAP[s.id] || undefined } : s))
        : r);
  };
})();`;

let fails = 0;
const ok = (c, n) => { console.log((c ? "  PASS " : "  FAIL ") + n); if (!c) fails++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const p = await b.newPage();
await p.setViewport({ width: 1280, height: 860 });
await p.evaluateOnNewDocument(readFileSync("tests/tauri-mock.js", "utf8"));
await p.evaluateOnNewDocument(patch);
await p.goto(`${base}/#desktop`, { waitUntil: "domcontentloaded" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
await p.evaluate(() => document.querySelector("#btn-connect")?.click());
await sleep(300);
await p.type("#c-host", "pi5.local");
await p.evaluate(() => document.querySelector("#c-go")?.click());
await p.waitForSelector('.grp-name[title="Gros"]', { timeout: 15000 });
await sleep(300);

const hdrSel = (path) => `.grp-name[title="${path}"]`; // chemins de test sans caractères spéciaux
const clickHdr = (path) => p.evaluate((x) => {
  document.querySelector(`.grp-name[title="${CSS.escape(x)}"]`)?.closest(".grp-sub")?.click();
}, path);
const rowState = (txt) => p.evaluate((t) => {
  const el = [...document.querySelectorAll(".sess")].find((s) => s.textContent.includes(t));
  return el ? getComputedStyle(el).display !== "none" : null;
}, txt);
const hdrState = (path) => p.evaluate((x) => {
  const el = document.querySelector(`.grp-name[title="${CSS.escape(x)}"]`)?.closest(".grp-sub");
  if (!el) return null;
  return { visible: getComputedStyle(el).display !== "none", closed: el.classList.contains("closed") };
}, path);
const rpcCount = () => p.evaluate(() => ({ ...window.__rpc }));

// ---------- A. Rendu + toggle ciblé : synchrone, sans RPC, sans rebuild --
ok((await p.evaluate(() => document.querySelectorAll(".sess").length)) === 140, "A1 140 sessions rendues");

// Marque un nœud : un rebuild le détruirait (régression du « ciblé »).
await p.evaluate(() => {
  const el = [...document.querySelectorAll(".sess")].find((s) => s.textContent.includes("Gros session 000"));
  el.__mark = "alive";
  window.__marked = el;
});
await p.evaluate(() => { window.__rpc.list_sessions = 0; window.__rpc.pr_list = 0; });
const toggle = await p.evaluate(() => {
  const hdr = document.querySelector('.grp-name[title="Gros"]')?.closest(".grp-sub");
  const row = [...document.querySelectorAll(".sess")].find((s) => s.textContent.includes("Gros session 000"));
  const sub = document.querySelector('.grp-name[title="Gros/Sous"]')?.closest(".grp-sub");
  const plain = [...document.querySelectorAll(".sess")].find((s) => s.textContent.includes("Plain session 0"));
  const t0 = performance.now();
  hdr.click();
  const ms = performance.now() - t0;
  return {
    ms,
    rowHidden: getComputedStyle(row).display === "none",
    subHidden: getComputedStyle(sub).display === "none",
    plainVisible: getComputedStyle(plain).display !== "none",
    markedAlive: window.__marked.isConnected && window.__marked.__mark === "alive",
  };
});
const rpcAfter = await rpcCount();
ok(toggle.rowHidden && toggle.subHidden, "A2 pli : sessions + sous-groupe masqués (synchrone)");
ok(toggle.plainVisible, "A3 sessions hors groupe intactes");
ok(toggle.markedAlive, "A4 aucune reconstruction DOM (même nœud)");
ok(rpcAfter.list_sessions === 0 && rpcAfter.pr_list === 0, "A5 aucun RPC (list_sessions/pr_list) sur le toggle");
ok(toggle.ms < 50, `A6 toggle < 1 frame (~${toggle.ms.toFixed(1)} ms relevé)`);
const st1 = await hdrState("Gros");
ok(st1?.closed === true && st1.visible === true, "A7 header reste visible, marqué closed");

// ---------- B. Dépli + chaîne de plis imbriqués -------------------------
await clickHdr("Gros/Sous"); await sleep(50);
ok((await hdrState("Gros/Sous"))?.closed === true, "B1 sous-groupe plié");
ok(await rowState("Sous session 00") === false, "B2 sessions du sous-groupe masquées");
await clickHdr("Gros"); await sleep(50);   // déplie Gros, Sous reste plié
ok(await rowState("Gros session 000") === true, "B3 sessions de Gros réaffichées");
const subSt = await hdrState("Gros/Sous");
ok(subSt?.visible === true && subSt.closed === true, "B4 en-tête Sous visible mais toujours plié");
ok(await rowState("Sous session 00") === false, "B5 sessions Sous toujours masquées (chaîne)");
ok((await p.evaluate(() => localStorage.getItem(`ac-grp-ssh:pi5.local:22::Gros/Sous`))) === "1", "B6 clé enfant conservée");

// ---------- C. Header connexion ----------------------------------------
await p.evaluate(() => document.querySelector(".grp-head.grp-conn")?.click());
await sleep(50);
ok(await rowState("Plain session 0") === false && await rowState("Gros session 000") === false,
  "C1 pli connexion masque tout le contenu");
await p.evaluate(() => document.querySelector(".grp-head.grp-conn")?.click());
await sleep(50);
ok(await rowState("Plain session 0") === true, "C2 dépli connexion restaure");

// ---------- D. Filtre force déplié, toggle désactivé --------------------
await p.type("#sess-filter-input", "sous session 00"); await sleep(300);
ok(await rowState("Sous session 00") === true, "D1 filtre force le déplié");
const keyBefore = await p.evaluate(() => localStorage.getItem(`ac-grp-ssh:pi5.local:22::Gros/Sous`));
await clickHdr("Gros/Sous"); await sleep(50);
ok((await p.evaluate(() => localStorage.getItem(`ac-grp-ssh:pi5.local:22::Gros/Sous`))) === keyBefore,
  "D2 toggle inerte sous filtre (clé inchangée)");
await p.evaluate(() => { const i = document.querySelector("#sess-filter-input"); i.value = ""; i.dispatchEvent(new Event("input")); });
await sleep(300);
ok(await rowState("Sous session 00") === false, "D3 filtre vidé → pli restauré");

// ---------- E. Persistance au reload -------------------------------------
await p.evaluate(() => localStorage.setItem(`ac-grp-ssh:pi5.local:22::Gros`, "1"));
await p.reload({ waitUntil: "domcontentloaded" });
// evaluateOnNewDocument est rejoué à chaque document — le patch
// (sessions + compteur RPC) est actif sans nouvelle injection.
await p.waitForSelector(hdrSel("Gros"), { timeout: 20000 });
await sleep(600);
const reSt = await hdrState("Gros");
ok(reSt?.closed === true && reSt.visible === true, "E1 pli connexion-groupe persisté après reload");
ok(await rowState("Gros session 000") === false, "E2 sessions toujours masquées après reload");

// ---------- F. ctx menu + drag & drop après toggle ciblé ----------------
// Déplier d'abord : les cibles du drag doivent être visibles.
await clickHdr("Gros"); await sleep(50);
await clickHdr("Gros/Sous"); await sleep(50);
ok(await rowState("Sous session 00") === true, "F0 dépli complet avant drag");
await p.evaluate(() => {
  document.querySelector('.grp-name[title="Gros"]')?.closest(".grp-sub")
    ?.dispatchEvent(new MouseEvent("contextmenu", { clientX: 200, clientY: 300, bubbles: true, cancelable: true }));
});
await sleep(200);
const menuBtns = await p.evaluate(() => [...document.querySelectorAll("#sctx button")].length);
ok(menuBtns >= 5, `F1 menu ctx groupe intact (${menuBtns} entrées)`);
await p.keyboard.press("Escape");

// Drag d'une session hors groupe → « Gros/Sous » (handlers bindés au
// rendu, toggle ciblé ne les a pas touchés).
await p.evaluate(() => {
  const dt = new DataTransfer();
  dt.setData("application/x-ac-session", JSON.stringify({ kind: "session", id: "p0", conn: "ssh:pi5.local:22" }));
  const row = [...document.querySelectorAll(".sess")].find((s) => s.textContent.includes("Plain session 0"));
  row?.dispatchEvent(new DragEvent("dragstart", { dataTransfer: dt, bubbles: true, cancelable: true }));
  const tgt = document.querySelector('.grp-name[title="Gros/Sous"]')?.closest(".grp-sub");
  tgt?.dispatchEvent(new DragEvent("drop", { dataTransfer: dt, bubbles: true, cancelable: true }));
  document.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
});
await sleep(800);
ok((await rowState("Plain session 0")) === true, "F2 session déposée visible après drop");
ok((await p.evaluate(() => {
  const el = [...document.querySelectorAll(".sess")].find((s) => s.textContent.includes("Plain session 0"));
  return el ? el.style.marginLeft : null;
})) === "28px", "F3 session bien rangée sous Gros/Sous (indent 28px)");

console.log(`\n${fails === 0 ? "SMOKE OK" : "SMOKE FAIL"} — ${fails} échec(s)`);
await b.close();
process.exit(fails ? 1 : 0);
