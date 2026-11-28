// Audit UX Android : couverture d'écrans (shots) + assertions automatiques.
// Étend tests/android-shots.mjs : chaque écran est mesuré (cibles tactiles,
// overflow horizontal, dialogues bornés au viewport, texte < 12px, toasts
// qui recouvrent la titlebar) et les constats sont agrégés dans
// <out>/violations.json + résumé stdout.
//
// Usage : npm run build && node tests/android-audit.mjs [outdir]
//   CHROME_PATH=/chemin/chrome   (défaut : /usr/bin/chromium, sinon cache local)
//   PORT=xxxx                    (port du `vite preview` — auto-détecté sinon)
import puppeteer from "puppeteer-core";
import { mkdirSync, writeFileSync, existsSync, readdirSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { spawn } from "node:child_process";
import { createServer } from "node:net";

const OUT = process.argv[2] || "audit-shots";
mkdirSync(OUT, { recursive: true });

// Port libre détecté (4174+ peut être pris par un autre worktree agent) —
// PORT env pour forcer.
const PORT = process.env.PORT || (await new Promise((res, rej) => {
  const s = createServer();
  s.once("error", rej);
  s.listen(0, () => {
    const p = String(s.address().port);
    s.close(() => res(p));
  });
}));

function findChrome() {
  if (process.env.CHROME_PATH) return process.env.CHROME_PATH;
  if (existsSync("/usr/bin/chromium")) return "/usr/bin/chromium";
  const base = `${process.env.HOME}/.cache/audit-chrome/chrome-headless-shell`;
  try {
    for (const v of readdirSync(base).sort()) {
      const p = `${base}/${v}/chrome-headless-shell-linux64/chrome-headless-shell`;
      if (existsSync(p)) return p;
    }
  } catch { /* cache absent */ }
  throw new Error("Chromium introuvable — CHROME_PATH=… ou /usr/bin/chromium");
}
const CHROME = findChrome();
console.log("Chrome:", CHROME);

// ── Serveur + page ──────────────────────────────────────────────────────────
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], {
  cwd: new URL("..", import.meta.url).pathname, stdio: "ignore",
});
let browser = null;
// Filet de sécurité : un throw en amont ne doit pas laisser le preview
// (port aléatoire) ni chrome orphelins.
process.on("exit", () => {
  try { server.kill(); } catch { /* déjà mort */ }
  try { browser?.process()?.kill(); } catch { /* déjà mort */ }
});
// Attente active : le preview est prêt quand il répond HTTP (plus fiable
// qu'un sleep fixe sur un cold start).
for (let i = 0; i < 60; i++) {
  try {
    const r = await fetch(`http://[::1]:${PORT}/`);
    if (r.ok) break;
  } catch { /* pas encore prêt */ }
  await new Promise((r) => setTimeout(r, 400));
  if (i === 59) throw new Error("vite preview injoignable après 24s");
}

browser = await puppeteer.launch({
  executablePath: CHROME,
  headless: "shell",
  args: ["--no-sandbox", "--disable-dev-shm-usage", "--font-render-hinting=none"],
});
const page = await browser.newPage();
await page.setUserAgent(
  "Mozilla/5.0 (Linux; Android 15; SM-S931B Build/AP3A.240905.015) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36");
const PORTRAIT = { width: 412, height: 915, deviceScaleFactor: 2.625, isMobile: true, hasTouch: true };
await page.setViewport(PORTRAIT);

// Mock Tauri + extension audit : capture des handlers d'events (__emit),
// table d'override RPC (__rpcOver), comptage des history.pushState
// (__pushLog — détecte les doubles empilements de la pile d'overlays).
await page.evaluateOnNewDocument(await readFile(new URL("./tauri-mock.js", import.meta.url), "utf8"));
await page.evaluateOnNewDocument(`
  window.__rpcOver = {};
  window.__evtHandlers = {};
  window.__pushLog = [];
  window.__emit = (event, payload) => {
    const id = window.__evtHandlers[event];
    if (id != null) window.__TAURI_INTERNALS__.runCallback(id, { payload });
    return id != null;
  };
  const __ps = history.pushState.bind(history);
  history.pushState = (s, t, u) => {
    window.__pushLog.push((s && s.ac) || "?");
    return __ps(s, t, u);
  };
  // Trace qui ouvre quels <dialog> (pile d'appel) — debug de séquencement.
  window.__dlgLog = [];
  const __sm = HTMLDialogElement.prototype.showModal;
  HTMLDialogElement.prototype.showModal = function () {
    window.__dlgLog.push({ id: this.id, stack: (new Error().stack || "").slice(0, 500) });
    return __sm.call(this);
  };
  document.addEventListener("click", (e) => {
    const b = e.target && e.target.closest &&
      e.target.closest("#btn-connect, #empty-cta, #btn-new-session");
    if (b) window.__dlgLog.push({ id: "CLICK:" + b.id, stack: "" });
  }, true);
  const __inv = window.__TAURI_INTERNALS__.invoke;
  window.__TAURI_INTERNALS__.invoke = (cmd, args) => {
    if (cmd === "plugin:event|listen" && args && args.event) {
      window.__evtHandlers[args.event] = args.handler;
    }
    if (cmd === "call" && args &&
        Object.prototype.hasOwnProperty.call(window.__rpcOver, args.method)) {
      const o = window.__rpcOver[args.method];
      const params = args.params ?? {};
      return Promise.resolve(typeof o === "function" ? o(params) : o);
    }
    if (cmd === "connect") {
      return Promise.resolve({ version: "0.10.0", sessions: 4, servers: 1,
        terminal: true, first_post: true });
    }
    return __inv(cmd, args);
  };
  // Overrides RPC nécessaires au parcours complet :
  //  - read_file : protocole paginé réel (offset/limit → tranches base64)
  //  - search_events : shape {matches, next_before, done, scanned}
  // daemon_status = map par conn ({local: {caps…}}) — les bools deviennent
  // les capabilities de conns.cap("local") : items Terminal/First-post du ⋮.
  window.__rpcOver.daemon_status = () => ({
    local: { version: "0.10.0", sessions: 4, terminal: true, first_post: true } });
  const __TEXT = "export async function apiCall(url, opts) {\\n" +
    "  return withRetry(() => fetch(url, opts), 3);\\n}\\n\\n" +
    "async function withRetry(fn, n) {\\n  try { return await fn(); }\\n" +
    "  catch (e) { if (n > 0) { await sleep(500); return withRetry(fn, n - 1); } throw e; }\\n}\\n";
  const __MD = "# README\\n\\nTexte **gras** et \`code inline\`.\\n\\n" +
    "- premier point\\n- second point avec [un lien](https://example.com)\\n\\n" +
    "\`\`\`sh\\necho hello && ls -la\\n\`\`\`\\n\\n> Une citation pour la forme.\\n";
  const __PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
  window.__rpcOver.read_file = (p) => {
    // Caveat : le démon réel renvoie la forme paginée seulement si limit est
    // fourni ; sans limit il renvoie la forme legacy {content, truncated}
    // (chemin « Copier le contenu » du fctx, inatteignable ici). Le mock
    // sert toujours la forme paginée — suffisant pour les scénarios joués.
    // daemon_status ignore params.conn : l'appel par-conn renverrait le
    // statut à plat chez le vrai démon ; le mock ne sert que la map fusionnée.
    // path canonique sous le cwd mocké (/home/agent/api) — requis par
    // editable() (fv-edit caché sinon).
    const rel = (p.path || "").split("/").filter(Boolean).join("/");
    const name = rel.split("/").pop() || "";
    let b64;
    if (name === "logo.png") b64 = __PNG;
    else if (name === "README.md") b64 = btoa(unescape(encodeURIComponent(__MD)));
    else b64 = btoa(unescape(encodeURIComponent(__TEXT)));
    const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    const size = bytes.length;
    const off = p.offset || 0;
    const lim = p.limit ? Math.min(p.limit, size - off) : size - off;
    const chunk = bytes.slice(off, off + Math.max(lim, 0));
    let s = "";
    for (const b of chunk) s += String.fromCharCode(b);
    return { path: "/home/agent/api/" + rel, offset: off, size, mtime_ms: 1760000000000,
      data: btoa(s), truncated: off + chunk.length < size };
  };
  window.__rpcOver.search_events = () => ({
    matches: [
      { seq: 3, ts: "2026-10-21T10:02:00Z", type: "tool_call", excerpt: "…apiCall(url) with retry…" },
      { seq: 9, ts: "2026-10-21T10:07:00Z", type: "agent_text", excerpt: "…withRetry encapsule…" },
    ],
    next_before: 2, done: true, scanned: 500,
  });
`);
// ── Helpers ──────────────────────────────────────────────────────────────────
const violations = [];
const V = (screen, kind, detail) => {
  violations.push({ screen, kind, detail });
  console.log(`  ⚠ [${kind}] ${detail}`);
};
let curScreen = "init";
page.on("pageerror", (e) => V(curScreen, "pageerror", e.message));

const shot = async (name, ms = 380) => {
  curScreen = name;
  await new Promise((r) => setTimeout(r, ms));
  await page.screenshot({ path: `${OUT}/${name}.png` });
  const dlgs = await ev(() => [...document.querySelectorAll("dialog[open]")].map((d) => d.id).join(","));
  if (dlgs) console.log("→", name, ` [dialogs: ${dlgs}]`);
  else console.log("→", name);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const tap = async (sel, ms = 320) => {
  for (let i = 0; i < 2; i++) {
    try {
      await page.waitForSelector(sel, { visible: true, timeout: 15000 });
      await page.tap(sel);
      await sleep(ms);
      return;
    } catch (e) {
      if (i === 1) throw e;
      await sleep(500);
    }
  }
};
const longPress = async (sel, ms = 380) => {
  const el = await page.$(sel);
  const box = el && await el.boundingBox();
  if (!box) throw new Error(`longPress: ${sel} absent ou sans box`);
  const x = box.x + box.width / 2, y = box.y + Math.min(box.height / 2, 30);
  const cdp = await page.createCDPSession();
  await cdp.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x, y, id: 1 }] });
  await sleep(650);
  await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
  await sleep(ms);
};
const ev = (fn, ...args) => page.evaluate(fn, ...args);
const emit = (event, payload) => ev((e, p) => window.__emit(e, p), event, payload);
const setRpc = (method, val) => ev(
  (m, vs, isFn) => { window.__rpcOver[m] = isFn ? eval(`(${vs})`) : vs; },
  method, typeof val === "function" ? String(val) : val, typeof val === "function");
const click = (sel) => ev((s) => document.querySelector(s)?.click(), sel);
const visible = (sel) => ev((s) => {
  const el = document.querySelector(s);
  return !!el && !el.hidden && getComputedStyle(el).display !== "none" &&
    el.getBoundingClientRect().width > 0;
}, sel);
const dlgOpen = (id) => ev((i) => !!document.getElementById(i)?.open, id);
// Ferme les <dialog> ouverts via close() — l'event « close » resynchronise
// la pile OverlayNav tout seul (nav.close splice + history.back si top).
const closeDialogs = async () => {
  await ev(() => {
    for (const d of document.querySelectorAll("dialog[open]")) d.close();
    document.getElementById("sctx")?.setAttribute("hidden", "");
    document.getElementById("fctx")?.setAttribute("hidden", "");
  });
  await sleep(200);
};
// Phase 3 : plus de drawer — l'accueil Agents est l'écran racine
// (body.m-home), la session est empilée au-dessus (body.m-in-session).
const onHome = () => ev(() => document.body.classList.contains("m-home"));
// Les toasts recouvrent la titlebar (z 55) — à fermer avant les taps du haut.
const dismissToasts = () =>
  ev(() => document.querySelectorAll(".toast-x").forEach((b) => b.click()));
// goBack borné à la pile d'overlays : les états poussés portent {ac},
// l'entrée racine a state null → ne jamais dépasser (sinon la page sort).
const appBack = async () => {
  if (!(await ev(() => history.state && history.state.ac != null))) return false;
  // history.back() en evaluate : page.goBack() attend une « navigation »
  // qui n'arrive jamais sur les waypoints pushState (timeout 30 s).
  await ev(() => history.back());
  await sleep(320);
  return true;
};
// Dépile overlays + écran session jusqu'à l'accueil. Les entrées « mortes »
// (overlay refermé sans back = waypoint transparent) restent dans
// l'historique : goBack n'en saute qu'une par appel — cap large.
const goHome = async () => {
  for (let i = 0; i < 40 && !(await onHome()); i++) {
    if (!(await appBack())) break;
  }
};
// Ferme un menu ctx bottom-sheet via un clic au-dehors (handler window).
const dismissCtx = () =>
  ev(() => window.dispatchEvent(new MouseEvent("click", { bubbles: false })));

// Assertions d'écran — tout est mesuré dans la page.
async function collect(screen) {
  const res = await ev(() => {
    const out = [];
    const vw = window.innerWidth, vh = window.innerHeight;
    const vis = (el) => {
      const cs = getComputedStyle(el);
      if (cs.display === "none" || cs.visibility === "hidden") return false;
      const r = el.getBoundingClientRect();
      return r.width > 0 && r.height > 0;
    };
    const desc = (el) => {
      const id = el.id ? `#${el.id}` : "";
      const cls = typeof el.className === "string" && el.className.trim()
        ? "." + el.className.trim().split(/\s+/).slice(0, 2).join(".") : "";
      const t = (el.textContent || "").trim().replace(/\s+/g, " ").slice(0, 30);
      return `${el.tagName.toLowerCase()}${id}${cls}«${t}»`;
    };
    // 1. Overflow horizontal global
    const de = document.documentElement;
    if (de.scrollWidth > vw + 1)
      out.push({ kind: "h-overflow", detail: `scrollWidth=${de.scrollWidth} > vw=${vw}` });
    // Éléments débordant à droite (hors conteneurs scroll-x et overlays
    // volontairement hors-champ : drawer fermé, viewer glissé)
    let n = 0;
    for (const el of document.querySelectorAll("body *")) {
      if (n >= 6) break;
      if (!vis(el)) continue;
      if (el.closest("#file-viewer:not(.open)")) continue;
      const r = el.getBoundingClientRect();
      if (r.right < 0) continue; // entièrement hors-champ gauche = off-canvas
      if (r.right > vw + 1 || r.left < -1) {
        let p = el.parentElement;
        let scrollParent = false;
        while (p && p !== document.body) {
          const ox = getComputedStyle(p).overflowX;
          if ((ox === "auto" || ox === "scroll") && p.scrollWidth > p.clientWidth + 1) {
            scrollParent = true; break;
          }
          p = p.parentElement;
        }
        if (!scrollParent) { out.push({ kind: "offscreen", detail: `${desc(el)} right=${Math.round(r.right)}` }); n++; }
      }
    }
    // 2. Cibles tactiles < 44px (hauteur OU largeur pour les éléments
    // compacts ; hauteur seule pour les lignes quasi pleine largeur).
    // Seuil à 43 : tolérance sub-pixel — getBoundingClientRect rend des
    // fractions, un élément théoriquement 44px peut mesurer 43.6px.
    // .mh-seg / .mh-chip exclus : contrôles compacts ~30px dessinés ainsi
    // dans la maquette (segmented control iOS, puces de flotte M3).
    const SEL = "button:not(.mh-seg):not(.mh-chip), select, input, textarea, a[href], [role=tab], .sess, .fnode, .fhit, .froot, .om-item, .ac-item, .pr, .pr-sess, .sv, .sr-row, .q-item button, .perm-opts button, label.s-row, .tool-head, .btn-more, .toast-x, .grp-add, .sess-del, .badge.st-dead, .badge.st-failed";
    const seen = new Set();
    for (const el of document.querySelectorAll(SEL)) {
      if (seen.size >= 25) break;
      if (!vis(el)) continue;
      const r = el.getBoundingClientRect();
      const wFull = r.width > vw * 0.5;
      if (r.height < 43 || (!wFull && r.width < 43)) {
        const k = desc(el);
        if (seen.has(k)) continue;
        seen.add(k);
        out.push({ kind: "target<44", detail: `${k} ${Math.round(r.width)}×${Math.round(r.height)}` });
      }
    }
    // 3. Dialogues bornés au viewport
    for (const d of document.querySelectorAll("dialog[open]")) {
      const r = d.getBoundingClientRect();
      if (r.left < -1 || r.top < -1 || r.right > vw + 1 || r.bottom > vh + 1)
        out.push({ kind: "dialog-bounds", detail: `#${d.id} rect=${Math.round(r.left)},${Math.round(r.top)}→${Math.round(r.right)},${Math.round(r.bottom)}` });
      if (d.scrollWidth > d.clientWidth + 2)
        out.push({ kind: "dialog-hscroll", detail: `#${d.id} scrollWidth=${d.scrollWidth} client=${d.clientWidth}` });
    }
    // 4. Texte < 12px (nœud ayant du texte direct, visible)
    const tiny = new Map();
    for (const el of document.querySelectorAll("body *")) {
      if (tiny.size >= 12) break;
      if (!vis(el)) continue;
      if (![...el.childNodes].some((n) => n.nodeType === 3 && n.textContent.trim())) continue;
      const fs = parseFloat(getComputedStyle(el).fontSize);
      if (fs < 12) {
        const k = el.tagName + "." + (typeof el.className === "string" ? el.className : "");
        if (!tiny.has(k)) tiny.set(k, `${desc(el)} ${fs}px`);
      }
    }
    for (const t of tiny.values()) out.push({ kind: "font<12", detail: t });
    // 5. Toasts recouvrant la titlebar (les ⋮/☰ deviennent intappables)
    const tb = document.getElementById("titlebar")?.getBoundingClientRect();
    if (tb) {
      for (const tst of document.querySelectorAll(".toast")) {
        const r = tst.getBoundingClientRect();
        if (r.top < tb.bottom && r.bottom > tb.top)
          out.push({ kind: "toast-overlap", detail: `toast top=${Math.round(r.top)} chevauche titlebar (bottom=${Math.round(tb.bottom)})` });
      }
    }
    return out;
  });
  for (const r of res) V(screen, r.kind, r.detail);
  return res;
}

const step = async (name, fn) => {
  try { await fn(); } catch (e) {
    console.log(`  ! étape ${name} : ${e.message}`);
    V(name, "script-step", e.message);
  }
};

// ════════════════════════════════════════════════════════════════════════════
await page.goto(`http://[::1]:${PORT}/`, { waitUntil: "networkidle0" });
await shot("a01-boot", 500);
await collect("a01-boot");

// ── Accueil Agents plein écran (racine — plus de drawer) ─────────────────────
await shot("a02-home");
await collect("a02-home");
if (!(await onHome())) V("a02-home", "nav", "body.m-home absent au boot");
if (!(await visible("#side-view")))
  V("a02-home", "nav", "barre de navigation basse absente");
if (!(await visible("#btn-new-session")))
  V("a02-home", "nav", "FAB nouvelle session absent de l'accueil");

// ── Dialogue connexion (via la puce #m-conn → feuille d'actions) ─────────────
await tap("#m-conn", 400);
await ev(() => [...document.querySelectorAll("#omenu .om-item")]
  .find((b) => /connexion|connection/i.test(b.textContent))?.click());
await sleep(400);
await shot("a03-connect");
await collect("a03-connect");
await ev(() => {
  const k = document.querySelector("#c-kind");
  k.value = "ssh"; k.dispatchEvent(new Event("change"));
});
await shot("a03b-connect-ssh", 300);
await collect("a03b-connect-ssh");
await page.type("#c-host", "me@pi5.local");
await tap("#c-go", 900);
await shot("a04-connected");
await collect("a04-connected");

// ── Vues de la barre basse : Repos, Backlog, Diagramme ───────────────────────
await step("a05-repos", async () => {
  await tap("#sv-repos", 500);
  await shot("a05-repos");
  await collect("a05-repos");
});
await step("a05b-backlog", async () => {
  await tap("#sv-backlog", 500);
  await shot("a05b-backlog");
  await collect("a05b-backlog");
});
await step("a05c-map", async () => {
  await tap("#sv-map", 600);
  await shot("a05c-map");
  await collect("a05c-map");
  if (!(await ev(() => document.body.classList.contains("m-map"))))
    V("a05c-map", "nav", "body.m-map absent — barre basse masquée ?");
  if (!(await visible("#side-view")))
    V("a05c-map", "nav", "barre basse non visible par-dessus le diagramme");
  // N5 : pan au doigt (viewBox bouge sans ouvrir de session), double tap
  // = retour à la vue de référence.
  const vb0 = await ev(() => document.getElementById("map-svg")?.getAttribute("viewBox"));
  const box = await ev(() => {
    const r = document.getElementById("map-svg")?.getBoundingClientRect();
    return r ? { x: r.left + r.width / 2, y: r.top + r.height / 2 } : null;
  });
  if (vb0 && box) {
    await page.touchscreen.touchStart(box.x, box.y);
    for (let i = 1; i <= 5; i++)
      await page.touchscreen.touchMove(box.x + i * 12, box.y - i * 8);
    await page.touchscreen.touchEnd();
    await sleep(200);
    const vb1 = await ev(() => document.getElementById("map-svg")?.getAttribute("viewBox"));
    if (vb1 === vb0) V("a05c-map", "nav", "glisser sur le diagramme n'a pas déplacé le viewBox");
    if (await ev(() => document.body.classList.contains("m-in-session")))
      V("a05c-map", "nav", "le glisser a ouvert une session");
    await page.touchscreen.tap(box.x, box.y);
    await sleep(120);
    await page.touchscreen.tap(box.x, box.y);
    await sleep(250);
    const vb2 = await ev(() => document.getElementById("map-svg")?.getAttribute("viewBox"));
    if (vb2 !== vb0) V("a05c-map", "nav", "double tap n'a pas restauré la vue de référence");
  }
  await ev(() => document.getElementById("map-close")?.click());
  await sleep(300);
});
await tap("#sv-agents", 400);

// ── Accueil « par état » (N3) : segmenté, carte À traiter, réponse ───────────
await step("a05d-inbox", async () => {
  if (!(await visible("#m-home-seg")))
    V("a05d-inbox", "nav", "contrôle segmenté État|Groupes absent");
  // L'accueil s'ouvre en mode État (profil neuf, aucun choix mémorisé).
  if (!(await ev(() =>
    document.querySelector('.mh-seg[data-m="state"]')?.classList.contains("on"))))
    V("a05d-inbox", "nav", "mode État non actif par défaut");
  // Indice d'appui long (N4) : posé dès que l'accueil a listé des sessions.
  if (!(await ev(() => { try { return !!localStorage.getItem("ac-hint-longpress"); } catch { return false; } })))
    V("a05d-inbox", "nav", "indice d'appui long jamais affiché");
  await ev(() => document.querySelector('.mh-seg[data-m="state"]')?.click());
  await sleep(600);
  await shot("a05d-inbox");
  await collect("a05d-inbox");
  if (!(await visible("#m-home-state")))
    V("a05d-inbox", "nav", "vue par état non affichée");
  if (await ev(() => !document.getElementById("session-list").hidden))
    V("a05d-inbox", "nav", "arbre encore visible en mode État");
  // Carte « À traiter » de s4 (waiting_input + permission r1).
  if (!(await ev(() => !!document.querySelector("#mh-sec-todo .mh-card"))))
    V("a05d-inbox", "nav", "carte À traiter absente");
  const opts = await ev(() =>
    [...document.querySelectorAll("#mh-sec-todo .mh-card-acts .btn")]
      .map((b) => b.textContent.trim()));
  if (!opts.length) V("a05d-inbox", "nav", "options de la demande absentes");
  // reject_* en premier : « Refuser » doit précéder « Autoriser une fois ».
  if (opts.length && !/refuser|reject/i.test(opts[0]))
    V("a05d-inbox", "nav", `tri des options : 1re = «${opts[0]}» (reject attendu)`);
  // Répondre depuis la carte : allow_once débloque l'agent → au refresh,
  // s4 repart « working » et la section À traiter disparaît.
  await ev(() => [...document.querySelectorAll("#mh-sec-todo .mh-card-acts .btn")]
    .find((b) => /une fois|once/i.test(b.textContent))?.click());
  await sleep(900);
  if (await ev(() => !!document.getElementById("mh-sec-todo")))
    V("a05d-inbox", "nav", "section À traiter toujours visible après réponse");
  // Retour en mode Groupes : l'arbre reparaît.
  await ev(() => document.querySelector('.mh-seg[data-m="group"]')?.click());
  await sleep(400);
  if (await ev(() => document.getElementById("session-list").hidden))
    V("a05d-inbox", "nav", "arbre absent en mode Groupes");
});

// ── Menu contextuel session (long-press) + vérif double-push ─────────────────
await ev(() => { window.__pushLog = []; });
await longPress(".sess");
await shot("a06-sctx");
await collect("a06-sctx");
const pushes = await ev(() => window.__pushLog.filter((x) => x === "sctx").length);
if (pushes !== 1) V("a06-sctx", "ctx-double-push", `nav.push("sctx") ×${pushes} pour un seul long-press`);
await dismissCtx();
await sleep(250);
await ev(() => { if (!document.getElementById("sctx").hidden) document.getElementById("sctx").hidden = true; });

// ── Glisser-gauche (N4) : révèle Endormir / Supprimer, tap dehors ferme ──────
await step("a06b-swipe", async () => {
  const box = await ev(() => {
    const b = document.querySelector(".sess").getBoundingClientRect();
    return { x: b.x + b.width - 20, y: b.y + b.height / 2 };
  });
  await page.touchscreen.touchStart(box.x, box.y);
  for (let i = 1; i <= 8; i++)
    await page.touchscreen.touchMove(box.x - i * 15, box.y);
  await page.touchscreen.touchEnd();
  await sleep(350);
  if (!(await ev(() => !!document.querySelector(".sess .sess-acts"))))
    V("a06b-swipe", "nav", "glisser-gauche n'a pas révélé les actions");
  await shot("a06b-swipe");
  await page.touchscreen.tap(40, 500);
  await sleep(350);
  if (await ev(() => !!document.querySelector(".sess .sess-acts")))
    V("a06b-swipe", "nav", "bande d'actions non refermée au tap extérieur");
  // Un défilement vertical ne doit jamais engager le glisser.
  await page.touchscreen.touchStart(box.x, box.y + 100);
  for (let i = 1; i <= 8; i++)
    await page.touchscreen.touchMove(box.x - i * 2, box.y + 100 - i * 12);
  await page.touchscreen.touchEnd();
  await sleep(250);
  if (await ev(() => !!document.querySelector(".sess .sess-acts")))
    V("a06b-swipe", "nav", "défilement vertical a révélé les actions");
});

// ── Menu contextuel groupe (long-press sur un header de groupe) ──────────────
await step("a07-grp-ctx", async () => {
  if (!await visible(".grp-head")) throw new Error("pas de .grp-head visible");
  await longPress(".grp-head");
  await shot("a07-grp-ctx");
  await collect("a07-grp-ctx");
  await dismissCtx();
  await sleep(250);
  await ev(() => { if (!document.getElementById("sctx").hidden) document.getElementById("sctx").hidden = true; });
});

// ── Session s1 (working + permission + queue) : push « session » ─────────────
await page.evaluate(() => document.querySelector(".sess")?.click());
await sleep(800);
await shot("a08-chat");
await collect("a08-chat");
if (!(await ev(() => document.body.classList.contains("m-in-session"))))
  V("a08-chat", "nav", "m-in-session absent après ouverture de session");
if (await onHome())
  V("a08-chat", "nav", "m-home encore posé en session");
if (!(await visible("#m-sbar-nav")))
  V("a08-chat", "nav", "bouton retour ‹ absent de la barre session");

// ── File d'attente : la puce ouvre la feuille basse ─────────────────────────
await step("a08b-queue-sheet", async () => {
  if (!(await visible("#queue-open"))) throw new Error("#queue-open absent");
  await tap("#queue-open", 450);
  if (!(await visible("#queue-panel"))) throw new Error("#queue-panel non ouvert");
  await shot("a08b-queue-sheet");
  await collect("a08b-queue-sheet");
  // Fermeture par le ✕ (chemin utilisateur complet — le scrim se retire).
  await click("#qp-close");
  await sleep(300);
});

// ── Dock de permission (S3) : la carte r1 vit au-dessus du composer ──────────
await step("a08c-perm-dock", async () => {
  if (!(await visible("#perm-dock"))) throw new Error("#perm-dock absent");
  if (!(await visible("#perm-dock .pd-opts"))) throw new Error("pd-opts absent");
  await shot("a08c-perm-dock");
  await collect("a08c-perm-dock");
  // Le mock annonce « Toujours autoriser » en premier : l'action principale
  // doit rester « Autoriser une fois », placée en dernier (à droite).
  const prim = await ev(() => {
    const bs = [...document.querySelectorAll("#perm-dock .pd-opts button")];
    const p = document.querySelector("#perm-dock .pd-opts .pd-primary");
    return { text: p?.textContent ?? "", last: !!p && bs[bs.length - 1] === p };
  });
  if (!/une fois|once/i.test(prim.text) || !prim.last)
    V("a08c-perm-dock", "nav", `action principale du dock : «${prim.text}» (allow_once à droite attendu)`);
  // Réponse depuis le dock = clic sur le bouton réel de la carte : la
  // carte passe .done et le dock se referme tout seul (observer rAF).
  await click("#perm-dock .pd-opts .btn-allow.pd-primary");
  await sleep(400);
  if (await ev(() => !document.querySelector("#events .ev.perm.done")))
    throw new Error("carte perm non résolue par le dock");
  if (await visible("#perm-dock")) throw new Error("dock encore visible après résolution");
});

// ── Composer pilule (C1) : classe m-input-empty selon le contenu ─────────────
await step("a08d-send-mic", async () => {
  const empty = await ev(() => document.body.classList.contains("m-input-empty"));
  if (!empty) throw new Error("m-input-empty absent avec champ vide");
  await ev(() => {
    const i = document.getElementById("input");
    i.value = "bonjour"; i.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await sleep(300);
  const still = await ev(() => document.body.classList.contains("m-input-empty"));
  if (still) throw new Error("m-input-empty persistant après frappe");
  await ev(() => {
    const i = document.getElementById("input");
    i.value = ""; i.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await sleep(300);
});

// ── Suite d'outils (S4) : seqs 3-4 consécutifs → « +N outils » sur le dernier
await step("a08e-tool-run", async () => {
  await ev(() => document.querySelector("#events .ev.tool.run-last")
    ?.scrollIntoView({ block: "center" }));
  await sleep(300);
  const sum = await ev(() => document.querySelector(".run-sum")?.textContent ?? "");
  if (!sum.trim()) throw new Error("pastille .run-sum absente sur la suite 3-4");
  if (await ev(() => {
    const r = document.querySelector(".ev.tool.run-rest");
    return r && getComputedStyle(r).display !== "none";
  })) throw new Error("membre run-rest visible alors que la suite est repliée");
  await shot("a08e-tool-run");
  await collect("a08e-tool-run");
  // Tap sur la pastille → la suite se déplie (et se replie au tap suivant).
  await ev(() => document.querySelector(".run-sum")?.click());
  await sleep(300);
  if (await ev(() => {
    const r = document.querySelector(".ev.tool.run-rest");
    return !r || getComputedStyle(r).display === "none";
  })) throw new Error("suite non dépliée au tap de la pastille");
  await ev(() => document.querySelector(".run-sum")?.click());
  await sleep(200);
});

// ── Feuille Session (tap sur le titre de la barre) ───────────────────────────
await dismissToasts();
await tap("#m-sbar-title", 400);
await shot("a09-session-sheet");
await collect("a09-session-sheet");
// Section modèle : liste radio des variantes annoncées.
await step("a09b-model", async () => {
  await ev(() => document.querySelector("#dlg-session .ss-radios")
    ?.scrollIntoView({ block: "center" }));
  await sleep(250);
  await shot("a09b-session-model-list");
  await collect("a09b-session-model-list");
});

// ── Recherche dans l'historique (via la feuille Session) ─────────────────────
await step("a10-search", async () => {
  await ev(() => [...document.querySelectorAll("#dlg-session .ss-act")]
    .find((b) => /recherche|search/i.test(b.textContent))?.click());
  await sleep(400);
  await shot("a10-search");
  await collect("a10-search");
  await page.type("#search-input", "retry");
  await sleep(600);
  await shot("a10b-search-results");
  await collect("a10b-search-results");
  // Clic sur un résultat → saut vers l'event
  await ev(() => document.querySelector("#search-results .sr-row")?.click());
  await sleep(600);
  await shot("a10c-search-jump");
  await collect("a10c-search-jump");
  await tap("#search-close", 350);
});

// ── Autocomplete slash ───────────────────────────────────────────────────────
await ev(() => {
  const i = document.querySelector("#input");
  i.value = "/"; i.dispatchEvent(new Event("input", { bubbles: true }));
});
await sleep(350);
await shot("a11-slash-ac");
await collect("a11-slash-ac");
await ev(() => { document.querySelector("#input").value = ""; document.getElementById("cmd-ac").hidden = true; });

// ── Composer : texte multiligne ──────────────────────────────────────────────
await ev(() => {
  const i = document.querySelector("#input");
  i.value = "Ligne 1\nLigne 2\nLigne 3\nLigne 4\nLigne 5\nLigne 6";
  i.dispatchEvent(new Event("input", { bubbles: true }));
});
await shot("a12-multiline", 300);
await collect("a12-multiline");
await ev(() => { document.querySelector("#input").value = ""; });

// ── Events live : append + question_request ──────────────────────────────────
const nEvBefore = await ev(() => document.querySelectorAll("#events .ev").length);
await emit("session-event", { session: "s1", event: { type: "agent_text", seq: 12, ts: "10:10", text: "Réponse en direct — appended live." } });
await emit("session-event", { session: "s1", event: { type: "question_request", request_id: "q1", seq: 13, ts: "10:11", message: "Valider le plan ?" } });
await sleep(400);
const nEvAfter = await ev(() => document.querySelectorAll("#events .ev").length);
if (nEvAfter <= nEvBefore) V("a13-live-append", "live-append", `events ${nEvBefore}→${nEvAfter} non ajoutés`);
await shot("a13-live");
await collect("a13-live");

// ── Explorateur de fichiers (entrée de la feuille Session) ──────────────────
await dismissToasts();
await tap("#m-sbar-title", 350);
await ev(() => [...document.querySelectorAll("#dlg-session .ss-act")]
  .find((b) => /fichier|files|workspace/i.test(b.textContent))?.click());
await sleep(600);
await shot("a14-files");
await collect("a14-files");

// ── Viewer fichier ───────────────────────────────────────────────────────────
await tap(".fnode:not(.is-dir)", 700);
await shot("a15-fileview");
await collect("a15-fileview");
if (await ev(() => !document.querySelector("#file-view")?.textContent.trim() &&
                  document.getElementById("file-edwrap")?.hidden)) {
  V("a15-fileview", "viewer-empty", "contenu du fichier vide après ouverture");
}

// ── Mode édition + dirty guard au back ───────────────────────────────────────
// Le fichier texte non-md s'ouvre DÉJÀ dans l'éditeur (édition directe,
// sans focus → pas de clavier virtuel au tap du nœud). fv-edit = retour
// lecture, pas prérequis pour taper.
await step("a16-fileedit", async () => {
  if (!(await ev(() => !document.getElementById("file-edwrap")?.hidden &&
                    document.getElementById("fv-edit")?.classList.contains("on")))) {
    V("a16-fileedit", "direct-edit", "fichier texte non-md non ouvert en édition directe");
  }
  await shot("a16-fileedit");
  await collect("a16-fileedit");
  await ev(() => {
    const ta = document.querySelector("#file-edit");
    ta.value += "\n// edit";
    ta.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await sleep(250);
  await appBack();
  await sleep(200);
  await shot("a17-dirty-back");
  await collect("a17-dirty-back");
  if (await dlgOpen("dlg-filedirty")) {
    await tap("#fd-discard", 450);
  } else {
    V("a17-dirty-back", "back-guard", "dlg-filedirty non ouvert au back sur buffer sale");
  }
  await sleep(300);
});
if (await ev(() => document.getElementById("file-viewer")?.classList.contains("open"))) {
  await tap("#fv-back", 400);
}

// ── Menu contextuel fichier ──────────────────────────────────────────────────
await step("a18-fctx", async () => {
  await longPress(".fnode.is-dir");
  await shot("a18-fctx");
  await collect("a18-fctx");
  const pushes2 = await ev(() => {
    const n = window.__pushLog.filter((x) => x === "fctx").length;
    window.__pushLog = [];
    return n;
  });
  if (pushes2 !== 1) V("a18-fctx", "ctx-double-push", `nav.push("fctx") ×${pushes2} pour un seul long-press`);
  await dismissCtx();
  await sleep(200);
  await ev(() => { document.getElementById("fctx").hidden = true; });
});

// ── Markdown rendu/source ────────────────────────────────────────────────────
await step("a19-md", async () => {
  await ev(() => [...document.querySelectorAll(".fnode:not(.is-dir)")]
    .find((f) => f.textContent.includes("README"))?.click());
  await sleep(600);
  await shot("a19-md");
  await collect("a19-md");
  if (await visible("#fv-md")) {
    await tap("#fv-md", 450);
    await shot("a19b-md-toggle");
    await collect("a19b-md-toggle");
  } else V("a19-md", "md-toggle", "bouton bascule md absent");
  await tap("#fv-back", 400);
});

// ── Image ────────────────────────────────────────────────────────────────────
await step("a20-image", async () => {
  await ev(() => [...document.querySelectorAll(".fnode:not(.is-dir)")]
    .find((f) => f.textContent.includes("logo.png"))?.click());
  await sleep(500);
  await shot("a20-image");
  await collect("a20-image");
  if (await ev(() => document.getElementById("file-viewer")?.classList.contains("open"))) {
    await tap("#fv-back", 400);
  }
});
await ev(() => document.getElementById("file-close")?.click());
await sleep(350);

// ── Dialogue nouvelle session (FAB de l'accueil) ─────────────────────────────
await dismissToasts();
await goHome();
if (!(await onHome())) V("a21-new-session", "nav", "retour accueil impossible");
await tap("#btn-new-session", 550);
await shot("a21-new-session");
await collect("a21-new-session");
await ev(() => { const d = document.getElementById("dlg-new"); d.scrollTop = d.scrollHeight; });
await shot("a21b-new-scrolled", 250);
await ev(() => document.getElementById("f-cancel")?.click());
await sleep(300);

// ── Dialogue groupe (via ctx session, depuis l'accueil) ──────────────────────
await step("a22-group-dlg", async () => {
  await goHome();
  await sleep(300);
  if (!(await visible(".sess"))) throw new Error("pas de .sess visible");
  for (let i = 0; i < 3 && !(await visible("#sctx")); i++) {
    await longPress(".sess");
    await sleep(350);
  }
  if (!(await visible("#sctx"))) throw new Error("ctx session non ouvert après 3 essais");
  await ev(() => [...document.querySelectorAll("#sctx button")]
    .find((b) => /groupe|group/i.test(b.textContent))?.click());
  await sleep(450);
  await shot("a22-group-dlg");
  await collect("a22-group-dlg");
  if (!(await dlgOpen("dlg-group"))) V("a22-group-dlg", "dialog", "dlg-group non ouvert");
  await ev(() => document.getElementById("g-cancel")?.click());
  await sleep(250);
});

// ── Dialogue reprise (via /resume, en session) ───────────────────────────────
await step("a23-resume", async () => {
  await closeDialogs();
  await goHome();
  await ev(() => document.querySelector(".sess")?.click());
  await sleep(600);
  await ev(() => {
    const i = document.querySelector("#input");
    i.value = "/resume";
    i.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await click("#btn-send");
  await sleep(600);
  await shot("a23-resume");
  await collect("a23-resume");
  await ev(() => document.querySelector("#dlg-resume menu button")?.click());
  await sleep(250);
});

// ── Paramètres (⋮ de l'accueil — la section Application a quitté la
//    feuille Session en phase 3) ─────────────────────────────────────────────
await step("a24-settings", async () => {
  await closeDialogs();
  await dismissToasts();
  await goHome();
  await tap("#tb-more", 400);
  await ev(() => [...document.querySelectorAll("#omenu .om-item")]
    .find((b) => /param|settings/i.test(b.textContent))?.click());
  await sleep(500);
  await shot("a24-settings");
  await collect("a24-settings");
  // Y4 : la vue s'ouvre en liste de sections fermées (lignes 56 px) —
  // référentiel visuel de la spec.
  await shot("a13-settings-list");
  await collect("a13-settings-list");
  // Tap sur la section « Général » (canal + mises à jour) → sous-écran :
  // les autres sections disparaissent, le titre monte dans l'en-tête.
  await tap("#s-sec-general > summary", 500);
  await shot("a13b-settings-section");
  await collect("a13b-settings-section");
  await tap("#s-checkupdate", 800);
  await shot("a24b-settings-update");
  await collect("a24b-settings-update");
  await tap("#settings-close", 350);
});

// ── À propos (⋮ de l'accueil) ────────────────────────────────────────────────
await step("a25-about", async () => {
  await closeDialogs();
  await dismissToasts();
  await goHome();
  await tap("#tb-more", 400);
  await ev(() => [...document.querySelectorAll("#omenu .om-item")]
    .find((b) => /about|propos/i.test(b.textContent))?.click());
  await sleep(400);
  await shot("a25-about");
  await collect("a25-about");
  await ev(() => document.querySelector("#dlg-about menu button")?.click());
  await sleep(250);
});

// ── Dialogue « mise à jour requise » ─────────────────────────────────────────
await step("a26-outdated", async () => {
  await ev(() => {
    document.getElementById("outdated-msg").textContent =
      "Client 0.5.0 < démon 0.6.0 — mettez à jour (0.6.0 dispo).";
    document.getElementById("dlg-outdated").showModal();
  });
  await sleep(350);
  await shot("a26-outdated");
  await collect("a26-outdated");
  await ev(() => document.querySelector("#dlg-outdated menu button")?.click());
  await sleep(250);
});

// ── Panneau terminal (caps injectées) — entrée de la feuille Session ────────
await step("a27-term", async () => {
  await closeDialogs();
  await dismissToasts();
  await goHome();
  await ev(() => document.querySelector(".sess")?.click());
  await sleep(600);
  await tap("#m-sbar-title", 350);
  await ev(() => [...document.querySelectorAll("#dlg-session .ss-act")]
    .find((b) => /terminal/i.test(b.textContent))?.click());
  await sleep(700);
  if (!(await visible("#term-panel")))
    V("a27-term", "script-step", "item Terminal absent du ⋮ ou panneau non ouvert (caps watchdog ?)");
  await shot("a27-term");
  await collect("a27-term");
  // S5 : plein écran + rangée de touches — Ctrl tap = « once » (pastille),
  // re-tap = relâché. La rangée garde le clavier ouvert (pointerdown).
  // Checks gatés sur le panneau : le flake caps ci-dessus a déjà son constat.
  if (await visible("#term-panel")) {
    if (!(await visible("#term-keys")))
      V("a27-term", "term-keys", "rangée de touches absente en plein écran");
    const cst = await ev(() => document.querySelector(".tkey-ctrl")?.dataset.state);
    if (cst !== "off") V("a27-term", "term-keys", `ctrl initial=${cst}`);
    await ev(() => document.querySelector(".tkey-ctrl")?.click());
    await sleep(200);
    const cst2 = await ev(() => document.querySelector(".tkey-ctrl")?.dataset.state);
    if (cst2 !== "once") V("a27-term", "term-keys", `ctrl après tap=${cst2}`);
    await ev(() => document.querySelector(".tkey-ctrl")?.click());
  }
  await shot("a12-terminal");
  await ev(() => document.getElementById("btn-term-hide")?.click());
  await sleep(300);
});

// ── Events push : status_changed + session_deleted (toast + liste) ───────────
await emit("session-event", { session: "s2", event: { type: "status_changed", state: "working" } });
await emit("session-event", { session: "s3", event: { type: "session_deleted" } });
await sleep(600);
await shot("a28-toast-refresh");
await collect("a28-toast-refresh");
if (!(await ev(() => document.querySelectorAll(".toast").length > 0)))
  V("a28-toast-refresh", "toast", "aucun toast après session_deleted");
await ev(async () => {
  for (let i = 0; i < 15; i++) {
    window.__emit("session-event", { session: "s2", event: { type: "status_changed", state: i % 2 ? "working" : "idle" } });
    await new Promise((r) => setTimeout(r, 30));
  }
});
await sleep(400);
await collect("a28b-rafale");

// ── Back-stack : ctx session sur l'accueil → back ferme sctx seul ;
//    puis entrée en session → back ramène à l'accueil (N2) ───────────────────
await step("a29-backstack", async () => {
  await closeDialogs();
  await dismissToasts();
  await goHome();
  await sleep(300);
  for (let i = 0; i < 3 && !(await visible("#sctx")); i++) {
    await longPress(".sess");
    await sleep(350);
  }
  if (!(await visible("#sctx"))) throw new Error("sctx non ouvert — setup impossible");
  await shot("a29-stack");
  await appBack();
  if (await visible("#sctx"))
    V("a29-backstack", "back-stack", "back n'a pas fermé sctx");
  if (!(await onHome()))
    V("a29-backstack", "back-stack", "le back sctx a quitté l'accueil");
  // « session » empilé : le ‹ de la barre session = history.back().
  await ev(() => document.querySelector(".sess")?.click());
  await sleep(700);
  if (!(await ev(() => document.body.classList.contains("m-in-session"))))
    throw new Error("m-in-session absent — session non empilée");
  await tap("#m-sbar-nav", 500);
  if (!(await onHome()))
    V("a29-backstack", "back-stack", "‹ session n'a pas ramené à l'accueil");
  await shot("a29b-after-backs");
});

// ── Grande liste de sessions (40) — perf + overflow ─────────────────────────
await setRpc("list_sessions", () => {
  const agents = ["devin", "mock", "claude"];
  const out = [];
  for (let i = 0; i < 40; i++) {
    out.push({
      id: `sx${i}`, conn: "local", agent: agents[i % 3],
      title: `Session de travail numéro ${i} — refonte complète du module de synchronisation`,
      cwd: `/home/agent/proj${i}`, created_at: "2026-10-20T10:00:00Z",
      group: i % 3 ? `Groupe ${i % 4}` : undefined,
      available_modes: [], available_models: [], available_commands: [],
      state: ["idle", "working", "dead", "compacting"][i % 4],
    });
  }
  return out;
});
await step("a31-many", async () => {
  await closeDialogs();
  await dismissToasts();
  // Déclenche sidebar.refresh() → list_sessions re-fetch.
  await emit("session-event", { session: "s1", event: { type: "status_changed", state: "working" } });
  await sleep(500);
  await goHome();
  await sleep(600);
  await shot("a31-many-sessions");
  await collect("a31-many-sessions");
  const scrollMs = await ev(async () => {
    const list = document.getElementById("session-list");
    const t0 = performance.now();
    for (let i = 0; i < 20; i++) {
      list.scrollTop = (list.scrollHeight / 20) * i;
      await new Promise((r) => requestAnimationFrame(r));
    }
    return Math.round(performance.now() - t0);
  });
  console.log(`  scroll 20 frames : ${scrollMs}ms (${(scrollMs / 20).toFixed(1)}ms/frame)`);
  if (scrollMs / 20 > 32) V("a31-many", "scroll-perf", `${(scrollMs / 20).toFixed(1)}ms/frame > 32ms`);
  await page.screenshot({ path: `${OUT}/a31b-scrolled.png` });
  await tap("#m-search", 250);
  await page.type("#sess-filter-input", "Groupe 2");
  await sleep(400);
  await shot("a31c-filtered");
  await collect("a31c-filtered");
});

// ── Thème clair (un écran témoin) ────────────────────────────────────────────
await step("a32-light", async () => {
  await ev(() => {
    const s = document.querySelector("#s-theme");
    s.value = "light"; s.dispatchEvent(new Event("change"));
  });
  await sleep(400);
  await shot("a32-light");
  await ev(() => {
    const s = document.querySelector("#s-theme");
    s.value = "dark"; s.dispatchEvent(new Event("change"));
  });
});

// ── Résumé ───────────────────────────────────────────────────────────────────
writeFileSync(`${OUT}/violations.json`, JSON.stringify(violations, null, 2));
// Trace des ouvertures de dialogues (debug séquencement).
const dlgLog = await ev(() => window.__dlgLog || []);
writeFileSync(`${OUT}/dlg-log.json`, JSON.stringify(dlgLog, null, 2));
const byKind = {};
for (const v of violations) (byKind[v.kind] ??= []).push(v);
console.log("\n═══ Résumé ═══");
for (const [k, arr] of Object.entries(byKind)) console.log(`${k}: ${arr.length}`);
console.log(`Total: ${violations.length} constats → ${OUT}/violations.json`);

await browser.close();
server.kill();
