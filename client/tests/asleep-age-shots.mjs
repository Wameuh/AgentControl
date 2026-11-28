// Shots de l'âge d'état « asleep » (#383) : badge visible « 3 h » +
// tooltip « asleep · for 3 h · … » sur une session endormie, rendu
// inchangé pour wait-asleep (âge du waiter, pas de double comptage).
// Les metas mockées sont patchées APRÈS tauri-mock.js : list_sessions
// renvoie les sessions du mock avec state/state_since/wait injectés.
// Usage : npm run build && node tests/asleep-age-shots.mjs
import puppeteer from "puppeteer-core";
import { readFileSync, mkdirSync } from "node:fs";
import { startVitePreview } from "./lib/vite-preview.mjs";

const EXE = process.env.CHROME_BIN
  ?? `${process.env.HOME}/.cache/puppeteer/chrome-headless-shell/linux-156.0.8078.47/chrome-headless-shell-linux64/chrome-headless-shell`;
const { server, base: BASE } = await startVitePreview();
const b = await puppeteer.launch({ executablePath: EXE, headless: true, args: ["--no-sandbox"] });
const p = await b.newPage();
await p.setViewport({ width: 1280, height: 800, deviceScaleFactor: 1 });

// tauri-mock d'abord, puis patch list_sessions : l'âge est calculé au
// moment du rendu — state_since relatif à Date.now() du navigateur.
await p.evaluateOnNewDocument(readFileSync("tests/tauri-mock.js", "utf8"));
await p.evaluateOnNewDocument(`(() => {
  const inv = window.__TAURI_INTERNALS__.invoke;
  window.__TAURI_INTERNALS__.invoke = (cmd, args) => {
    if (!(cmd === "call" && args?.method === "list_sessions"))
      return inv(cmd, args);
    return inv(cmd, args).then((list) => {
      const now = Math.floor(Date.now() / 1000);
      for (const s of list) {
        if (s.id === "s2") {           // idle → endormie depuis ~3 h
          s.state = "asleep";
          s.state_since = now - (3 * 3600 + 7 * 60 + 12);
        }
        if (s.id === "s3") {           // dead → endormie sous waiter
          s.state = "wait-asleep";
          s.state_since = now - (5 * 3600);
          s.wait = { secs: 754, name: "deploy-watch", timeout: 7200 };
        }
        if (s.id === "s4") {           // waiting_input → âge aussi tracé
          s.state_since = now - 95;
        }
      }
      return list;
    });
  };
})()`);

mkdirSync("shots", { recursive: true });
await p.goto(BASE, { waitUntil: "networkidle0" });
await new Promise((r) => setTimeout(r, 900));
await p.screenshot({ path: "shots/asleep-age-sidebar.png" });
console.log("-> shots/asleep-age-sidebar.png");

// Tooltip de la gemme endormie (title natif — vérifié dans le DOM).
const tips = await p.evaluate(() =>
  [...document.querySelectorAll("#session-list .sess .dot")].map((d) => d.title),
);
console.log(tips);
await b.close();
server.kill();
