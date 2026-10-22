// Captures Android : Chromium headless, viewport S25, mock __TAURI_INTERNALS__.
// Usage : npm run build && node tests/android-shots.mjs [outdir]
import puppeteer from "puppeteer-core";
import { mkdirSync } from "node:fs";
import { spawn } from "node:child_process";

const OUT = process.argv[2] || "shots";
mkdirSync(OUT, { recursive: true });

const server = spawn("npx", ["vite", "preview", "--port", "4173", "--strictPort"], {
  cwd: new URL("..", import.meta.url).pathname, stdio: "ignore",
});
await new Promise((r) => setTimeout(r, 1800));

const browser = await puppeteer.launch({
  executablePath: "/usr/bin/chromium",
  headless: "new",
  args: ["--no-sandbox", "--disable-dev-shm-usage", "--font-render-hinting=none"],
});
const page = await browser.newPage();
await page.setUserAgent(
  "Mozilla/5.0 (Linux; Android 15; SM-S931B Build/AP3A.240905.015) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36");
await page.setViewport({ width: 412, height: 915, deviceScaleFactor: 2.625, isMobile: true, hasTouch: true });
await page.evaluateOnNewDocument(await (await import("node:fs/promises")).readFile(new URL("./tauri-mock.js", import.meta.url), "utf8"));
page.on("pageerror", (e) => console.log("[pageerror]", e.message));

const shot = async (name, ms = 350) => {
  await new Promise((r) => setTimeout(r, ms));
  await page.screenshot({ path: `${OUT}/${name}.png` });
  console.log("→", name);
};
const tap = async (sel, ms = 300) => {
  await page.waitForSelector(sel, { visible: true, timeout: 5000 });
  await page.tap(sel);
  await new Promise((r) => setTimeout(r, ms));
};
const longPress = async (sel) => {
  const el = await page.$(sel);
  const box = await el.boundingBox();
  const x = box.x + box.width / 2, y = box.y + Math.min(box.height / 2, 30);
  const cdp = await page.createCDPSession();
  await cdp.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x, y, id: 1 }] });
  await new Promise((r) => setTimeout(r, 650));
  await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
};

await page.goto("http://localhost:4173/", { waitUntil: "networkidle0" });
await shot("01-app");

await tap("#tb-menu", 450);           await shot("02-drawer");
await tap("#btn-connect", 500);       await shot("03-connect-sheet");

// Remplir host + Connecter → mock retourne OK → sessions listées
await page.type("#c-host", "me@pi5.local");
await tap("#c-go", 900);              await shot("04-connected");

await tap(".sess", 800);              await shot("05-chat");           // ouvre s1, drawer fermé
await tap("#chat-more", 400);         await shot("06-chat-sheet");
await page.tap("#scrim").catch(() => {});  // ferme le sheet
await new Promise((r) => setTimeout(r, 300));

await tap("#chat-more", 300);
await page.evaluate(() => [...document.querySelectorAll("#omenu .om-item")].find((b) => /fichier|files/i.test(b.textContent))?.click());
await shot("07-files", 500);
await tap(".fnode:not(.is-dir)", 700); await shot("08-fileview");
await tap("#fv-back", 400);                                        // retour liste
await longPress(".fnode.is-dir");      await shot("09-fctx", 400);
await page.evaluate(() => { document.getElementById("fctx").hidden = true; });

// Nouvelle session → bottom sheet (depuis le drawer)
await page.evaluate(() => { document.getElementById("file-panel").hidden = true; document.getElementById("file-viewer").classList.remove("open"); });
await tap("#tb-menu", 400);
await tap("#btn-new-session", 500);    await shot("10-new-session");

await browser.close();
server.kill();
console.log("Terminé →", OUT);
