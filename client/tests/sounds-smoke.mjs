// Smoke DOM — alertes sonores : diff d'états list_sessions → lecture Audio.
// Couvre : seed silencieux, transitions done/question/error, pas de rejeu
// d'un état connu, cooldown par type, toggles par type, master off,
// volume, option « fenêtre sans focus », boutons « Tester ».
// Usage : node tests/sounds-smoke.mjs   (après npm run build)
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";

const EXE = process.env.CHROME_BIN ?? "/usr/bin/chromium";
const server = spawn("npx", ["vite", "preview", "--port", "4199", "--strictPort"], { stdio: "ignore" });
await new Promise((r) => setTimeout(r, 1800));
const b = await puppeteer.launch({ executablePath: EXE, headless: true, args: ["--no-sandbox", "--autoplay-policy=no-user-gesture-required"] });
const mock = readFileSync("tests/tauri-mock.js", "utf8");
// Patch : capture des handlers d'events Tauri (__emit → status_changed
// déclenche sidebar.refresh), états de sessions réassignables (__states)
// et espion des lectures audio (__played = {src, volume}).
const patch = `(() => {
  window.__evtHandlers = {};
  window.__emit = (event, payload) => {
    const id = window.__evtHandlers[event];
    if (id != null) window.__TAURI_INTERNALS__.runCallback(id, { payload });
    return id != null;
  };
  window.__states = {};
  window.__played = [];
  const __inv = window.__TAURI_INTERNALS__.invoke;
  window.__TAURI_INTERNALS__.invoke = (cmd, args) => {
    if (cmd === "plugin:event|listen" && args && args.event) {
      window.__evtHandlers[args.event] = args.handler;
    }
    if (cmd === "call" && args?.method === "list_sessions") {
      return __inv(cmd, args).then((r) =>
        Array.isArray(r)
          ? r.map((s) => (s.id in window.__states ? { ...s, state: window.__states[s.id] } : s))
          : r);
    }
    return __inv(cmd, args);
  };
  HTMLMediaElement.prototype.play = function () {
    window.__played.push({ src: this.currentSrc || this.src, volume: this.volume });
    return Promise.resolve();
  };
})();`;

let fails = 0;
const ok = (c, n) => { console.log((c ? "  PASS " : "  FAIL ") + n); if (!c) fails++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const p = await b.newPage();
await p.setViewport({ width: 1280, height: 860 });
await p.evaluateOnNewDocument(mock);
await p.evaluateOnNewDocument(patch);
await p.goto("http://localhost:4199/#desktop", { waitUntil: "domcontentloaded" });
await p.waitForSelector("#btn-connect", { timeout: 15000 });
await p.evaluate(() => document.querySelector("#btn-connect")?.click());
await sleep(300);
await p.type("#c-host", "me@pi5.local");
await p.evaluate(() => document.querySelector("#c-go")?.click());
await sleep(1200);

// helpers page
const ev = (fn, ...a) => p.evaluate(fn, ...a);
const setState = (id, st) => ev(({ id, st }) => { window.__states[id] = st; }, { id, st });
const emitState = (id, st) => ev(({ id, st }) =>
  window.__emit("session-event", { session: id, event: { seq: 1, ts: "t", type: "status_changed", state: st } }), { id, st });
const played = () => ev(() => window.__played.map((x) => x.src));
const lastVol = () => ev(() => window.__played.at(-1)?.volume);
const countSrc = async (frag) => (await played()).filter((s) => s.includes(frag)).length;
const lsSet = (k, v) => ev(({ k, v }) => localStorage.setItem(k, v), { k, v });

// ---------- A. Seed silencieux ----------
// États initiaux du mock : s1 working, s2 idle, s3 dead, s4 compacting —
// première observation = pas de son (waiting/dead déjà connus).
ok((await played()).length === 0, "A1 seed initial silencieux");

// ---------- B. Transitions ----------
await setState("s2", "working"); await emitState("s2", "working"); await sleep(500);
ok((await played()).length === 0, "B1 idle→working : pas de déclencheur");
await setState("s2", "idle"); await emitState("s2", "idle"); await sleep(500);
ok((await played()).length === 1 && (await played())[0].includes("/sounds/done.wav"), "B2 working→idle → done.wav");
ok(Math.abs((await lastVol()) - 0.6) < 0.001, "B3 volume par défaut 0.6");

// Rejeu d'un état déjà connu : deuxième status_changed idle → silence.
await emitState("s2", "idle"); await sleep(500);
ok((await countSrc("done.wav")) === 1, "B4 état déjà connu → pas de rejeu");

await setState("s4", "waiting_input"); await emitState("s4", "waiting_input"); await sleep(500);
ok((await countSrc("question.wav")) === 1, "B5 →waiting_input → question.wav");

await setState("s4", "dead"); await emitState("s4", "dead"); await sleep(500);
ok((await countSrc("error.wav")) === 1, "B6 waiting_input→dead → error.wav");

// dead→interrupted = même famille d'erreur → pas de second son.
await setState("s4", "interrupted"); await emitState("s4", "interrupted"); await sleep(500);
ok((await countSrc("error.wav")) === 1, "B7 dead→interrupted : pas de rejeu");

// ---------- C. Cooldown ~2 s par type ----------
// Deux sessions passent waiting_input dans le même refresh → 1 seule
// question (rafale absorbée par le cooldown). >2 s depuis B5 pour sortir
// du cooldown de la question précédente.
await sleep(2100);
await setState("s1", "waiting_input"); await setState("s2", "waiting_input");
await emitState("s1", "waiting_input"); await sleep(600);
ok((await countSrc("question.wav")) === 2, "C1 rafale → 1 son question (cooldown)");

// ---------- D. Toggles ----------
await lsSet("ac-snd-question", "0");
await setState("s3", "waiting_input"); await emitState("s3", "waiting_input"); await sleep(600);
ok((await countSrc("question.wav")) === 2, "D1 toggle question off → silence");
await lsSet("ac-snd-question", "1");

// ---------- E. Master off ----------
await lsSet("ac-snd", "0");
await setState("s1", "dead"); await emitState("s1", "dead"); await sleep(600);
ok((await countSrc("error.wav")) === 1, "E1 master off → silence");
await lsSet("ac-snd", "1");

// ---------- F. Volume ----------
// s2 est waiting_input → retour idle = « done » (waiting_input ∈ busy).
await lsSet("ac-snd-vol", "30");
await setState("s2", "idle"); await emitState("s2", "idle"); await sleep(600);
ok(Math.abs((await lastVol()) - 0.3) < 0.001, "F1 volume 30 % honoré");
await lsSet("ac-snd-vol", "60");

// ---------- G. « Seulement sans le focus » ----------
await lsSet("ac-snd-unfocused", "1");
await ev(() => { document.hasFocus = () => true; });
await setState("s3", "dead"); await emitState("s3", "dead"); await sleep(600);
ok((await countSrc("error.wav")) === 1, "G1 focus + option → silence");
await ev(() => { document.hasFocus = () => false; });
// interrupted → waiting_input = sortie de la famille erreur → « question ».
await setState("s4", "waiting_input"); await emitState("s4", "waiting_input"); await sleep(600);
ok((await countSrc("question.wav")) === 3, "G2 sans focus → son joué");
await lsSet("ac-snd-unfocused", "0");

// ---------- H. Boutons « Tester » (jouent même master off) ----------
await p.evaluate(() => document.querySelector("#tb-settings")?.click());
await sleep(400);
await lsSet("ac-snd", "0");
await p.evaluate(() => document.querySelector("#s-snd")?.click()); // resync case + reste off
await sleep(200);
ok(await ev(() => document.querySelector("#s-snd-done")?.disabled === true), "H1 master off → toggles inertes");
ok(await ev(() => document.querySelector("#s-snd-test-done")?.disabled === false), "H2 bouton Tester reste cliquable");
const before = await countSrc("question.wav");
await p.evaluate(() => document.querySelector("#s-snd-test-question")?.click());
await sleep(300);
ok((await countSrc("question.wav")) === before + 1, "H3 Tester joue même master off");
ok(Math.abs((await lastVol()) - 0.6) < 0.001, "H4 Tester au volume réglé (60 %)");
await p.evaluate(() => document.querySelector("#s-snd")?.click()); // réactive
await sleep(200);
ok(await ev(() => document.querySelector("#s-snd-done")?.disabled === false), "H5 master on → toggles actifs");

console.log(`\n${fails === 0 ? "SMOKE OK" : "SMOKE FAIL"} — ${fails} échec(s)`);
await b.close(); server.kill();
process.exit(fails ? 1 : 0);
