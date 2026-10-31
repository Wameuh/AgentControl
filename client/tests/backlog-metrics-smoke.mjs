// Smoke DOM — onglet Backlog, section « Activité » (RPC backlog_metrics) :
// samples présents → canvas réellement tracé + cartes d'agrégats ;
// RPC inconnue → section masquée sans erreur brute ; journal vide →
// placeholder discret + cartes conservées. Usage :
//   node tests/backlog-metrics-smoke.mjs   (après npm run build)
import puppeteer from "puppeteer-core";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";

const EXE = process.env.CHROME_BIN ?? "/usr/bin/chromium";
// detached:true → kill(-pid) tue tout le groupe ; port éphère lu sur la
// sortie vite (un port fixe squatté piloterait le build d'un autre
// worktree — faux négatifs déjà observés).
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
for (let i = 0; i < 40; i++) {
  try {
    const r = await fetch(BASE + "/");
    if (r.ok) break;
  } catch {}
  await new Promise((r) => setTimeout(r, 250));
}
const b = await puppeteer.launch({ executablePath: EXE, headless: true, args: ["--no-sandbox"] });
const mock = readFileSync("tests/tauri-mock.js", "utf8");

let failed = 0;
const ok = (cond, name) => { console.log(cond ? "ok  " : "FAIL", name); if (!cond) failed++; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Conn « local » liée au boot : sans saved_connection la cap
// `components` reste fausse et la file affiche l'état « démon vieux »
// au lieu des items — le graphe seul ne suffit pas à tout vérifier.
const PATCH_CONN = `(() => {
  const inv = window.__TAURI_INTERNALS__.invoke;
  window.__TAURI_INTERNALS__.invoke = (cmd, args) =>
    cmd === "saved_connections" ? Promise.resolve([{ kind: "local" }]) : inv(cmd, args);
})();`;

/** Page avec le mock (+ script d'amorce) puis onglet Backlog ouvert. */
const open = async (pre) => {
  const p = await b.newPage();
  await p.evaluateOnNewDocument(mock + "\n" + PATCH_CONN + "\n" + (pre ?? ""));
  await p.goto(BASE + "/", { waitUntil: "networkidle0" });
  await p.evaluate(() => document.querySelector("#sv-backlog")?.click());
  return p;
};

// --- 1) Série présente : canvas tracé + 4 cartes alimentées par les
//        agrégats RPC (pas de recalcul client — les valeurs mockées
//        doivent transparaître). --------------------------------------
const p1 = await open();
try {
  await p1.waitForSelector("#bl-mcanvas", { timeout: 9000 });
  ok(true, "samples → canvas présent");
  const painted = await p1.evaluate(() => {
    const c = document.querySelector("#bl-mcanvas");
    const d = c.getContext("2d").getImageData(0, 0, c.width, c.height).data;
    for (let i = 3; i < d.length; i += 4) if (d[i] > 0) return true;
    return false;
  });
  ok(painted, "canvas réellement tracé (pixels non vides)");
  const cards = await p1.evaluate(() =>
    [...document.querySelectorAll(".bl-m-card .bl-m-v")].map((x) => x.textContent.trim()));
  ok(cards.length === 4, "4 cartes stats rendues");
  // lead_time_avg_s = 60300 s → « 16,8 h » ; done_per_day 0.86 → « 0.9 » ;
  // workers_peak 3 → « 3 » — signe que les agrégats RPC traversent.
  ok(cards[0].endsWith("h") && cards[3] === "3",
    `agrégats RPC affichés (${cards.join(" | ")})`);
  const legend = await p1.evaluate(() => document.querySelectorAll(".bl-m-legend span").length);
  ok(legend === 3, "légende 3 séries (pending/wip/workers)");
} catch (e) {
  ok(false, `scénario samples: ${e.message}`);
}
await p1.close();

// --- 2) RPC inconnue (vieux démon) : section absente du DOM, aucune
//        erreur brute dans le panneau. --------------------------------
const p2 = await open(`window.__blMetrics = "off";`);
// La file charge son propre cycle — attendre son rendu plutôt qu'un
// sleep fixe (le fetch métriques échoue en parallèle, plus rapide).
await p2.waitForSelector("#backlog-list .bl-item", { timeout: 9000 });
await sleep(800); // le rejet RPC masque la section au re-render suivant
ok(await p2.evaluate(() => !document.querySelector("#bl-m-tgl")),
  "RPC inconnue → section masquée");
ok(await p2.evaluate(() => !document.querySelector("#backlog-list .warn")),
  "pas d'erreur brute affichée");
// La file elle-même reste fonctionnelle.
ok(await p2.evaluate(() => document.querySelectorAll("#backlog-list .bl-item").length > 0),
  "la file reste affichée");
await p2.close();

// --- 3) Journal vide (orch off) : placeholder discret + cartes. ------
const p3 = await open(`window.__blMetrics = [];`);
try {
  await p3.waitForSelector(".bl-metrics .sess-none", { timeout: 9000 });
  ok(true, "journal vide → placeholder affiché");
  ok(await p3.evaluate(() => !document.querySelector("#bl-mcanvas")),
    "pas de canvas sans samples");
  ok(await p3.evaluate(() => document.querySelectorAll(".bl-m-card").length === 4),
    "cartes conservées (agrégats items exploitables)");
} catch (e) {
  ok(false, `scénario journal vide: ${e.message}`);
}
await p3.close();

await b.close();
try { process.kill(-server.pid); } catch { server.kill(); }
console.log(failed ? `${failed} FAIL` : "all ok");
process.exit(failed ? 1 : 0);
