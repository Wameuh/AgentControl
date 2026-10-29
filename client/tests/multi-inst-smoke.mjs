// Smoke DOM — multi-instances : 1 instance = 1 connexion, accent par
// connexion, titre de fenêtre nommé, bouton « nouvelle instance »
// (spawn_instance) et re-bind exclusif (l'ancienne conn est évincée).
// Deux pages Chromium = deux instances, chacune avec sa conn mockée.
// Usage : node tests/multi-inst-smoke.mjs   (après npm run build)
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";

const EXE = process.env.CHROME_BIN ?? "/usr/bin/chromium";
// detached:true → kill(-pid) tue tout le groupe (npx → sh → vite) : sinon le
// preview survit orphelin après la fin du test.
const server = spawn("npx", ["vite", "preview", "--port", "4203", "--strictPort"], { stdio: "ignore", detached: true });
await new Promise((r) => setTimeout(r, 1800));
const b = await puppeteer.launch({ executablePath: EXE, headless: true, args: ["--no-sandbox"] });
const mock = readFileSync("tests/tauri-mock.js", "utf8");

// Patch par « instance » : saved_connections + cli_conn propres à la
// page, sessions retaggées sur les conns encore bindées (connect/disconnect
// trackés — comme le routeur conn du backend), appels enregistrés dans
// window.__calls.
const PATCH = `(() => {
  const inv = window.__TAURI_INTERNALS__.invoke;
  const keyOf = (c) => !c ? "" : c.kind === "ssh" ? \`\${c.native ? "ssh-native" : "ssh"}:\${c.host}:\${c.port ?? 22}\`
    : c.kind === "tcp" ? \`tcp:\${c.addr}\` : c.kind === "web" ? "web" : "local";
  window.__live = new Set(); window.__saved = []; window.__cliConn = null;
  window.__calls = { spawn: [], disconnect: [] };
  window.__TAURI_INTERNALS__.invoke = (cmd, args) => {
    if (cmd === "saved_connections") return Promise.resolve(window.__saved);
    if (cmd === "cli_conn") return Promise.resolve(window.__cliConn);
    if (cmd === "spawn_instance") { window.__calls.spawn.push(args?.conn); return Promise.resolve(null); }
    if (cmd === "connect") window.__live.add(keyOf(args?.cfg));
    if (cmd === "disconnect") { window.__calls.disconnect.push(args?.conn); window.__live.delete(args?.conn); }
    return inv(cmd, args).then((r) =>
      cmd === "call" && args?.method === "list_sessions" && Array.isArray(r)
        ? r.map((s) => ({ ...s, conn: args?.conn ?? [...window.__live].pop() ?? "local" }))
        : r);
  };
})();`;
const initPage = async (saved, cliConn) => {
  const p = await b.newPage();
  await p.setViewport({ width: 1280, height: 860 });
  await p.evaluateOnNewDocument(mock);
  await p.evaluateOnNewDocument(PATCH);
  await p.evaluateOnNewDocument(`window.__saved = ${JSON.stringify(saved)}; window.__cliConn = ${JSON.stringify(cliConn)};`);
  await p.goto("http://localhost:4203/#desktop", { waitUntil: "domcontentloaded" });
  await p.waitForSelector("#btn-connect", { timeout: 15000 });
  await sleep(1200);
  return p;
};

let fails = 0;
const ok = (c, n) => { console.log((c ? "  PASS " : "  FAIL ") + n); if (!c) fails++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------- Instance A : conn "local" couleur violet ----------
const SAVED_A = [{ kind: "local", color: "violet" }, { kind: "ssh", host: "me@pi5.local", remote_port: 7331, port: 22, native: true, color: "gold" }];
const pA = await initPage(SAVED_A, null);

const clsA = () => pA.evaluate(() => [...document.documentElement.classList].join(" "));
const titleA = () => pA.evaluate(() => document.title);
const connHeaders = (p) => p.evaluate(() => [...document.querySelectorAll(".grp-head .grp-name")].map((n) => n.textContent.trim()));

ok((await clsA()).includes("conn-tinted"), "A1 :root.conn-tinted posé (conn colorée liée)");
ok((await clsA()).includes("theme-violet"), "A2 accent violet appliqué (couleur de la conn)");
ok(!(await pA.evaluate(() => localStorage.getItem("ac-accent"))) || (await pA.evaluate(() => localStorage.getItem("ac-accent"))) === "sage",
  "A3 accent global non pollué (persist=false)");
ok((await titleA()) === "AgentControl — local", `A4 titre fenêtre nommé (${await titleA()})`);
ok((await pA.evaluate(() => document.querySelector("#titlebar-title")?.textContent)) === "AgentControl — local", "A5 titlebar custom nommée");
const headsA = await connHeaders(pA);
ok(headsA.length === 1 && headsA[0] === "local", `A6 une seule conn liée (${headsA})`);
ok((await pA.evaluate(() => document.querySelectorAll(".sess").length)) === 4, "A7 sessions de la conn visibles");

// ---------- Instance B : conn ssh-native couleur gold, visée par --conn ----------
const SAVED_B = [{ kind: "local" }, { kind: "ssh", host: "me@pi5.local", remote_port: 7331, port: 22, native: true, color: "gold" }];
const KEY_B = "ssh-native:me@pi5.local:22";
const pB = await initPage(SAVED_B, KEY_B);

const clsB = () => pB.evaluate(() => [...document.documentElement.classList].join(" "));
ok((await clsB()).includes("theme-gold"), "B1 accent gold appliqué (--conn ciblée)");
ok((await pB.evaluate(() => document.title)) === "AgentControl — me@pi5.local", "B2 titre = hôte de la conn ciblée");
const headsB = await connHeaders(pB);
ok(headsB.length === 1 && headsB[0] === KEY_B, `B3 une seule conn, = --conn (${headsB})`);
ok(!(await clsB()).includes("theme-violet"), "B4 indépendance des instances (pas l'accent de A)");

// ---------- C : « ouvrir dans une nouvelle instance » depuis la vue ----------
await pA.evaluate(() => document.querySelector("#btn-connect")?.click());
await sleep(400);
ok(!(await pA.evaluate(() => document.querySelector("#conn-view")?.hidden)),
  "C0 vue connexion ouverte dans #chat (pas un dialog)");
await pA.evaluate(() =>
  document.querySelectorAll("#cv-saved .cv-conn .cv-new")[1]?.click());
await sleep(300);
ok((await pA.evaluate(() => window.__calls.spawn[0])) === "ssh-native:me@pi5.local:22", "C1 spawn_instance avec la clé connKey de la saved conn");

// ---------- D : re-bind exclusif — « connecter ici » évince la 1re conn ----------
await pA.evaluate(() =>
  document.querySelectorAll("#cv-saved .cv-conn .cv-go")[1]?.click());
await sleep(900);
ok((await clsA()).includes("theme-gold"), "D1 re-bind → accent de la nouvelle conn");
ok((await titleA()) === "AgentControl — me@pi5.local", "D2 titre mis à jour au re-bind");
const headsA2 = await connHeaders(pA);
ok(headsA2.length === 1 && headsA2[0] === KEY_B, `D3 ancienne conn évincée (${headsA2})`);
ok((await pA.evaluate(() => window.__calls.disconnect)).includes("local"), "D4 disconnect invoqué sur l'ancienne conn");

console.log(`\n${fails === 0 ? "SMOKE OK" : "SMOKE FAIL"} — ${fails} échec(s)`);
await b.close();
// SIGKILL sur le groupe : vite preview ignore SIGTERM dans certains runs
// (preview orphelin déjà observé après une fin « propre »).
try { process.kill(-server.pid, "SIGKILL"); } catch {}
process.exit(fails ? 1 : 0);
