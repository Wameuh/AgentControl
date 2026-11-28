// E2E « arborescence détachable » (#389) — pop-out du panneau fichiers
// en fenêtre flottante : drag, persistance géométrie/mode, dock retour,
// restauration au reload. Mock Tauri (tauri-mock.js) + `#desktop`.
// Usage : npm run build && node tests/tree-float-e2e.mjs
//   (chromium via $CHROME_BIN ou chrome-headless-shell puppeteer)
import puppeteer from "puppeteer-core";
import { readFileSync, mkdirSync } from "node:fs";
import { startVitePreview } from "./lib/vite-preview.mjs";

const EXE = process.env.CHROME_BIN
  ?? `${process.env.HOME}/.cache/puppeteer/chrome-headless-shell/linux-156.0.8078.47/chrome-headless-shell-linux64/chrome-headless-shell`;
const { server, base: BASE } = await startVitePreview();
const b = await puppeteer.launch({ executablePath: EXE, headless: "new", args: ["--no-sandbox"] });
const p = await b.newPage();
await p.setViewport({ width: 1280, height: 800, deviceScaleFactor: 2 });
p.on("console", (m) => { if (m.type() === "error") console.log("[page]", m.text()); });
await p.evaluateOnNewDocument(readFileSync("tests/tauri-mock.js", "utf8"));

let pass = 0, fail = 0;
const check = (name, ok, extra = "") => {
  if (ok) { pass++; console.log("PASS", name); }
  else { fail++; console.log("FAIL", name, extra); }
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const vis = (sel) => p.evaluate((s) => {
  const el = document.querySelector(s);
  return el && !el.hidden && el.getClientRects().length > 0;
}, sel);
const state = () => p.evaluate(() => ({
  floatHidden: document.getElementById("ft-float").hidden,
  sideParent: document.getElementById("ft-side").parentElement.id,
  nodes: document.querySelectorAll("#ft-float .fnode").length,
  popOn: document.getElementById("btn-tree-pop").classList.contains("on"),
  geo: JSON.parse(localStorage.getItem("ac-ft-float") ?? "null"),
}));

mkdirSync("shots", { recursive: true });
const shot = (n) => p.screenshot({ path: `shots/389-${n}.png` }).then(() => console.log("->", n));

await p.goto(`${BASE}#desktop`, { waitUntil: "networkidle0" });
await sleep(600);
// Session mockée → panneau fichiers → arbre docké.
await p.evaluate(() => document.querySelector("#session-list .sess")?.click());
await sleep(900);
await p.evaluate(() => document.getElementById("btn-files").click());
await sleep(900);
check("arbre docké : #ft-side dans #file-body", (await state()).sideParent === "file-body");
check("arbre docké : nœuds présents", (await p.evaluate(() =>
  document.querySelectorAll("#file-tree .fnode").length)) > 3);

// Pop-out → la colonne vit dans #ft-float.
await p.evaluate(() => document.getElementById("btn-tree-pop").click());
await sleep(400);
let s = await state();
check("pop-out : #ft-side déplacé dans #ft-float", s.sideParent === "ft-float");
check("pop-out : flottant visible", !s.floatHidden);
check("pop-out : arbre complet dans le flottant", s.nodes > 3, `n=${s.nodes}`);
check("pop-out : bouton actif (.on)", s.popOn);
check("pop-out : géométrie persistée", !!(s.geo?.d && s.geo.w > 0), JSON.stringify(s.geo));
await shot("float-open");

// Drag de la barre → nouvelle position persistée.
const before = s.geo;
await p.evaluate(() => {
  const bar = document.getElementById("ft-float-bar");
  const r = bar.getBoundingClientRect();
  const o = { bubbles: true, button: 0, pointerId: 1, buttons: 1,
              clientX: r.x + r.width / 2, clientY: r.y + 6 };
  bar.dispatchEvent(new PointerEvent("pointerdown", o));
  window.dispatchEvent(new PointerEvent("pointermove",
    { ...o, clientX: o.clientX - 120, clientY: o.clientY + 90 }));
  window.dispatchEvent(new PointerEvent("pointerup", { ...o, buttons: 0 }));
});
await sleep(200);
s = await state();
check("drag : position persistée", s.geo.x === before.x - 120 && s.geo.y === before.y + 90,
  `${before.x},${before.y} → ${s.geo.x},${s.geo.y}`);
await shot("float-dragged");

// Recherche dans le flottant (même DOM #ft-q / #file-hits) : le mock
// ne connaît pas find_files → on vérifie le basculement UI recherche
// (arbre masqué, hits affichés), pas les résultats.
await p.evaluate(() => {
  const q = document.getElementById("ft-q");
  q.value = "client";
  q.dispatchEvent(new Event("input", { bubbles: true }));
});
await sleep(700);
check("recherche flottante : hits affichés, arbre masqué", await p.evaluate(() =>
  !document.getElementById("file-hits").hidden &&
  document.getElementById("file-tree").hidden));
await shot("float-search");

// Régression review : ouvrir un fichier depuis le flottant panneau
// fermé ne doit PAS recharger l'arbre (dossiers dépliés conservés).
await p.evaluate(() => {
  const q = document.getElementById("ft-q");
  q.value = "";
  q.dispatchEvent(new Event("input", { bubbles: true }));
});
await sleep(500);
// Déplie « src » dans le flottant.
await p.evaluate(() => {
  const n = [...document.querySelectorAll("#ft-float .fnode.is-dir")]
    .find((x) => x.dataset.rel === "src");
  n?.click();
});
await sleep(500);
check("src déplié dans le flottant", await p.evaluate(() => {
  const n = [...document.querySelectorAll("#ft-float .fnode.is-dir")]
    .find((x) => x.dataset.rel === "src");
  return n?.nextElementSibling?.classList.contains("fchildren") ?? false;
}));
// Ferme le panneau fichiers puis clique un fichier du flottant.
await p.evaluate(() => document.getElementById("file-close").click());
await sleep(500);
check("panneau fermé, flottant toujours visible", await p.evaluate(() =>
  document.getElementById("file-panel").hidden &&
  !document.getElementById("ft-float").hidden));
await p.evaluate(() => {
  [...document.querySelectorAll("#ft-float .fnode")]
    .find((n) => n.dataset.rel === "src/client.ts")?.click();
});
await sleep(800);
check("fichier ouvert : panneau rouvert", await vis("#file-panel"));
check("fichier ouvert : arbre toujours dans le flottant",
  (await state()).sideParent === "ft-float");
check("fichier ouvert : src toujours déplié", await p.evaluate(() => {
  const n = [...document.querySelectorAll("#ft-float .fnode.is-dir")]
    .find((x) => x.dataset.rel === "src");
  return n?.nextElementSibling?.classList.contains("fchildren") ?? false;
}));
await shot("float-file-open");

// Dock retour → colonne dans #file-body, géométrie conservée pour plus tard.
await p.evaluate(() => document.getElementById("ft-float-dock").click());
await sleep(300);
s = await state();
check("dock : #ft-side de retour dans #file-body", s.sideParent === "file-body");
check("dock : flottant masqué", s.floatHidden);
check("dock : mode détaché persisté à false", s.geo?.d === false);
await shot("docked");

// Re-pop puis reload : le mode détaché + la géométrie survivent.
await p.evaluate(() => document.getElementById("btn-tree-pop").click());
await sleep(300);
await p.reload({ waitUntil: "networkidle0" });
await sleep(700);
await p.evaluate(() => document.querySelector("#session-list .sess")?.click());
await sleep(900);
await p.evaluate(() => document.getElementById("btn-files").click());
await sleep(900);
s = await state();
check("reload : mode détaché restauré", s.sideParent === "ft-float" && !s.floatHidden);
check("reload : arbre repeuplé dans le flottant", s.nodes > 3, `n=${s.nodes}`);
await shot("float-restored");

await b.close();
server.kill();
console.log(`\n${pass} PASS, ${fail} FAIL`);
process.exit(fail ? 1 : 0);
