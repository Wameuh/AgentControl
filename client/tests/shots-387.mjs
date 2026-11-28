// Shots avant/après #387 : header chat (btn-caps-refresh vs btn-refresh)
// et panneau terminal (btn-term-restart). `#desktop` impose la plateforme
// linux au mock (tauri-mock.js) — sinon le layout mobile masque #chat.
// Usage : npm run build && node tests/shots-387.mjs <tag>
import puppeteer from "puppeteer-core";
import { readFileSync, mkdirSync } from "node:fs";
import { startVitePreview } from "./lib/vite-preview.mjs";

const TAG = process.argv[2] || "shot";
const EXE = process.env.CHROME_BIN
  ?? `${process.env.HOME}/.cache/puppeteer/chrome-headless-shell/linux-156.0.8078.47/chrome-headless-shell-linux64/chrome-headless-shell`;
const { server, base: BASE } = await startVitePreview();
const b = await puppeteer.launch({ executablePath: EXE, headless: "new", args: ["--no-sandbox"] });
const p = await b.newPage();
await p.setViewport({ width: 1280, height: 800, deviceScaleFactor: 2 });
await p.evaluateOnNewDocument(readFileSync("tests/tauri-mock.js", "utf8"));
await p.goto(`${BASE}#desktop`, { waitUntil: "networkidle0" });
await new Promise((r) => setTimeout(r, 900));

// Ouvrir une session mockée : #chat-opts (et btn-caps-refresh) restent
// masqués tant que app.current est nul.
const sess = await p.$("#session-list .sess");
if (sess) {
  await sess.click();
  await new Promise((r) => setTimeout(r, 1200));
}

mkdirSync("shots", { recursive: true });

const clipShot = async (sel, name) => {
  const box = await p.evaluate((s) => {
    const el = document.querySelector(s);
    if (!el) return null;
    el.scrollIntoView({ block: "nearest" });
    const r = el.getBoundingClientRect();
    return { x: r.x, y: r.y, width: r.width, height: r.height };
  }, sel);
  if (!box) throw new Error(`missing ${sel}`);
  await p.screenshot({ path: `shots/387-${TAG}-${name}.png`, clip: box });
  console.log("->", name, JSON.stringify(box));
};

// Header chat : force l'état « caps chargées » (deux refresh côte à côte)
await p.evaluate(() => {
  for (const id of ["btn-caps-refresh", "btn-search", "btn-first", "btn-pins", "btn-steps", "btn-term", "btn-files"]) {
    document.getElementById(id).hidden = false;
  }
});
await new Promise((r) => setTimeout(r, 150));
await clipShot("#chat-header", "header");

// Panneau terminal : btn-term-restart
await p.evaluate(() => {
  document.getElementById("term-panel").hidden = false;
});
await new Promise((r) => setTimeout(r, 400));
await clipShot("#term-head", "term");

await b.close();
server.kill();
