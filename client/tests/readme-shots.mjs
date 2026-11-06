// Captures du README : Chromium headless, mock __TAURI_INTERNALS__
// (tests/tauri-mock.js — sessions/groupes/events/PRs/pins/backlog/PDF).
// Produit docs/screenshots/*.png : desktop-chat, desktop-files,
// desktop-pair, desktop-settings, desktop-backlog + android-* .
// Usage : npm run build && node tests/readme-shots.mjs [outdir]
//   CHROME=/path/chrome   — sinon chrome-headless-shell du cache
//   puppeteer, sinon /usr/bin/chromium.
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";
import { readFileSync, mkdirSync, existsSync, readdirSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const OUT = process.argv[2] || "../docs/screenshots";
mkdirSync(OUT, { recursive: true });

const CHROME = process.env.CHROME ?? (() => {
  const dir = join(homedir(), ".cache/puppeteer/chrome-headless-shell");
  if (existsSync(dir)) {
    const v = readdirSync(dir).sort().pop();
    const p = join(dir, v, "chrome-headless-shell-linux64/chrome-headless-shell");
    if (existsSync(p)) return p;
  }
  return "/usr/bin/chromium";
})();

const PORT = Number(process.env.SHOTS_PORT || 4183);
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], {
  cwd: new URL("..", import.meta.url).pathname, stdio: "ignore",
});
await new Promise((r) => setTimeout(r, 1800));

const browser = await puppeteer.launch({
  executablePath: CHROME,
  headless: "new",
  args: ["--no-sandbox", "--disable-dev-shm-usage", "--font-render-hinting=none",
         "--force-device-scale-factor=1"],
});
const mock = readFileSync(new URL("./tauri-mock.js", import.meta.url), "utf8");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const clickText = (p, sel, re) =>
  p.evaluate((s, r) => {
    const el = [...document.querySelectorAll(s)]
      .find((e) => new RegExp(r, "i").test(e.textContent));
    el?.click();
    return !!el;
  }, sel, re.source ?? re);

// Connexion auto au boot (saved_connections du mock) — on attend juste
// les sessions. Fallback : formulaire manuel si le dialogue reste.
async function waitSessions(p) {
  await p.waitForSelector(".sess", { visible: true, timeout: 15000 })
    .catch(async () => {
      await p.type("#c-host", "me@pi5.local");
      await p.click("#c-go");
      await p.waitForSelector(".sess", { visible: true, timeout: 15000 });
    });
  await sleep(600);
}

async function openSession(p, frag = "Refactor") {
  await clickText(p, ".sess", frag);
  await sleep(1000);
  await p.evaluate(() => {
    const e = document.querySelector("#events");
    if (e) e.scrollTop = 1e9;
  });
  await sleep(400);
}

// ---------- DESKTOP ----------
{
  const p = await browser.newPage();
  await p.setViewport({ width: 1568, height: 1054, deviceScaleFactor: 1 });
  await p.evaluateOnNewDocument(mock);
  p.on("pageerror", (e) => console.log("[pageerror]", e.message));
  const shot = async (n, ms = 400) => {
    await sleep(ms);
    await p.screenshot({ path: `${OUT}/${n}.png` });
    console.log("->", n);
  };

  await p.goto(`http://localhost:${PORT}/#desktop`, { waitUntil: "networkidle0" });
  await waitSessions(p);

  // Chat : markdown + tool calls + permission + steer + file d'attente.
  await openSession(p);
  await shot("desktop-chat");

  // Explorateur : arbre + preview PDF ouverte (viewer pdf.js embarqué).
  await p.click("#btn-files"); await sleep(900);
  await clickText(p, ".fnode", "src"); await sleep(500);
  await clickText(p, ".fnode", "rapport.pdf"); await sleep(300);
  await p.waitForSelector(".fv-pdfpage canvas", { timeout: 15000 }).catch(() => {});
  await shot("desktop-files", 1200);
  await p.evaluate(() => { document.getElementById("file-panel").hidden = true; });
  await sleep(300);

  // Onglet Backlog : file + graphique d'activité.
  await p.click("#sv-backlog"); await sleep(700);
  await shot("desktop-backlog");
  await p.click("#sv-agents"); await sleep(400);

  // Paramètres (thème/accents, sons, notifications, maj, appairage…).
  await p.click("#tb-settings");
  await p.waitForSelector("#settings-view:not([hidden])", { timeout: 8000 })
    .catch(() => {});
  await shot("desktop-settings", 700);
  await p.click("#settings-close");
  await p.waitForSelector("#settings-view[hidden]", { timeout: 5000 })
    .catch(() => {});
  await sleep(250);

  // Appairage : connexion enregistrée → bouton QR → dialogue + QR v2.
  await p.click("#sv-agents").catch(() => {});
  await p.click("#btn-connect"); await sleep(600);
  await p.waitForSelector(".cv-qr", { visible: true, timeout: 8000 });
  await p.click(".cv-qr");
  await p.waitForSelector("#dlg-qr[open]", { timeout: 8000 });
  await p.click("#qr-gen");
  await p.waitForSelector("#qr-canvas:not([hidden])", { timeout: 10000 })
    .catch(() => {});
  await shot("desktop-pair", 800);
  await p.close();
}

// ---------- ANDROID ----------
{
  const p = await browser.newPage();
  await p.setUserAgent(
    "Mozilla/5.0 (Linux; Android 15; SM-S931B Build/AP3A.240905.015) " +
    "AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36");
  await p.setViewport({ width: 412, height: 915, deviceScaleFactor: 2.625,
                      isMobile: true, hasTouch: true });
  await p.evaluateOnNewDocument(mock);
  p.on("pageerror", (e) => console.log("[pageerror]", e.message));
  const shot = async (n, ms = 400) => {
    await sleep(ms);
    await p.screenshot({ path: `${OUT}/${n}.png` });
    console.log("->", n);
  };
  const tap = async (sel, ms = 350) => {
    await p.waitForSelector(sel, { visible: true, timeout: 8000 });
    await p.tap(sel); await sleep(ms);
  };

  await p.goto(`http://localhost:${PORT}/`, { waitUntil: "networkidle0" });
  await waitSessions(p);
  await openSession(p);
  await shot("android-chat");

  // Drawer (sessions + groupes + actions).
  await tap("#tb-menu", 500);
  await shot("android-drawer");
  // Retour chat puis fichiers plein écran via le sheet ⋮.
  await p.evaluate(() => document.getElementById("scrim")?.click());
  await sleep(350);
  await tap("#chat-more", 350);
  await clickText(p, "#omenu .om-item", "fichier|files");
  await sleep(700);
  await shot("android-files");

  // Paysage : retour au chat, puis rotation (non référencé, gardé à jour).
  await p.evaluate(() => document.getElementById("file-close")?.click());
  await sleep(500);
  await p.setViewport({ width: 915, height: 412, deviceScaleFactor: 2.625,
                      isMobile: true, hasTouch: true });
  await sleep(700);
  await shot("android-landscape", 500);
  await p.close();
}

await browser.close();
server.kill();
console.log("Terminé →", OUT);
