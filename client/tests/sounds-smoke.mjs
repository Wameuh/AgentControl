// Smoke DOM — alertes sonores : diff d'états list_sessions → lecture WebAudio.
// Couvre : seed silencieux, transitions done/question/error, pas de rejeu
// d'un état connu, cooldown par type, toggles par type, master off,
// volume, option « fenêtre sans focus », boutons « Tester », preset choisi.
// Usage : node tests/sounds-smoke.mjs   (après npm run build)
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";

const EXE = process.env.CHROME_BIN ?? "/usr/bin/chromium";
// Port configurable — strictPort + stdio ignorés rendent un conflit muet
// (le test taperait alors sur le dist d'un AUTRE preview déjà lancé).
const PORT = Number(process.env.SMOKE_PORT ?? 4199);
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });
await new Promise((r) => setTimeout(r, 1800));
const b = await puppeteer.launch({ executablePath: EXE, headless: true, args: ["--no-sandbox", "--autoplay-policy=no-user-gesture-required"] });
const mock = readFileSync("tests/tauri-mock.js", "utf8");
// Patch : capture des handlers d'events Tauri (__emit → status_changed
// déclenche sidebar.refresh), états de sessions réassignables (__states)
// et espion des lectures audio — la lecture passe par le graphe WebAudio
// (emit → master → compresseur → ctx.destination), donc :
//   · un « play » = un nœud connecté à ctx.destination ;
//   · le gain maître = gain.value du GainNode connecté juste avant ;
//   · l'événement se lit sur la fréquence du premier oscillateur démarré
//     dans la foulée (f0 — voir F0 ci-dessous).
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
  let __lastGain = 0;
  const __conn = AudioNode.prototype.connect;
  AudioNode.prototype.connect = function (d, ...rest) {
    if (this.gain) __lastGain = this.gain.value;
    if (d === this.context.destination)
      window.__played.push({ gain: __lastGain, f0: null });
    return __conn.call(this, d, ...rest);
  };
  const __ostart = OscillatorNode.prototype.start;
  OscillatorNode.prototype.start = function (...a) {
    const p = window.__played.at(-1);
    if (p && p.f0 === null) p.f0 = this.frequency.value;
    return __ostart.apply(this, a);
  };
})();`;

let fails = 0;
const ok = (c, n) => { console.log((c ? "  PASS " : "  FAIL ") + n); if (!c) fails++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const p = await b.newPage();
await p.setViewport({ width: 1280, height: 860 });
await p.evaluateOnNewDocument(mock);
await p.evaluateOnNewDocument(patch);
await p.goto(`http://localhost:${PORT}/#desktop`, { waitUntil: "domcontentloaded" });
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
// Fréquence du premier oscillateur du preset « classic » de chaque
// événement — l'équivalent des anciens chemins /sounds/<kind>.wav.
const F0 = { done: 659.26, question: 587.33, error: 220.0 };
// AudioParam.value est float32 → comparaison par proximité, pas égalité.
const near = (f, ref) => f !== null && Math.abs(f - ref) < 0.05;
const played = () => ev(() => window.__played.map((x) => x.f0));
const lastVol = () => ev(() => window.__played.at(-1)?.gain);
const countKind = async (k) => (await played()).filter((f) => near(f, F0[k])).length;
const lsSet = (k, v) => ev(({ k, v }) => localStorage.setItem(k, v), { k, v });

// ---------- A. Seed silencieux ----------
// États initiaux du mock : s1 working, s2 idle, s3 dead, s4 compacting —
// première observation = pas de son (waiting/dead déjà connus).
ok((await played()).length === 0, "A1 seed initial silencieux");

// ---------- B. Transitions ----------
await setState("s2", "working"); await emitState("s2", "working"); await sleep(500);
ok((await played()).length === 0, "B1 idle→working : pas de déclencheur");
await setState("s2", "idle"); await emitState("s2", "idle"); await sleep(500);
ok((await countKind("done")) === 1, "B2 working→idle → done");
ok(Math.abs((await lastVol()) - 0.6) < 0.001, "B3 volume par défaut 0.6");

// Rejeu d'un état déjà connu : deuxième status_changed idle → silence.
await emitState("s2", "idle"); await sleep(500);
ok((await countKind("done")) === 1, "B4 état déjà connu → pas de rejeu");

await setState("s4", "waiting_input"); await emitState("s4", "waiting_input"); await sleep(500);
ok((await countKind("question")) === 1, "B5 →waiting_input → question");

await setState("s4", "dead"); await emitState("s4", "dead"); await sleep(500);
ok((await countKind("error")) === 1, "B6 waiting_input→dead → error");

// dead→interrupted = même famille d'erreur → pas de second son.
await setState("s4", "interrupted"); await emitState("s4", "interrupted"); await sleep(500);
ok((await countKind("error")) === 1, "B7 dead→interrupted : pas de rejeu");

// ---------- C. Cooldown ~2 s par type ----------
// Deux sessions passent waiting_input dans le même refresh → 1 seule
// question (rafale absorbée par le cooldown). >2 s depuis B5 pour sortir
// du cooldown de la question précédente.
await sleep(2100);
await setState("s1", "waiting_input"); await setState("s2", "waiting_input");
await emitState("s1", "waiting_input"); await sleep(600);
ok((await countKind("question")) === 2, "C1 rafale → 1 son question (cooldown)");

// ---------- D. Toggles ----------
await lsSet("ac-snd-question", "0");
await setState("s3", "waiting_input"); await emitState("s3", "waiting_input"); await sleep(600);
ok((await countKind("question")) === 2, "D1 toggle question off → silence");
await lsSet("ac-snd-question", "1");

// ---------- E. Master off ----------
await lsSet("ac-snd", "0");
await setState("s1", "dead"); await emitState("s1", "dead"); await sleep(600);
ok((await countKind("error")) === 1, "E1 master off → silence");
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
ok((await countKind("error")) === 1, "G1 focus + option → silence");
await ev(() => { document.hasFocus = () => false; });
// interrupted → waiting_input = sortie de la famille erreur → « question ».
await setState("s4", "waiting_input"); await emitState("s4", "waiting_input"); await sleep(600);
ok((await countKind("question")) === 3, "G2 sans focus → son joué");
await lsSet("ac-snd-unfocused", "0");

// ---------- H. Boutons « Tester » (jouent même master off) ----------
await p.evaluate(() => document.querySelector("#tb-settings")?.click());
await sleep(400);
await lsSet("ac-snd", "0");
await p.evaluate(() => document.querySelector("#s-snd")?.click()); // resync case + reste off
await sleep(200);
ok(await ev(() => document.querySelector("#s-snd-done")?.disabled === true), "H1 master off → toggles inertes");
ok(await ev(() => document.querySelector("#s-snd-preset-done")?.disabled === true), "H1b selects de presets inertes aussi");
ok(await ev(() => document.querySelector("#s-snd-test-done")?.disabled === false), "H2 bouton Tester reste cliquable");
const beforeQ = await countKind("question");
await p.evaluate(() => document.querySelector("#s-snd-test-question")?.click());
await sleep(300);
ok((await countKind("question")) === beforeQ + 1, "H3 Tester joue même master off");
ok(Math.abs((await lastVol()) - 0.6) < 0.001, "H4 Tester au volume réglé (60 %)");
await p.evaluate(() => document.querySelector("#s-snd")?.click()); // réactive
await sleep(200);
ok(await ev(() => document.querySelector("#s-snd-done")?.disabled === false), "H5 master on → toggles actifs");

// ---------- I. Preset choisi honoré ----------
// « bell » démarre sur son bourdon (ratio 0,5) : f0 = 659.26/2 ≠ classic.
await lsSet("ac-snd-preset-done", "bell");
await setState("s1", "working"); await emitState("s1", "working"); await sleep(400);
await setState("s1", "idle"); await emitState("s1", "idle"); await sleep(600);
ok(near((await played()).at(-1), 329.63), "I1 preset « bell » joué (f0 bourdon)");
// id inconnu → retour au défaut classic (> 2 s pour sortir du cooldown done).
await sleep(2100);
await lsSet("ac-snd-preset-done", "nope");
await setState("s1", "working"); await emitState("s1", "working"); await sleep(400);
await setState("s1", "idle"); await emitState("s1", "idle"); await sleep(600);
ok(near((await played()).at(-1), F0.done), "I2 preset inconnu → classic");
// Choisir dans le select persiste la clé (câblage settings).
await ev(() => {
  const s = document.querySelector("#s-snd-preset-question");
  s.value = "retro"; s.dispatchEvent(new Event("change"));
});
await sleep(300);
ok(await ev(() => localStorage.getItem("ac-snd-preset-question") === "retro"), "I3 select → localStorage");
ok(near((await played()).at(-1), F0.question), "I4 changement de select → écoute immédiate");

console.log(`\n${fails === 0 ? "SMOKE OK" : "SMOKE FAIL"} — ${fails} échec(s)`);
await b.close(); server.kill();
process.exit(fails ? 1 : 0);
