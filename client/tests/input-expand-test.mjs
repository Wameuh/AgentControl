// Test manuel du composer auto-grow + bouton expand (item 5 backlog).
// Usage : npm run build && node tests/input-expand-test.mjs
//   (chromium via $CHROMIUM ou /usr/bin/chromium)
import puppeteer from "puppeteer-core";
import { mkdirSync, readFileSync } from "node:fs";
import { startVitePreview } from "./lib/vite-preview.mjs";

mkdirSync("shots", { recursive: true });
// vite preview : port libre + poll HTTP jusqu'à réponse — la sortie
// vite n'est pas un contrat fiable (muette >60 s sur runner GHA).
const { server, base: BASE } = await startVitePreview();
const CHROMIUM = process.env.CHROMIUM ?? "/usr/bin/chromium";
const b = await puppeteer.launch({
  executablePath: CHROMIUM,
  headless: true,
  args: ["--no-sandbox"],
});
const p = await b.newPage();
await p.setViewport({ width: 1280, height: 800 });
await p.evaluateOnNewDocument(readFileSync("tests/tauri-mock.js", "utf8"));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const shot = async (n) => { await p.screenshot({ path: `shots/${n}.png` }); console.log("->", n); };

await p.goto(BASE + "/#desktop", { waitUntil: "networkidle0" });
await sleep(400);
// Connexion mock → ouvrir la 1re session
await p.click("#btn-connect").catch(() => {});
await sleep(300);
await p.type("#c-host", "me@pi5.local").catch(() => {});
await p.click("#c-go").catch(() => {});
await sleep(900);
await p.click("#session-list .sess").catch(() => {});
await sleep(800);

const st = await p.evaluate(() => {
  const inp = document.getElementById("input");
  const btn = document.getElementById("btn-expand");
  const chat = document.getElementById("chat");
  return {
    baseline: inp.offsetHeight, btnHidden: btn.hidden, chatH: chat.clientHeight,
    smallCap: Math.max(100, Math.round(chat.clientHeight * 0.33)),
    bigCap: Math.max(Math.max(100, Math.round(chat.clientHeight * 0.33)) + 80, Math.round(chat.clientHeight * 0.66)),
  };
});
console.log("baseline:", st);

// Coller ~40 lignes → dépasse le cap
await p.evaluate(() => {
  const inp = document.getElementById("input");
  inp.value = Array.from({ length: 40 }, (_, i) => `ligne ${i + 1} du prompt très long`).join("\n");
  inp.dispatchEvent(new Event("input", { bubbles: true }));
});
await sleep(200);
const over = await p.evaluate(() => {
  const inp = document.getElementById("input");
  const btn = document.getElementById("btn-expand");
  return { h: inp.offsetHeight, btnHidden: btn.hidden, txt: btn.textContent.trim(), overCls: document.getElementById("input-wrap").className };
});
console.log("overflowed:", over);
await shot("input-1-collapsed");

// Expand
await p.click("#btn-expand");
await sleep(200);
const exp = await p.evaluate(() => ({
  h: document.getElementById("input").offsetHeight,
  txt: document.getElementById("btn-expand").textContent.trim(),
}));
console.log("expanded:", exp);
await shot("input-2-expanded");

// Collapse
await p.click("#btn-expand");
await sleep(200);
const col = await p.evaluate(() => ({
  h: document.getElementById("input").offsetHeight,
  txt: document.getElementById("btn-expand").textContent.trim(),
}));
console.log("collapsed:", col);

// Envoi → reset (session s1 mock accepte "send" → null)
await p.evaluate(() => document.getElementById("btn-send").click());
await sleep(400);
const sent = await p.evaluate(() => ({
  h: document.getElementById("input").offsetHeight,
  v: document.getElementById("input").value,
  btnHidden: document.getElementById("btn-expand").hidden,
}));
console.log("sent:", sent);

// Vérifs
const ok =
  !over.btnHidden && over.h <= st.smallCap + 2 && over.txt.includes("40") &&
  exp.h > col.h && exp.h <= st.bigCap + 2 &&
  sent.btnHidden && sent.h === st.baseline;
console.log(ok ? "PASS" : "FAIL");
await b.close();
server.kill();
process.exit(ok ? 0 : 1);
