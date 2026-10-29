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
// Port éphémère (`--port 0`) lu sur la sortie vite — un port fixe est
// squatté par les previews des autres worktrees et le test pilotait
// alors leur build (faux négatifs déjà observés).
const server = spawn("npx", ["vite", "preview", "--port", "0", "--strictPort"], { stdio: ["ignore", "pipe", "ignore"], detached: true });
const BASE = await new Promise((res, rej) => {
  let buf = "";
  const to = setTimeout(() => rej(new Error("vite preview: URL introuvable")), 15000);
  server.stdout.on("data", (d) => {
    buf += d;
    const m = buf.match(/https?:\/\/localhost:(\d+)/);
    if (m) {
      clearTimeout(to);
      res(`http://localhost:${m[1]}`);
    }
  });
});
// Attendre que le preview serve (npx cold peut dépasser 2 s) — un
// goto prématuré donnait ERR_CONNECTION_REFUSED avant le cleanup.
for (let i = 0; i < 40; i++) {
  try {
    const r = await fetch(BASE + "/");
    if (r.ok) break;
  } catch {}
  await new Promise((r) => setTimeout(r, 250));
}
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
  window.__delays = {};
  window.__calls = { spawn: [], disconnect: [] };
  window.__TAURI_INTERNALS__.invoke = (cmd, args) => {
    if (cmd === "saved_connections") return Promise.resolve(window.__saved);
    if (cmd === "cli_conn") return Promise.resolve(window.__cliConn);
    if (cmd === "spawn_instance") { window.__calls.spawn.push(args?.conn); return Promise.resolve(null); }
    if (cmd === "connect") {
      window.__live.add(keyOf(args?.cfg));
      // Latence simulée par conn (__delays) — pour les scénarios de
      // connects croisés (un flux résout longtemps après un autre).
      const d = window.__delays[keyOf(args?.cfg)] ?? 0;
      return new Promise((r) => setTimeout(r, d)).then(() => inv(cmd, args));
    }
    if (cmd === "disconnect") { window.__calls.disconnect.push(args?.conn); window.__live.delete(args?.conn); }
    return inv(cmd, args).then((r) =>
      cmd === "call" && args?.method === "list_sessions" && Array.isArray(r)
        ? r.map((s) => ({ ...s, conn: args?.conn ?? [...window.__live].pop() ?? "local" }))
        : r);
  };
})();`;
const initPage = async (saved, cliConn, delays = {}) => {
  const p = await b.newPage();
  await p.setViewport({ width: 1280, height: 860 });
  await p.evaluateOnNewDocument(mock);
  await p.evaluateOnNewDocument(PATCH);
  await p.evaluateOnNewDocument(`window.__saved = ${JSON.stringify(saved)}; window.__cliConn = ${JSON.stringify(cliConn)}; window.__delays = ${JSON.stringify(delays)};`);
  await p.goto(BASE + "/#desktop", { waitUntil: "domcontentloaded" });
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

// ---------- E : garde de génération — deux connects croisés ----------
// --conn vise SLOW dont le connect reste en vol (mock retardé) ; pendant
// l'attente l'utilisateur connecte FAST via le dialogue → FAST se lie.
// Quand SLOW résout enfin, son flux est périmé (gen ≠ connectSeq) : il
// relâche son transport (disconnect SLOW) sans voler le binding.
const SLOW = { kind: "ssh", host: "me@slow.local", remote_port: 7331, port: 22, native: true, color: "red" };
const FAST = { kind: "ssh", host: "me@fast.local", remote_port: 7331, port: 22, native: true, color: "grey" };
const KEY_SLOW = "ssh-native:me@slow.local:22";
const KEY_FAST = "ssh-native:me@fast.local:22";
const pC = await initPage([SLOW, FAST], KEY_SLOW, { [KEY_SLOW]: 2800 });

await pC.evaluate(() => document.querySelector("#btn-connect")?.click());
await sleep(400);
// Vue connexion (#137) : « connecter ici » = le ▶ de la saved conn
// (la vue s'est aussi auto-ouverte en accueil — SLOW n'est pas up).
await pC.evaluate(() =>
  document.querySelectorAll("#cv-saved .cv-conn .cv-go")[1]?.click());
await sleep(600);
const headsC0 = await connHeaders(pC);
ok(headsC0.length === 1 && headsC0[0] === KEY_FAST, `E1 FAST liée pendant que SLOW est en vol (${headsC0})`);
await sleep(3200); // SLOW résout → garde de génération doit le faire perdre
const headsC1 = await connHeaders(pC);
ok(headsC1.length === 1 && headsC1[0] === KEY_FAST, `E2 le perdant résolu en retard ne vole pas le binding (${headsC1})`);
ok((await pC.evaluate(() => window.__calls.disconnect)).includes(KEY_SLOW), "E3 transport du perdant relâché (disconnect SLOW)");
ok((await pC.evaluate(() => document.title)) === "AgentControl — me@fast.local", "E4 titre = conn gagnante");
ok((await pC.evaluate(() => [...document.documentElement.classList].join(" "))).includes("theme-grey"), "E5 accent = couleur de la conn gagnante");

// ---------- F : vue diagramme — nœuds, arêtes, clic session/composant ----------
// Page liée à la conn "local" : 4 composants mockés (capability
// components:true dans daemon_status — 2 tools : agentbacklog
// installed+version, acctl absent), 4 sessions dont s2 spawnée par s1
// et s4 rattachée au composant backlog-orchestrator via `origin`.
const pD = await initPage([{ kind: "local" }], null);
await pD.evaluate(() => document.querySelector("#tb-map")?.click());
await sleep(900);
const cnt = (sel) => pD.evaluate((s) => document.querySelectorAll(s).length, sel);
ok((await pD.evaluate(() => !document.querySelector("#map-view")?.hidden)), "F1 #tb-map ouvre la vue diagramme");
ok((await cnt(".n-daemon")) === 1, "F2 nœud démon au centre");
ok((await cnt(".n-comp")) === 4, `F3 4 composants en anneau (${await cnt(".n-comp")})`);
ok((await cnt(".n-sess")) === 4, `F4 4 sessions positionnées (${await cnt(".n-sess")})`);
ok((await cnt(".e-dc")) === 4 && (await cnt(".e-s")) === 4,
  `F5 arêtes : 4 démon→composant + 4 parent→session (${await cnt(".e-dc")}+${await cnt(".e-s")})`);
ok((await cnt(".e-s.flow")) >= 1, "F6 flux actif animé (s1 working)");
ok((await cnt(".npr")) >= 1, "F7 badge PR #N sur les sessions du registre");
ok((await pD.evaluate(() => document.querySelector('.n-sess[data-session="s4"]') != null)),
  "F8 s4 (origin component) présente dans le graphe");
// Clic composant → carte info ; clic session → chat ouvert + vue refermée.
await pD.evaluate(() => document.querySelector('[data-comp="backlog-orchestrator"]')
  .dispatchEvent(new MouseEvent("click", { bubbles: true })));
await sleep(300);
ok((await pD.evaluate(() => !document.querySelector("#map-info")?.hidden)) &&
   (await pD.evaluate(() => document.querySelector("#map-info")?.textContent.includes("backlog-orchestrator"))) &&
   (await pD.evaluate(() => document.querySelector("#map-info")?.textContent.includes("running"))),
  "F9 clic composant → carte info (nom + état running)");
// Tools passifs : états installé/absent rendus + version en tooltip/carte.
ok((await cnt(".n-comp.st-installed")) === 1 && (await cnt(".n-comp.st-absent")) === 1,
  `F9b tools passifs : 1 installed + 1 absent (${await cnt(".n-comp.st-installed")}/${await cnt(".n-comp.st-absent")})`);
ok((await pD.evaluate(() => document.querySelector('[data-comp="agentbacklog"] title')?.textContent.includes("agentbacklog 0.3.0"))),
  "F9c tooltip du tool contient la version sondée");
await pD.evaluate(() => document.querySelector('[data-comp="agentbacklog"]')
  .dispatchEvent(new MouseEvent("click", { bubbles: true })));
await sleep(300);
ok((await pD.evaluate(() => document.querySelector("#map-info")?.textContent.includes("agentbacklog 0.3.0"))),
  "F9d carte info du tool → ligne version");
await pD.evaluate(() => document.querySelector('[data-session="s1"]')
  .dispatchEvent(new MouseEvent("click", { bubbles: true })));
await sleep(600);
ok((await pD.evaluate(() => document.querySelector("#map-view")?.hidden)) === true &&
   (await pD.evaluate(() => document.querySelector("#chat-title")?.textContent.includes("Refactor API client"))) === true,
  "F10 clic session → chat ouvert, vue refermée");
await pD.evaluate(() => document.querySelector("#tb-map")?.click());
await sleep(300);
await pD.evaluate(() => window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" })));
await sleep(200);
ok((await pD.evaluate(() => document.querySelector("#map-view")?.hidden)) === true, "F11 Échap referme la vue");

// ---------- G : panneau composants — états passifs + version des tools ----------
// Même page pD (conn "local", capability components:true). Le panneau
// liste les 4 composants ; les 2 tools affichent leurs états passifs
// traduits, agentbacklog expose sa version sondée dans la méta.
await pD.evaluate(() => document.querySelector("#tb-components")?.click());
await sleep(900);
ok((await pD.evaluate(() => !document.querySelector("#components-view")?.hidden)),
  "G1 #tb-components ouvre le panneau");
ok((await cnt(".comp")) === 4, `G2 4 lignes composants (${await cnt(".comp")})`);
ok((await cnt(".comp-dot.cst-installed")) === 1 && (await cnt(".comp-dot.cst-absent")) === 1,
  `G3 dots passifs : 1 installed + 1 absent (${await cnt(".comp-dot.cst-installed")}/${await cnt(".comp-dot.cst-absent")})`);
const rowMeta = (n) => pD.evaluate((name) =>
  [...document.querySelectorAll(".comp")]
    .find((r) => r.querySelector(".comp-name")?.textContent === name)
    ?.querySelector(".comp-meta")?.textContent ?? null, n);
ok((await rowMeta("agentbacklog"))?.includes("agentbacklog 0.3.0") === true,
  `G4 méta du tool → version sondée (${await rowMeta("agentbacklog")})`);
ok((await pD.evaluate(() =>
    document.querySelector(".comp-dot.cst-installed")?.title === "installed" &&
    document.querySelector(".comp-dot.cst-absent")?.title === "absent")) === true,
  "G5 titres des dots traduits (installed/absent)");
// Tool : pas de cycle de vie ni logs (refusés démon) — seule l'édition reste.
ok((await pD.evaluate(() => {
    const r = [...document.querySelectorAll(".comp")]
      .find((x) => x.querySelector(".comp-name")?.textContent === "agentbacklog");
    return r?.querySelectorAll(".comp-acts button").length === 1;
  })) === true, "G6 tool → une seule action (édition)");
await pD.evaluate(() => window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" })));
await sleep(200);
ok((await pD.evaluate(() => document.querySelector("#components-view")?.hidden)) === true,
  "G7 Échap referme le panneau");
// ---------- H : onglet backlog — file via component_query, refresh borné --
// Même page pD (conn "local", components:true) : le mock sert BACKLOG via
// component_query{name:agentbacklog,query:list} ; __BACKLOG/__blHide/
// __qCalls pilotent mutation, absence du composant et comptage de trafic.
await pD.evaluate(() => document.querySelector("#sv-backlog")?.click());
await sleep(900); // fetch initial + rendu
ok((await pD.evaluate(() =>
  !document.querySelector("#backlog-list")?.hidden &&
  document.querySelector("#session-list")?.hidden === true &&
  document.querySelector("#sv-backlog")?.classList.contains("on"))) === true,
  "H1 onglet Backlog → #backlog-list visible, sessions masquées");
ok((await pD.evaluate(() => document.querySelectorAll("#backlog-list .bl-item").length)) === 4,
  "H2 4 items rendus (wip+pending+done)");
ok((await pD.evaluate(() => document.querySelector("#backlog-list .bl-sec.wip .bl-agent span")?.textContent)) === "Fix login crash",
  "H3 wip : agent_id=s2 résolu en titre de session");
ok((await pD.evaluate(() => document.querySelectorAll("#backlog-list .bl-blocked").length)) === 1,
  "H4 #34 bloquée (dep_status≠done → cadenas)");
ok((await pD.evaluate(() => document.querySelector("#sv-bcount")?.textContent)) === "3",
  "H5 badge onglet = wip+pending (3)");
ok((await pD.evaluate(() => document.querySelectorAll("#backlog-list .bl-agent.off").length)) === 1,
  "H6 done : agent_id inconnu → pastille non cliquable");
// Mutation live : push d'un item → le poll borné le ramène sans reload.
await pD.evaluate(() => window.__BACKLOG.push(
  { id: 40, title: "[test] item live", status: "pending", agent_id: null,
    created_at: new Date().toISOString(), done_at: null, priority: 0 }));
await sleep(4600);
ok((await pD.evaluate(() => document.querySelector("#backlog-list")?.textContent.includes("item live"))) === true,
  "H7 item ajouté → visible via le poll borné, sans reload");
// Clic pastille agent → chat de la session s2.
await pD.evaluate(() => document.querySelector('#backlog-list .bl-sec.wip .bl-agent[data-sid]')?.click());
await sleep(500);
ok((await pD.evaluate(() => document.querySelector("#chat-title")?.textContent.includes("Fix login crash"))) === true,
  "H8 clic pastille agent → chat de la session ouvert");
// Quitter l'onglet coupe le trafic : plus aucun component_query.
await pD.evaluate(() => document.querySelector("#sv-agents")?.click());
const qN = await pD.evaluate(() => window.__qCalls || 0);
await sleep(4700);
ok((await pD.evaluate(() => window.__qCalls || 0)) === qN,
  "H9 onglet quitté → poll arrêté (0 component_query de plus)");
// Composant masqué → état « non déclaré » + CTA ; Intégrer → install → file.
await pD.evaluate(() => { window.__blHide = true; document.querySelector("#sv-backlog")?.click(); });
await sleep(700);
ok((await pD.evaluate(() => document.querySelector("#bl-install") != null)) === true,
  "H10 composant absent → état propre + CTA Intégrer");
await pD.evaluate(() => document.querySelector("#bl-install")?.click());
await sleep(900);
ok((await pD.evaluate(() => document.querySelectorAll("#backlog-list .bl-item").length)) >= 5,
  "H11 Intégrer → component_install → file visible");

console.log(`\n${fails === 0 ? "SMOKE OK" : "SMOKE FAIL"} — ${fails} échec(s)`);
await b.close();
// SIGKILL sur le groupe : vite preview ignore SIGTERM dans certains runs
// (preview orphelin déjà observé après une fin « propre »).
try { process.kill(-server.pid, "SIGKILL"); } catch {}
process.exit(fails ? 1 : 0);
