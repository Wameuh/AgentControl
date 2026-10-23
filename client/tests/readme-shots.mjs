import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";

const server = spawn("npx", ["vite", "preview", "--port", "4183", "--strictPort"], { stdio: "ignore" });
await new Promise(r => setTimeout(r, 1800));
const b = await puppeteer.launch({ executablePath: "/usr/bin/chromium", headless: "new", args: ["--no-sandbox"] });
const mock = readFileSync("tests/tauri-mock.js", "utf8");
const shot = (p, n) => p.screenshot({ path: `shots/readme/${n}.png` });
const dom = (p, s) => p.evaluate((x) => document.querySelector(x)?.click(), s);

async function connect(p) {
  await dom(p, "#btn-connect"); await new Promise(r=>setTimeout(r,300));
  await p.type("#c-host","me@pi5.local"); await dom(p, "#c-go");
  await new Promise(r=>setTimeout(r,900));
}

// ---------- DESKTOP ----------
{
  const p = await b.newPage();
  await p.setViewport({ width: 1280, height: 860, deviceScaleFactor: 1.4 });
  await p.evaluateOnNewDocument(mock);
  await p.goto("http://localhost:4183/#desktop", { waitUntil: "domcontentloaded" });
  await p.waitForSelector("#btn-connect", { timeout: 15000 });
  await connect(p);
  // groupe nommé visible dans la sidebar
  await dom(p, ".grp-add"); await p.type("#g-name", "Prod"); await dom(p, "#g-ok");
  await new Promise(r=>setTimeout(r,500));
  await p.evaluate(() => {
    const dt = new DataTransfer();
    dt.setData("application/x-ac-session", JSON.stringify({ id: "s1", conn: "local" }));
    const t = [...document.querySelectorAll(".grp-sub")].find(g => g.textContent.includes("Prod"));
    t?.dispatchEvent(new DragEvent("drop", { dataTransfer: dt, bubbles: true, cancelable: true }));
  });
  await new Promise(r=>setTimeout(r,600));
  await p.evaluate(() => [...document.querySelectorAll(".sess")].find(s=>s.textContent.includes("Refactor"))?.click());
  await new Promise(r=>setTimeout(r,900));
  await p.evaluate(() => { document.querySelector("#events").scrollTop = 99999; });
  await new Promise(r=>setTimeout(r,300));
  await shot(p, "desktop-chat");
  // panneau fichiers
  await p.evaluate(() => document.querySelector("#btn-files")?.click() ?? document.querySelector("#chat-files")?.click());
  await new Promise(r=>setTimeout(r,800));
  await p.evaluate(() => [...document.querySelectorAll("#file-tree .frow")].find(e=>e.textContent.includes("src"))?.click());
  await new Promise(r=>setTimeout(r,500));
  await shot(p, "desktop-files");
  await p.close();
}

// ---------- ANDROID ----------
{
  const p = await b.newPage();
  await p.setViewport({ width: 412, height: 915, deviceScaleFactor: 2, isMobile: true, hasTouch: true });
  await p.evaluateOnNewDocument(mock);
  await p.goto("http://localhost:4183/", { waitUntil: "domcontentloaded" });
  await p.waitForSelector("#btn-connect", { timeout: 15000 });
  await connect(p);
  await p.evaluate(() => [...document.querySelectorAll(".sess")].find(s=>s.textContent.includes("Refactor"))?.click());
  await new Promise(r=>setTimeout(r,900));
  await p.evaluate(() => { document.querySelector("#events").scrollTop = 99999; });
  await new Promise(r=>setTimeout(r,300));
  await shot(p, "android-chat");
  // drawer
  await p.evaluate(() => document.querySelector("#tb-menu, #btn-menu, .hamburger")?.click() ?? document.querySelector("#burger")?.click());
  await new Promise(r=>setTimeout(r,600));
  await shot(p, "android-drawer");
  // fichiers plein écran
  await p.evaluate(() => document.querySelector("#btn-files")?.click() ?? document.querySelector("#chat-files")?.click());
  await new Promise(r=>setTimeout(r,800));
  await shot(p, "android-files");
  await p.close();
}

await b.close(); server.kill();
