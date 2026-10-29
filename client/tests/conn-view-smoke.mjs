// Smoke DOM — vue connexion (#32) : l'écran de connexion n'est plus une
// modale mais une vue dans #chat (pattern settings-view).
//   A. Aucune connexion enregistrée → la vue s'ouvre seule en accueil.
//   B. Échap ferme → empty-state + CTA ; le CTA rouvre.
//   C. Nouvelle connexion via le formulaire → connect + save → retour
//      à la conversation (vue fermée, sessions listées).
//   D. La saved conn apparaît en liste : pastille, statut live, actions.
//   E. Éditer → form prérempli ; reset → mode nouvelle connexion.
//   F. Supprimer (2 taps) → remove_connection + ligne retirée.
//   G. Conn sauvegardée mais auto-connect off → accueil auto sur la vue,
//      « connecter ici » depuis la ligne.
//   H. Android : back système (history.back) ferme la vue.
//   I. Exclusion mutuelle settings ↔ conn-view : l'inert de l'overlay
//      encore ouvert n'est jamais stripé par le close de l'autre.
//   J. Clic sur une session de la sidebar pendant que la vue est
//      ouverte → la vue se ferme (comme le panneau settings).
// Usage : node tests/conn-view-smoke.mjs   (après npm run build)
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";

const EXE = process.env.CHROME_BIN ?? "/usr/bin/chromium";
const PORT = process.env.PORT ?? "4210";
// detached:true → kill(-pid) tue tout le groupe (npx → sh → vite) : sinon le
// preview survit orphelin après la fin du test.
const server = spawn("npx", ["vite", "preview", "--port", PORT, "--strictPort"], { stdio: "ignore", detached: true });
await new Promise((r) => setTimeout(r, 1800));
const b = await puppeteer.launch({ executablePath: EXE, headless: true, args: ["--no-sandbox"] });
const mock = readFileSync("tests/tauri-mock.js", "utf8");

// Saved conns mutables + save/remove effectifs, appels enregistrés.
const PATCH = `(() => {
  const inv = window.__TAURI_INTERNALS__.invoke;
  const keyOf = (c) => !c ? "" : c.kind === "ssh" ? \`\${c.native ? "ssh-native" : "ssh"}:\${c.host}:\${c.port ?? 22}\`
    : c.kind === "tcp" ? \`tcp:\${c.addr}\` : c.kind === "web" ? "web" : "local";
  window.__saved = window.__saved ?? []; window.__calls = { save: [], remove: [], connect: [] };
  window.__TAURI_INTERNALS__.invoke = (cmd, args) => {
    if (cmd === "saved_connections") return Promise.resolve(window.__saved);
    if (cmd === "cli_conn") return Promise.resolve(null);
    if (cmd === "save_connection") {
      window.__calls.save.push(args?.cfg);
      window.__saved = [args.cfg, ...window.__saved.filter((c) => keyOf(c) !== keyOf(args.cfg))];
      return Promise.resolve(null);
    }
    if (cmd === "remove_connection") {
      window.__calls.remove.push(args?.cfg);
      window.__saved = window.__saved.filter((c) => keyOf(c) !== keyOf(args?.cfg));
      return Promise.resolve(null);
    }
    if (cmd === "connect") { window.__calls.connect.push(args?.cfg); }
    return inv(cmd, args);
  };
})();`;

const initPage = async (saved, hash = "#desktop", extra = "") => {
  const p = await b.newPage();
  await p.setViewport({ width: 1280, height: 860 });
  p.on("pageerror", (e) => console.log("[pageerror]", e.message));
  await p.evaluateOnNewDocument(mock);
  await p.evaluateOnNewDocument(PATCH);
  await p.evaluateOnNewDocument(`window.__saved = ${JSON.stringify(saved)};` + extra);
  await p.goto(`http://localhost:${PORT}/${hash}`, { waitUntil: "domcontentloaded" });
  await p.waitForSelector("#btn-connect", { timeout: 15000 });
  await sleep(1200);
  return p;
};

let fails = 0;
const ok = (c, n) => { console.log((c ? "  PASS " : "  FAIL ") + n); if (!c) fails++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const viewOpen = (p) => p.evaluate(() => !document.getElementById("conn-view").hidden);
const rows = (p) => p.evaluate(() =>
  [...document.querySelectorAll("#cv-saved .cv-conn")].map((r) => r.textContent.trim()));

// ---------- A/B : aucune conn → accueil auto ; Échap + CTA ----------
const pA = await initPage([]);
ok(await viewOpen(pA), "A1 vue connexion auto-ouverte (aucune conn enregistrée)");
ok(!(await pA.evaluate(() => !!document.querySelector("dialog[open]"))),
  "A2 aucun <dialog> ouvert (l'écran de connexion n'est plus modal)");
ok(!(await pA.evaluate(() => !!document.getElementById("dlg-conn"))),
  "A3 dlg-conn supprimé du DOM");

await pA.keyboard.press("Escape");
await sleep(400);
ok(!(await viewOpen(pA)), "B1 Échap ferme la vue → retour à la conversation");
ok(!(await pA.evaluate(() => document.getElementById("empty-state").hidden)),
  "B2 empty-state visible derrière (pas de gel)");
ok(!(await pA.evaluate(() => document.getElementById("empty-cta").hidden)),
  "B3 CTA « nouvelle connexion » proposé");
await pA.evaluate(() => document.getElementById("empty-cta").click());
await sleep(400);
ok(await viewOpen(pA), "B4 le CTA rouvre la vue (pas de modal)");

// ---------- C : nouvelle connexion → retour conversation ----------
await pA.evaluate(() => {
  document.getElementById("c-kind").value = "local";
  document.getElementById("c-kind").dispatchEvent(new Event("change"));
});
await pA.evaluate(() => document.getElementById("c-go").click());
await sleep(900);
ok(!(await viewOpen(pA)), "C1 connexion réussie → vue fermée (retour discussion)");
ok((await pA.evaluate(() => window.__calls.save.length)) === 1,
  "C2 connexion sauvegardée");
ok((await pA.evaluate(() => document.querySelectorAll("#session-list .sess").length)) === 4,
  "C3 sessions de la conn listées");
ok(!(await pA.evaluate(() => document.getElementById("empty-state").hidden))
  === false, "C4 empty-state refermé");

// ---------- D : ligne saved conn (pastille, statut, actions) ----------
await pA.evaluate(() => document.getElementById("btn-connect").click());
await sleep(400);
const rA = await rows(pA);
ok(rA.length === 1 && /local/.test(rA[0]), `D1 saved conn listée (${JSON.stringify(rA)})`);
ok((await pA.evaluate(() => document.querySelectorAll("#cv-saved .cv-dot").length)) === 1,
  "D2 pastille couleur présente");
ok((await pA.evaluate(() => document.querySelectorAll("#cv-saved .cv-st-ok").length)) === 1,
  "D3 statut live « ok » sur la conn bound");
ok((await pA.evaluate(() => document.querySelectorAll("#cv-saved .cv-new").length)) === 1,
  "D4 action « nouvelle instance » (desktop)");
ok((await pA.evaluate(() => document.querySelectorAll("#cv-saved .cv-edit").length)) === 1,
  "D5 action « éditer »");
ok((await pA.evaluate(() => document.querySelectorAll("#cv-saved .cv-btns button svg").length)) === 4,
  "D6 icônes inline dans les 4 boutons d'action");

// ---------- E : éditer / reset ----------
await pA.evaluate(() => document.querySelector("#cv-saved .cv-edit").click());
await sleep(300);
ok((await pA.evaluate(() => document.getElementById("c-kind").value)) === "local",
  "E1 éditer → form prérempli (kind=local)");
ok(!(await pA.evaluate(() => document.getElementById("cv-form-reset").hidden)),
  "E2 bouton reset visible en édition");
await pA.evaluate(() => document.getElementById("cv-form-reset").click());
await sleep(200);
ok((await pA.evaluate(() => document.getElementById("c-kind").value)) === "ssh",
  "E3 reset → retour au form nouvelle connexion");

// ---------- F : supprimer (2 taps) ----------
await pA.evaluate(() => document.querySelector("#cv-saved .cv-del").click());
await sleep(200);
ok((await rows(pA)).length === 1, "F1 1er tap : armé, ligne toujours là");
await pA.evaluate(() => document.querySelector("#cv-saved .cv-del").click());
await sleep(400);
ok((await rows(pA)).length === 0, "F2 2e tap : conn supprimée de la liste");
ok((await pA.evaluate(() => window.__calls.remove.length)) === 1,
  "F3 remove_connection invoqué");
ok(await pA.evaluate(() => document.getElementById("cv-saved-sec").hidden),
  "F4 section liste masquée quand vide");

// ---------- G : saved + autoconnect off → accueil auto + « connecter ici »
const SAVED_G = [{ kind: "tcp", addr: "10.0.0.5:7331", color: "gold" }];
const pG = await initPage(SAVED_G, "#desktop",
  `localStorage.setItem("ac-autoconnect", "0");`);
await sleep(300);
ok(await viewOpen(pG), "G1 autoconnect off → vue accueil auto-ouverte");
ok((await pG.evaluate(() => document.querySelectorAll("#cv-saved .cv-conn").length)) === 1,
  "G2 saved conn listée");
await pG.evaluate(() => document.querySelector("#cv-saved .cv-go").click());
await sleep(900);
ok(!(await viewOpen(pG)), "G3 « connecter ici » → retour conversation");
ok((await pG.evaluate(() => window.__calls.connect.at(-1)?.addr)) === "10.0.0.5:7331",
  "G4 connect invoqué avec la cfg de la ligne");

// ---------- H : Android — back système ferme la vue ----------
const pH = await initPage([], "");
await sleep(300);
ok(await viewOpen(pH), "H1 android : vue accueil auto-ouverte");
await pH.evaluate(() => history.back());
await sleep(500);
ok(!(await viewOpen(pH)), "H2 back système ferme la vue (nav overlay)");
await pH.evaluate(() => document.getElementById("btn-connect").click());
await sleep(300);
await pH.evaluate(() => history.back());
await sleep(500);
ok(!(await viewOpen(pH)), "H3 back ferme aussi une ouverture manuelle");

// ---------- I : exclusion mutuelle — inert de l'overlay survivant ----------
const pI = await initPage([], "#desktop");
await sleep(300);
ok(await viewOpen(pI), "I1 conn-view auto-ouverte (aucune conn)");
await pI.evaluate(() => document.getElementById("tb-settings").click());
await sleep(300);
ok(!(await viewOpen(pI)) &&
  !(await pI.evaluate(() => document.getElementById("settings-view").hidden)),
  "I2 settings ouvert par-dessus → conn-view fermée");
// (l'inert est posé sur les enfants directs de #chat — #composer parent
// de #input — la propriété de #input elle-même reste toujours fausse)
ok(await pI.evaluate(() => document.getElementById("composer").inert),
  "I3 composer reste inerte sous settings (close conn-view ne stripe pas)");
await pI.evaluate(() => document.getElementById("settings-close").click());
await sleep(300);
ok(!(await pI.evaluate(() => document.getElementById("composer").inert)),
  "I4 settings fermé → inert levé, champ réutilisable");

// ---------- J : clic session sidebar pendant que la vue est ouverte ----------
// (localStorage est partagé entre pages du même navigateur/origine —
//  la section G a posé ac-autoconnect=0 → le retirer pour que
//  l'autoconnect de cette page bind bien la conn local)
const pJ = await initPage([{ kind: "local" }], "#desktop",
  `localStorage.removeItem("ac-autoconnect");`);
await sleep(300);
await pJ.evaluate(() => document.getElementById("btn-connect").click());
await sleep(400);
ok(await viewOpen(pJ), "J1 vue ouverte par-dessus une conn bound");
await pJ.evaluate(() => document.querySelector("#session-list .sess").click());
await sleep(500);
ok(!(await viewOpen(pJ)), "J2 clic session → conn-view fermée (comme settings)");
ok(!(await pJ.evaluate(() => document.getElementById("composer").inert)),
  "J3 #input réutilisable après ouverture de la session");

console.log(`\n${fails === 0 ? "SMOKE OK" : "SMOKE FAIL"} — ${fails} échec(s)`);
await b.close();
// SIGKILL sur le groupe : vite preview ignore SIGTERM dans certains runs
// (preview orphelin déjà observé après une fin « propre »).
try { process.kill(-server.pid, "SIGKILL"); } catch {}
process.exit(fails ? 1 : 0);
