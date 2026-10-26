import "./styles.css";
import "./mesh-bg.ts";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getVersion } from "@tauri-apps/api/app";
import { save } from "@tauri-apps/plugin-dialog";
import { appDataDir, downloadDir } from "@tauri-apps/api/path";
import { gemSVG } from "./crystal";
import { initLang, setLang, getLang, t, applyStatic, type Lang } from "./i18n";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

// Thème + langue persistés (localStorage), appliqués au boot
initLang();
const mesh = (window as { MeshBackground?: { setMood: (m: string, o: object) => void } })
  .MeshBackground;
const bgMood = () => (document.documentElement.classList.contains("light") ? "paper" : "sand");

function applyTheme(name: "dark" | "light") {
  document.documentElement.classList.toggle("light", name === "light");
  localStorage.setItem("ac-theme", name);
  mesh?.setMood(bgMood(), { intensity: 0.55 });
}
applyTheme(localStorage.getItem("ac-theme") === "light" ? "light" : "dark");
applyStatic();

// Gemme cristal : verte (neutral) = identité app ; dorée (human) = travail
for (const id of ["titlebar-gem", "about-gem"]) {
  const el = document.getElementById(id);
  if (el) el.innerHTML = gemSVG("neutral");
}
const cg = document.getElementById("chat-gem");
if (cg) cg.innerHTML = gemSVG("human");

// Plateforme (android | windows | linux) — gates UI mobile.
// Détection synchrone par UA (avant le resolve de platform()).
let IS_ANDROID = /Android/i.test(navigator.userAgent);
function applyAndroidChrome() {
  // Pas de fenêtre à minimizer/drag sous Android : ☰ + ⋮ seulement.
  $("tb-menu").hidden = false;
  $("tb-more").hidden = false;
  $("chat-more").hidden = false;
  $("titlebar").removeAttribute("data-tauri-drag-region");
  ($("input") as HTMLTextAreaElement).enterKeyHint = "send";
  ($("input") as HTMLTextAreaElement).placeholder = t("input_ph_mobile");
  $("file-close").textContent = "‹"; // page fichiers → flèche retour
}
invoke<string>("platform").then((p) => {
  IS_ANDROID = p === "android";
  document.body.classList.toggle("android", IS_ANDROID);
  if (IS_ANDROID) applyAndroidChrome();
}).catch(() => {});
document.body.classList.toggle("android", IS_ANDROID);
if (IS_ANDROID) applyAndroidChrome();

/// Ouvre une URL externe (plugin opener — navigateur système sur Android).
function shellOpen(url: string) {
  invoke("plugin:opener|open_url", { url }).catch(() => {});
}

/// Clic droit (desktop) ou appui long ~500 ms (tactile) → menu contextuel.
function bindCtx(el: HTMLElement, cb: (x: number, y: number) => void) {
  el.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    e.stopPropagation();
    cb(e.clientX, e.clientY);
  });
  let timer: ReturnType<typeof setTimeout> | null = null;
  let px = 0;
  let py = 0;
  // Le touchend qui termine un long-press synthétise un click : l'avaler pour
  // ne pas déclencher l'action normale de l'élément ni refermer le menu.
  const swallowClick = (e: Event) => { e.preventDefault(); e.stopPropagation(); };
  el.addEventListener("touchstart", (e) => {
    const t0 = e.touches[0];
    px = t0.clientX;
    py = t0.clientY;
    timer = setTimeout(() => {
      timer = null;
      el.addEventListener("click", swallowClick, { capture: true, once: true });
      setTimeout(() => el.removeEventListener("click", swallowClick, { capture: true }), 450);
      cb(px, py);
    }, 500);
  }, { passive: true });
  const cancel = () => {
    if (timer) { clearTimeout(timer); timer = null; }
  };
  el.addEventListener("touchend", cancel);
  el.addEventListener("touchmove", (e) => {
    const t0 = e.touches[0];
    if (Math.hypot(t0.clientX - px, t0.clientY - py) > 10) cancel();
  }, { passive: true });
}

// Fenêtre custom (decorations: false) : drag natif via data-tauri-drag-region
const appWin = getCurrentWindow();
$("tb-min").onclick = () => appWin.minimize();
$("tb-max").onclick = () => appWin.toggleMaximize();
$("tb-close").onclick = () => appWin.close();

// dragDropEnabled:false → WebView2 garde AllowExternalDrop=true : un
// fichier OS droppé sur la fenêtre ferait naviguer la webview vers
// file:///… — on avale les drops de fichiers au niveau window (les
// cibles internes ont déjà traité les leurs en amont ; restreint à
// "Files" pour garder curseur + drop de texte natif intacts).
const isFileDrag = (e: DragEvent) => e.dataTransfer?.types.includes("Files") ?? false;
window.addEventListener("dragover", (e) => { if (isFileDrag(e)) e.preventDefault(); });
window.addEventListener("drop", (e) => { if (isFileDrag(e)) e.preventDefault(); });

// ---------------------------------------------------------------------------
// Navigation Android : pile d'overlays adossée à l'historique → le bouton
// "retour" ferme le panneau/menu/dialog ouvert au lieu de quitter l'app.
// ---------------------------------------------------------------------------
const navStack: string[] = [];
let suppressPops = 0; // back() déclenchés par navClose → autant de popstate à ignorer
function navPush(id: string) {
  if (!IS_ANDROID) return;
  navStack.push(id);
  try { history.pushState({ ac: id }, ""); } catch { /* noop */ }
}
function navClose(id: string) {
  if (!IS_ANDROID) return;
  const i = navStack.lastIndexOf(id);
  if (i < 0) return;
  navStack.splice(i, 1);
  if ((history.state as { ac?: string } | null)?.ac === id) {
    suppressPops++;
    history.back();
  }
}
function rawCloseOverlay(id: string) {
  if (id === "drawer") {
    $("sidebar").classList.remove("open");
    scrimOff("drawer");
  } else if (id === "sheet" || id === "fctx" || id === "sctx") {
    omenu.hidden = true;
    fileCtx.hidden = true;
    sessCtx.hidden = true;
    scrimOff("sheet");
  } else if (id === "fileview") {
    $("file-viewer").classList.remove("open");
  } else if (id === "files") {
    $("file-panel").hidden = true;
  } else if (id.startsWith("dlg:")) {
    (document.getElementById(id.slice(4)) as HTMLDialogElement | null)?.close();
  }
}
window.addEventListener("popstate", () => {
  if (!IS_ANDROID) return;
  if (suppressPops > 0) { suppressPops--; return; }
  const top = navStack.pop();
  if (top) rawCloseOverlay(top);
});
// Dialogs trackés automatiquement dans la pile (Cancel = back propre).
if (IS_ANDROID) {
  const origShowModal = HTMLDialogElement.prototype.showModal;
  HTMLDialogElement.prototype.showModal = function (this: HTMLDialogElement) {
    origShowModal.call(this);
    navPush(`dlg:${this.id}`);
  };
  document.addEventListener("close", (e) => {
    const d = e.target as HTMLDialogElement;
    if (d?.id) navClose(`dlg:${d.id}`);
  }, true);
}

// Drawer mobile : ☰ ouvre la sidebar, tap dehors/selection ferme.
const scrim = document.createElement("div");
scrim.id = "scrim";
scrim.hidden = true;
let scrimOwner: "drawer" | "sheet" | null = null;
function scrimOn(owner: "drawer" | "sheet") {
  scrimOwner = owner;
  scrim.hidden = false;
}
function scrimOff(owner: "drawer" | "sheet") {
  if (scrimOwner === owner) { scrimOwner = null; scrim.hidden = true; }
}
scrim.onclick = () => {
  if (scrimOwner === "drawer") { navClose("drawer"); $("sidebar").classList.remove("open"); scrimOff("drawer"); }
  else if (scrimOwner === "sheet") { navClose("sheet"); omenu.hidden = true; scrimOff("sheet"); }
};
// NB : dans #app-body (pas body) — #app a z-index:1 (contexte d'empilement),
// un enfant de body passerait au-dessus de toute l'app quel que soit son z.
$("app-body").appendChild(scrim);
// Menu ⋮ en bottom-sheet (Android uniquement)
const omenu = document.createElement("div");
omenu.id = "omenu";
omenu.hidden = true;
$("app-body").appendChild(omenu);
function openSheetMenu(items: { label: string; danger?: boolean; fn: () => void }[]) {
  omenu.innerHTML = "";
  for (const it of items) {
    const b = document.createElement("button");
    b.className = "om-item" + (it.danger ? " danger" : "");
    b.textContent = it.label;
    b.onclick = () => { closeSheet(); it.fn(); };
    omenu.appendChild(b);
  }
  omenu.hidden = false;
  scrimOn("sheet");
  navPush("sheet");
}
function closeSheet() {
  navClose("sheet");
  omenu.hidden = true;
  scrimOff("sheet");
}
function openDrawer() {
  $("sidebar").classList.add("open");
  scrimOn("drawer");
  navPush("drawer");
}
function closeDrawer() {
  navClose("drawer");
  $("sidebar").classList.remove("open");
  scrimOff("drawer");
}
$("tb-menu").onclick = () =>
  $("sidebar").classList.contains("open") ? closeDrawer() : openDrawer();
// Swipe depuis le bord gauche → ouvre le drawer ; swipe gauche dessus → ferme.
{
  let sx = 0, sy = 0, swiping = false;
  document.addEventListener("touchstart", (e) => {
    const t0 = e.touches[0];
    const inDrawer = (e.target as HTMLElement).closest("#sidebar") != null;
    if (!IS_ANDROID || document.querySelector("dialog[open]")) { swiping = false; return; }
    swiping = (inDrawer && $("sidebar").classList.contains("open")) ||
      (!inDrawer && t0.clientX <= 26 && !(e.target as HTMLElement).closest("#file-panel, #omenu, #fctx, #sctx"));
    sx = t0.clientX; sy = t0.clientY;
  }, { passive: true });
  document.addEventListener("touchmove", (e) => {
    if (!swiping) return;
    const t0 = e.touches[0];
    const dx = t0.clientX - sx, dy = t0.clientY - sy;
    if (Math.abs(dy) > Math.abs(dx)) { swiping = false; return; }
    if (dx > 56) { openDrawer(); swiping = false; }
    else if (dx < -56 && $("sidebar").classList.contains("open")) { closeDrawer(); swiping = false; }
  }, { passive: true });
}
$("titlebar").addEventListener("dblclick", (e) => {
  if (!IS_ANDROID && !(e.target as HTMLElement).closest("button")) appWin.toggleMaximize();
});
// Drag fenêtre explicite (plus fiable que data-tauri-drag-region seul)
$("titlebar").addEventListener("mousedown", (e) => {
  if (!IS_ANDROID && e.button === 0 && !(e.target as HTMLElement).closest("button"))
    void appWin.startDragging();
});
document.querySelectorAll<HTMLElement>(".resize-edge").forEach((el) => {
  el.addEventListener("mousedown", (e) => {
    e.preventDefault();
    void appWin.startResizeDragging(el.dataset.dir as never);
  });
});

// Settings : thème + langue
$("tb-settings").onclick = () => {
  ($("s-theme") as HTMLSelectElement).value =
    document.documentElement.classList.contains("light") ? "light" : "dark";
  ($("s-lang") as HTMLSelectElement).value = getLang();
  ($("s-autoconn") as HTMLInputElement).checked =
    localStorage.getItem("ac-autoconnect") !== "0";
  ($("s-images") as HTMLInputElement).checked = inlineImages();
  ($("dlg-settings") as HTMLDialogElement).showModal();
};
($("s-theme") as HTMLSelectElement).onchange = (e) =>
  applyTheme((e.target as HTMLSelectElement).value as "dark" | "light");
($("s-autoconn") as HTMLInputElement).onchange = (e) =>
  localStorage.setItem("ac-autoconnect", (e.target as HTMLInputElement).checked ? "1" : "0");
($("s-images") as HTMLInputElement).onchange = (e) =>
  localStorage.setItem("ac-inline-images", (e.target as HTMLInputElement).checked ? "1" : "0");
($("s-lang") as HTMLSelectElement).onchange = (e) => {
  setLang((e.target as HTMLSelectElement).value as Lang);
  refreshSessions();
  updateHeader();
};

// About : version réelle + lien GitHub via le plugin shell
$("tb-about").onclick = async () => {
  try {
    $("about-version").textContent = "v" + (await getVersion());
  } catch {
    /* dev */
  }
  ($("dlg-about") as HTMLDialogElement).showModal();
};
// Menus ⋮ (Android) : l'app-level ouvre Settings/About, celui du chat les
// actions de session (cachées du header sur mobile).
$("tb-more").onclick = () =>
  openSheetMenu([
    { label: `⚙  ${t("settings_title")}`, fn: () => $("tb-settings").click() },
    { label: `ⓘ  ${t("about_title")}`, fn: () => $("tb-about").click() },
  ]);
$("chat-more").onclick = () => {
  const items: { label: string; danger?: boolean; fn: () => void }[] = [];
  if (current) items.push({ label: `📁  ${t("files_title")}`, fn: () => $("btn-files").click() });
  if (current && !alive) items.push({ label: `↻  ${t("resume")}`, fn: () => $("btn-resume").click() });
  items.push({ label: `⟳  ${t("refresh_title")}`, fn: () => $("btn-refresh").click() });
  openSheetMenu(items);
};
$("about-gh").addEventListener("click", (e) => {
  e.preventDefault();
  shellOpen("https://github.com/Wameuh/AgentControl");
});

// Check update au boot : version distante (gh release ou package.json sur
// main) > locale → chip "⬆ vX" dans la titlebar → clic = update (deb+dpkg)
// ou ouverture de la page releases si aucun asset.
function cmpVer(a: string, b: string): number {
  const pa = a.split(".").map(Number);
  const pb = b.split(".").map(Number);
  for (let i = 0; i < 3; i++) {
    const d = (pa[i] || 0) - (pb[i] || 0);
    if (d) return d;
  }
  return 0;
}
// Version distante : fetch direct (pas d'outil local), fallback commande Rust.
async function remoteVersion(): Promise<string> {
  try {
    const r = await fetch(
      "https://api.github.com/repos/Wameuh/AgentControl/releases/latest",
    );
    if (r.ok) {
      const j = (await r.json()) as { tag_name?: string };
      if (j.tag_name) return j.tag_name.replace(/^v/, "");
    }
  } catch {
    /* CSP/réseau → fallback invoke */
  }
  return invoke<string>("check_update");
}
(async () => {
  try {
    const remote = await remoteVersion();
    const local = await getVersion().catch(() => "0.0.0");
    if (cmpVer(remote, local) <= 0) return;
    const b = document.createElement("button");
    b.className = "tb-update";
    b.textContent = `⬆ v${remote}`;
    b.title = `Update v${remote} available — click to install`;
    b.onclick = () => {
      b.textContent = "…";
      performClientUpdate(remote).finally(() => {
        b.textContent = `⬆ v${remote}`;
      });
    };
    $("titlebar").insertBefore(b, document.querySelector(".tb-btns"));
  } catch {
    /* pas de gh/réseau → tant pis */
  }
})();

/// URL de l'APK de la release `tag` (asset `.apk`), fallback = page releases.
async function apkReleaseUrl(tag: string): Promise<string> {
  try {
    const r = await fetch(
      `https://api.github.com/repos/Wameuh/AgentControl/releases/tags/v${tag}`,
    );
    if (r.ok) {
      const j = (await r.json()) as {
        assets?: { name?: string; browser_download_url?: string }[];
      };
      const a = j.assets?.find((x) => x.name?.endsWith(".apk"));
      if (a?.browser_download_url) return a.browser_download_url;
    }
  } catch {
    /* réseau */
  }
  return "https://github.com/Wameuh/AgentControl/releases/latest";
}

/// Update du client vers `tag` : desktop = .deb auto (`update_client`,
/// fallback page releases) ; Android = APK ouvert dans le navigateur —
/// le gestionnaire de paquets fait la maj in-place (signature identique).
async function performClientUpdate(tag: string) {
  if (IS_ANDROID) {
    shellOpen(await apkReleaseUrl(tag));
    return;
  }
  await invoke("update_client", { tag }).catch(() => {
    shellOpen(`https://github.com/Wameuh/AgentControl/releases/tag/v${tag}`);
  });
}

/// Client plus vieux que le démon (minor) : le démon n'est PAS touché —
/// popup proposant l'update du client (desktop : auto, Android : APK).
async function showOutdated(lv: string, dv: string) {
  const dlg = $("dlg-outdated") as HTMLDialogElement;
  if (dlg.open) return;
  const remote = await remoteVersion().catch(() => "");
  const target = remote && cmpVer(remote, dv) >= 0 ? remote : dv;
  $("outdated-msg").textContent = t("outdated_msg", { lv, dv, target });
  const btn = $("outdated-update") as HTMLButtonElement;
  btn.onclick = async () => {
    btn.disabled = true;
    await performClientUpdate(target);
    btn.disabled = false;
    dlg.close();
  };
  dlg.showModal();
}

// Bloque le menu contextuel natif du webview (son "Reload" tuait la connexion)
window.addEventListener("contextmenu", (e) => e.preventDefault());

// ---------------------------------------------------------------------------
// Types miroir du protocole
// ---------------------------------------------------------------------------

type CmdInfo = string | { name: string; description?: string; hint?: string };
const cmdName = (c: CmdInfo) => (typeof c === "string" ? c.replace(/^\//, "") : c.name);
const cmdDesc = (c: CmdInfo) => (typeof c === "string" ? "" : (c.description ?? ""));
const cmdHint = (c: CmdInfo) => (typeof c === "string" ? "" : (c.hint ?? ""));

interface SessionMeta {
  conn?: string;
  /** Connexion down → session affichée depuis le cache (grisée). */
  offline?: boolean;
  group?: string;
  id: string;
  agent: string;
  title: string;
  cwd: string;
  created_at: string;
  native_session_id?: string;
  mode?: string;
  model?: string;
  available_modes: string[];
  available_commands: CmdInfo[];
  available_models: string[];
  config_options?: ConfigOption[];
  state: string;
  context_used?: number;
  context_size?: number;
}

interface ConfigOpt { value: string; name: string; description?: string }
interface ConfigOption {
  id: string; name: string; category?: string;
  currentValue?: string; options: ConfigOpt[];
}

// Devin : les modèles portent l'effort en suffixe (-medium/-high/...) ;
// Cursor/Codex : variantes `base[param=v,…]` (ex. gpt-6.1-sol[medium]).
const EFF_RE = /-(low|minimal|medium|high|max|lightning)$/;
const BRACKET_RE = /(\[[^\]]*\])$/;
const splitModel = (v: string): [string, string] => {
  const b = BRACKET_RE.exec(v);
  if (b) return [v.slice(0, -b[0].length), b[1]];
  const m = EFF_RE.exec(v);
  return m ? [v.slice(0, -m[0].length), m[1]] : [v, ""];
};
// base + variante : "-" pour les efforts suffixe, concat pour les [params]
const joinModel = (base: string, eff: string) =>
  !eff ? base : eff.startsWith("[") ? base + eff : `${base}-${eff}`;

/** Décompose "fusion-<prim>-<eff>-sidekick-<side>-<eff>" → [pb, pe, sb, se]. */
function fusionParts(v: string): [string, string, string, string] | null {
  if (!v.startsWith("fusion-")) return null;
  const rest = v.slice("fusion-".length);
  const i = rest.indexOf("-sidekick-");
  if (i < 0) return null;
  const [pb, pe] = splitModel(rest.slice(0, i));
  const [sb, se] = splitModel(rest.slice(i + "-sidekick-".length));
  return [pb, pe, sb, se];
}

interface AcEvent {
  seq: number;
  ts: string;
  type: string;
  [k: string]: unknown;
}

// ---------------------------------------------------------------------------
// État UI
// ---------------------------------------------------------------------------

let current: SessionMeta | null = null;
let minRendered = Number.MAX_SAFE_INTEGER;
let renderedCount = 0;
const MAX_RENDERED = 400;
const PAGE = 50; // paquets au scroll
const INITIAL_PAGE = 200; // base d'historique à l'ouverture
const toolBlocks = new Map<string, HTMLElement>();
const answered = new Set<string>();
let loadingOld = false;
let hasMoreHistory = true;
let alive = false;

const eventsEl = $("events");
const sentinel = $("scroll-sentinel");

// Option « images en ligne » (Settings, défaut activé) : les URLs http(s)
// finissant par une extension image sont rendues en <img> cliquable.
const IMG_URL_RE = /\.(?:png|jpe?g|gif|webp|avif|bmp|svg|ico)(?:[?#][^\s<]*)?$/i;
const inlineImages = () => localStorage.getItem("ac-inline-images") !== "0";
// Image cassée (404, hôte inaccessible…) → on la remplace par son texte/URL,
// l'<a> englobant reste cliquable. capture=true : error ne bubble pas.
eventsEl.addEventListener(
  "error",
  (e) => {
    const img = e.target;
    if (img instanceof HTMLImageElement && img.classList.contains("md-img"))
      img.replaceWith(img.alt || img.src);
  },
  true,
);

// Redimensionnement de la sidebar : drag sur la poignée bord droit.
// Largeur persistée dans localStorage.
{
  const bar = $("sidebar");
  const grip = $("sidebar-resize");
  const clamp = (x: number) => Math.min(520, Math.max(180, x));
  const saved = Number(localStorage.getItem("sidebar-w"));
  if (saved) {
    bar.style.width = clamp(saved) + "px";
    bar.style.minWidth = clamp(saved) + "px";
  }
  grip.addEventListener("mousedown", (e) => {
    e.preventDefault();
    grip.classList.add("drag");
    document.body.style.userSelect = "none";
    const move = (ev: MouseEvent) => {
      const w = clamp(ev.clientX);
      bar.style.width = w + "px";
      bar.style.minWidth = w + "px";
    };
    const up = (ev: MouseEvent) => {
      document.removeEventListener("mousemove", move);
      document.removeEventListener("mouseup", up);
      grip.classList.remove("drag");
      document.body.style.userSelect = "";
      localStorage.setItem("sidebar-w", String(clamp(ev.clientX)));
    };
    document.addEventListener("mousemove", move);
    document.addEventListener("mouseup", up);
  });
}

/** Erreur visible et copiable (remplace alert). */
function showErr(msg: string) {
  const bar = $("err-bar");
  $("err-text").textContent = msg;
  bar.hidden = false;
}
$("err-close").addEventListener("click", () => ($("err-bar").hidden = true));

async function call<T = unknown>(method: string, params: Record<string, unknown> = {}): Promise<T> {
  const p = { ...params };
  // conn absent (undefined) → injecter celui de la session affichée ; un
  // appelant peut passer conn:"" explicitement (envois en arrière-plan).
  if (p.session && p.conn === undefined && current?.conn) p.conn = current.conn;
  // Conn marquée down → échec rapide au lieu d'attendre le timeout RPC.
  const ck = (p.conn as string) || "";
  if (ck && (connLive.get(ck) ?? "ok") !== "ok") throw new Error(t("conn_offline"));
  return invoke<T>("call", { method, params: p });
}

// ---------------------------------------------------------------------------
// Connexion
// ---------------------------------------------------------------------------

const minorOf = (v: string) => v.split(".").slice(0, 2).join(".");

// Groupes nommés par connexion : persistés côté client (les sessions portent
// leur groupe dans meta.json ; le store conserve aussi les groupes VIDES).
const GKEY = (c: string) => `ac-groups:${c}`;
function storedGroups(conn: string): string[] {
  try { return JSON.parse(localStorage.getItem(GKEY(conn)) ?? "[]") as string[]; }
  catch { return []; }
}
function saveGroups(conn: string, grps: string[]) {
  localStorage.setItem(GKEY(conn), JSON.stringify([...new Set(grps)].sort()));
}
function addStoredGroup(conn: string, name: string) {
  if (name) saveGroups(conn, [...storedGroups(conn), name]);
}
function removeStoredGroup(conn: string, name: string) {
  saveGroups(conn, storedGroups(conn).filter((g) => g !== name));
}

// Dialogue groupe : mode "session" (assigner), "rename" (renommer/dissoudre),
// "new" (créer un groupe vide dans une connexion).
let grpTarget: SessionMeta | null = null;
let grpMode: "session" | "rename" | "new" = "session";
let grpCtx = { conn: "", name: "" };

function openGroupDlg(mode: "session" | "rename" | "new", s: SessionMeta | null, conn = "", name = "") {
  grpMode = mode;
  grpTarget = s;
  grpCtx = { conn, name };
  const dlg = $("dlg-group") as HTMLDialogElement;
  dlg.querySelector("h3")!.textContent =
    t(mode === "rename" ? "rename_group" : mode === "new" ? "new_group" : "group_h");
  ($("g-name") as HTMLInputElement).value = mode === "session" ? (s?.group ?? "") : name;
  ($("g-none") as HTMLButtonElement).hidden = mode === "new";
  ($("g-none") as HTMLButtonElement).textContent =
    t(mode === "rename" ? "dissolve_group" : "group_none");
  const dl = $("g-list");
  dl.innerHTML = "";
  call<SessionMeta[]>("list_sessions").then((ls) => {
    const conns = mode === "session" ? [s?.conn ?? ""] : [conn];
    const all = new Set(conns.flatMap((c) => storedGroups(c)));
    for (const g of all) { const o = document.createElement("option"); o.value = g; dl.appendChild(o); }
    for (const g of new Set(ls.filter((x) => x.group).map((x) => x.group!))) {
      const o = document.createElement("option");
      o.value = g;
      dl.appendChild(o);
    }
  });
  dlg.showModal();
}

async function applyGroupAssign(s: SessionMeta, group?: string) {
  await call("set_session_group", { session: s.id, conn: s.conn, group }).catch((er) =>
    showErr(String(er)),
  );
  if (group) addStoredGroup(s.conn ?? "default", group);
}

($("dlg-group") as HTMLDialogElement).addEventListener("close", async (e) => {
  const rv = (e.target as HTMLDialogElement).returnValue;
  if (rv === "cancel") return;
  const name = ($("g-name") as HTMLInputElement).value.trim();
  if (grpMode === "session" && grpTarget) {
    await applyGroupAssign(grpTarget, rv === "none" ? undefined : name || undefined);
  } else if (grpMode === "rename" && grpCtx.name) {
    const members = lastSessions.filter(
      (s) => (s.conn ?? "default") === grpCtx.conn && s.group === grpCtx.name,
    );
    if (rv === "none" || !name) {
      for (const s of members) await applyGroupAssign(s, undefined);
      removeStoredGroup(grpCtx.conn, grpCtx.name);
    } else if (name !== grpCtx.name) {
      for (const s of members) await applyGroupAssign(s, name);
      removeStoredGroup(grpCtx.conn, grpCtx.name);
    }
  } else if (grpMode === "new" && name) {
    addStoredGroup(grpCtx.conn, name);
  }
  refreshSessions();
});
($("g-ok") as HTMLButtonElement).onclick = () => {
  ($("dlg-group") as HTMLDialogElement).close("ok");
};
// type=button → fermeture explicite ; Enter active g-ok (seul submit restant).
$("g-none").onclick = () => ($("dlg-group") as HTMLDialogElement).close("none");
$("g-cancel").onclick = () => ($("dlg-group") as HTMLDialogElement).close("cancel");
// Doit refléter conn_key() côté Rust : ssh → "ssh{,-native}:host:sshport".
const connKeyOf = (cfg: Record<string, unknown>) => {
  if (cfg.kind === "ssh") {
    const nat = cfg.native === true || IS_ANDROID;
    const p = Number(cfg.port) || 22;
    return `${nat ? "ssh-native" : "ssh"}:${cfg.host}:${p}`;
  }
  return cfg.kind === "tcp" ? `tcp:${cfg.addr}` : "local";
};

/// Params ensure_daemon — forward les champs ssh de la cfg (natif ou CLI).
function daemonParams(cfg: Record<string, unknown>, tag: string) {
  return {
    host: cfg.host,
    args: cfg.args ?? [],
    tag,
    port: cfg.port ?? null,
    user: cfg.user ?? null,
    password: cfg.password ?? null,
    key: cfg.key ?? null,
    native: cfg.native ?? null,
  };
}

/** Dialogue modal pendant install/update du démon distant. */
function daemonDlg(msg: string, open: boolean) {
  const d = $("dlg-daemon") as HTMLDialogElement;
  if (open) {
    ($("dlg-daemon-msg")).textContent = msg;
    if (!d.open) d.showModal();
  } else if (d.open) d.close();
}

/** Démon plus vieux que le client (minor ≠) : l'update redémarre le démon
 *  et tue les agents en cours → popup de confirmation. Le message compte
 *  les sessions vivantes de cette connexion (0 si la requête échoue). */
async function confirmDaemonUpdate(
  dv: string, lv: string, host: string, conn: string,
): Promise<boolean> {
  const dlg = $("dlg-dupdate") as HTMLDialogElement;
  if (dlg.open) return false;
  // list_sessions agrège toutes les conns (params.conn ignoré côté Rust)
  // → filtre client sur s.conn. "Vivant" = ni dead ni interrupted (idle
  // inclus : le restart tue son process aussi). Échec → compteur inconnu.
  const sessions = await call<SessionMeta[]>("list_sessions", { conn })
    .catch(() => null);
  const n = sessions?.filter((s) =>
    s.conn === conn && s.state !== "dead" && s.state !== "interrupted").length;
  $("dupdate-msg").textContent = n == null
    ? t("dupdate_msg_unk", { dv, lv, host })
    : t("dupdate_msg", { dv, lv, host, n });
  return new Promise((resolve) => {
    ($("dupdate-go") as HTMLButtonElement).onclick = () => {
      resolve(true);
      dlg.close();
    };
    dlg.addEventListener("close", () => resolve(false), { once: true });
    dlg.showModal();
  });
}

async function doConnect(cfg: Record<string, unknown>) {
  let status: unknown;
  let newerDaemon = "";
  try {
    // Toujours établir la connexion — daemon_status fusionné renvoie Ok
    // même sans connexions, il ne peut pas servir de pré-vérification.
    status = await invoke("connect", { cfg });
  } catch (e) {
    // SSH : échec → démon absent ou arrêté → bootstrap depuis la release.
    if (cfg.kind !== "ssh" || !cfg.host) throw e;
    const ver = await getVersion().catch(() => "0.0.0");
    daemonDlg(t("installing_daemon") + " — " + String(cfg.host), true);
    try {
      const res = await invoke<string>("ensure_daemon", daemonParams(cfg, ver));
      // Démon plus récent : le script distant refuse le downgrade.
      if (res.startsWith("newer:")) newerDaemon = res.slice(6);
    } finally {
      daemonDlg("", false);
    }
    status = await invoke("connect", { cfg }).catch((e2) => {
      if (newerDaemon) showOutdated(ver, newerDaemon);
      throw e2;
    });
  }
  // Incompatibilité de version mineure (x.y) — jamais à chaque patch.
  const dv = (status as { version?: string }).version || newerDaemon;
  const lv = await getVersion().catch(() => "0.0.0");
  if (dv && lv && minorOf(dv) !== minorOf(lv)) {
    if (cmpVer(minorOf(dv), minorOf(lv)) > 0) {
      // Démon plus récent → client outdated : popup, démon non modifié.
      showOutdated(lv, dv);
    } else if (cfg.kind === "ssh" && cfg.host) {
      // Redémarrer le démon tue les agents en cours → validation utilisateur.
      if (await confirmDaemonUpdate(dv, lv, String(cfg.host), connKeyOf(cfg))) {
        daemonDlg(t("updating_daemon", { dv, lv }) + " — " + String(cfg.host), true);
        try {
          const res = await invoke<string>("ensure_daemon", daemonParams(cfg, lv))
            .catch((e) => {
              showErr(String(e));
              return "";
            });
          // Un autre client a pu upgrader le démon entre-temps → même garde-fou.
          if (res.startsWith("newer:")) showOutdated(lv, res.slice(6));
          else status = await invoke("connect", { cfg }).catch(() => status);
        } finally {
          daemonDlg("", false);
        }
      }
    }
  }
  // Connexion établie → la surveillance/backoff la suit.
  const ck = connKeyOf(cfg);
  connCfg.set(ck, cfg);
  markAlive(ck);
  await refreshSessions();
}

// ---------------------------------------------------------------------------
// Liste des sessions
// ---------------------------------------------------------------------------

/** Gemme d'état : dorée working, rouge question/failed, verte idle, grise dead. */
const DOT_ROLE: Record<string, string> = {
  working: "human", running: "human", starting: "human",
  waiting_input: "inference", failed: "inference", compacting: "inference",
  idle: "neutral",
  interrupted: "slate", dead: "slate",
};
function dotIcon(state: string): string {
  return gemSVG(DOT_ROLE[state] ?? "slate");
}

/** La session courante a été supprimée (autre client / janitor) :
 *  vue figée avec marqueur, `current` libéré — les events affichés
 *  restent lisibles mais plus rien ne partira. */
function markSessionGone() {
  if (!current) return;
  addSysline(t("session_gone"));
  current = null;
  unseenBelow = 0;
  $("btn-bottom").hidden = true;
  $("chat-gem").hidden = true;
  for (const b of ["btn-resume", "btn-cancel", "btn-steer"]) $(b).hidden = true;
  const st = $("chat-state");
  st.textContent = "gone";
  st.className = "badge st-dead";
  updateHeader();
}

/** Icône fenêtre : rouge si une question attend, dorée si un agent tourne,
 *  grise sans agent vivant, gemme sauge sinon (no-op sur Android). */
let lastIconState = "";
function setAppIcon(state: string) {
  if (state === lastIconState) return;
  lastIconState = state;
  invoke("set_app_icon", { state }).catch(() => {});
}

async function refreshSessions() {
  const raw = await call<SessionMeta[]>("list_sessions").catch((e) => {
    setAppIcon("grey"); // démon injoignable → aucun agent joignable
    throw e;
  });
  // Marqueur Rust `conn_error` : la sonde list_sessions d'une conn a
  // échoué (timeout 8 s) → la marquer down (une seule fois : déjà down,
  // markDead re-rendererait refreshSessions en boucle).
  const list = raw.filter((s) => {
    if (!(s as { conn_error?: boolean }).conn_error) return true;
    const k = s.conn ?? "";
    if ((connLive.get(k) ?? "ok") === "ok") markDead(k);
    return false;
  });
  // Conn down : ses sessions n'arrivent plus — réafficher le dernier cache
  // connu (grisé) au lieu de faire disparaître le serveur.
  for (const [k, st] of connLive) {
    if (st !== "ok" && !list.some((s) => (s.conn ?? "default") === k)) {
      for (const s of lastSessions.filter((x) => (x.conn ?? "default") === k)) {
        list.push({ ...s, offline: true });
      }
    }
  }
  lastSessions = list;
  const live = list.filter((s) => !s.offline && s.state !== "dead" && s.state !== "interrupted");
  setAppIcon(
    live.some((s) => s.state === "waiting_input")
      ? "red"
      : live.some(
            (s) =>
              s.state === "working" ||
              s.state === "running" ||
              s.state === "starting" ||
              s.state === "compacting",
          )
        ? "gold"
        : live.length > 0 ? "base" : "grey",
  );
  // Session courante disparue alors que sa connexion répond encore
  // (sessions du même serveur présentes) → supprimée côté serveur.
  if (
    current &&
    !list.some((s) => s.id === current!.id) &&
    list.some((s) => (s.conn ?? "") === (current!.conn ?? ""))
  ) {
    markSessionGone();
  }
  // Lumières "agents" du fond : une par session active (les sessions
  // cachées d'une conn down ne comptent pas).
  const n = list.filter(
    (s) =>
      !s.offline &&
      (s.state === "working" || s.state === "waiting_input" || s.state === "compacting"),
  ).length;
  (window as { MeshBackground?: { setMood: (m: string, o: object) => void } })
    .MeshBackground?.setMood(bgMood(), { agents: Math.min(n, 4), intensity: 0.55 });
  const el = $("session-list");
  el.innerHTML = "";
  // Rendu : connexion → groupes nommés (le store client garde les groupes
  // VIDES visibles) → sessions non groupées.
  const byConn = new Map<string, SessionMeta[]>();
  for (const s of list) {
    const c = s.conn ?? "default";
    if (!byConn.has(c)) byConn.set(c, []);
    byConn.get(c)!.push(s);
  }
  // Conns suivies même sans session (ex. serveur down) → header + badge.
  for (const conn of [...new Set([...connCfg.keys(), ...byConn.keys()])].sort()) {
    const sessions = (byConn.get(conn) ?? []).sort((a, b) => a.title.localeCompare(b.title));
    const key = `ac-grp-${conn}`;
    const connClosed = localStorage.getItem(key) === "1";
    const st = connLive.get(conn) ?? "ok";
    const ch = document.createElement("div");
    ch.className = "grp-head" + (connClosed ? " closed" : "");
    const badge = connCfg.has(conn)
      ? `<span class="conn-st ${st === "ok" ? "ok" : st === "reconnecting" ? "warn" : "bad"}"` +
        ` title="${esc(connTip(st))}">${st === "reconnecting" ? "⟳" : "●"}</span> `
      : "";
    ch.innerHTML = `<span class="grp-arrow">${connClosed ? "▸" : "▾"}</span> ${badge}${esc(conn)}` +
      `<button class="grp-add" title="${esc(t("new_group"))}">＋</button>`;
    ch.onclick = () => {
      localStorage.setItem(key, connClosed ? "0" : "1");
      refreshSessions();
    };
    ch.querySelector(".grp-add")!.addEventListener("click", (e) => {
      e.stopPropagation();
      openGroupDlg("new", null, conn);
    });
    // Badge ≠ ok → clic = reconnecter maintenant (retry simple).
    ch.querySelector(".conn-st")?.addEventListener("click", (e) => {
      e.stopPropagation();
      if ((connLive.get(conn) ?? "ok") !== "ok") manualRetry(conn);
    });
    // Déposer sur le header connexion = dégrouper la session
    bindDropTarget(ch, conn, undefined);
    el.appendChild(ch);

    const grpNames = [
      ...new Set([
        ...storedGroups(conn),
        ...sessions.map((s) => s.group).filter((g): g is string => !!g),
      ]),
    ].sort();
    for (const grp of grpNames) {
      const gkey = `ac-grp-${conn}::${grp}`;
      const grpClosed = connClosed || localStorage.getItem(gkey) === "1";
      const g = document.createElement("div");
      g.className = "grp-sub" + (grpClosed ? " closed" : "");
      g.innerHTML = `<span class="grp-arrow">${grpClosed ? "▸" : "▾"}</span> ${esc(grp)}` +
        `<button class="grp-add" title="${esc(t("new_sess_grp"))}">＋</button>`;
      g.onclick = () => {
        localStorage.setItem(gkey, localStorage.getItem(gkey) === "1" ? "0" : "1");
        refreshSessions();
      };
      // + : nouvel agent dans le groupe — agent + workspace du 1er membre
      // préremplis (sessions triées par titre → "le premier" = premier affiché).
      g.querySelector(".grp-add")!.addEventListener("click", (e) => {
        e.stopPropagation();
        const apiConn = conn === "default" ? undefined : conn;
        pendingGroup = { conn: apiConn, name: grp };
        void openNewSessionDlg(apiConn, sessions.find((x) => x.group === grp));
      });
      bindCtx(g, () => openGroupDlg("rename", null, conn, grp));
      bindDropTarget(g, conn, grp);
      el.appendChild(g);
      for (const s of sessions.filter((x) => x.group === grp)) {
        el.appendChild(sessRow(s, conn, grp, grpClosed));
      }
    }
    for (const s of sessions.filter((x) => !x.group)) {
      el.appendChild(sessRow(s, conn, "", connClosed));
    }
  }
}

const DTYPE = "application/x-ac-session";

/** Cible de drop pour une session : conn = câble, group undefined = dégrouper. */
function bindDropTarget(el: HTMLElement, conn: string, group: string | undefined) {
  el.addEventListener("dragover", (e) => {
    if (e.dataTransfer?.types.includes(DTYPE)) {
      e.preventDefault();
      el.classList.add("drop-hint");
    }
  });
  el.addEventListener("dragleave", () => el.classList.remove("drop-hint"));
  el.addEventListener("drop", async (e) => {
    e.preventDefault();
    el.classList.remove("drop-hint");
    try {
      const d = JSON.parse(e.dataTransfer!.getData(DTYPE)) as { id: string; conn: string };
      if (d.conn !== conn) return; // pas de déplacement inter-serveur
      const s = lastSessions.find((x) => x.id === d.id);
      if (!s || (s.group ?? "") === (group ?? "")) return;
      await applyGroupAssign(s, group);
      refreshSessions();
    } catch {
      /* donnée étrangère : ignorer */
    }
  });
}

let lastSessions: SessionMeta[] = [];

// Menu contextuel session (clic droit / appui long) : rename / fork / exit.
// Même pattern que #fctx — fixe au point cliqué sur desktop, action-sheet
// ancrée en bas sur Android.
const sessCtx = $("sctx");

function hideSessCtx() {
  navClose("sctx");
  sessCtx.hidden = true;
}

function openSessionCtx(x: number, y: number, s: SessionMeta, row: HTMLElement) {
  sessCtx.innerHTML = `<div class="fctx-path">${esc(s.title)}</div>`;
  const add = (label: string, cls: string, fn: (b: HTMLButtonElement) => void) => {
    const b = document.createElement("button");
    b.textContent = label;
    b.className = cls;
    b.onclick = () => fn(b);
    sessCtx.appendChild(b);
    return b;
  };
  add(t("ctx_rename"), "", () => {
    hideSessCtx();
    renameSessRow(s, row);
  });
  // conn: s.conn — la session peut vivre sur une autre connexion que
  // celle affichée ; sans ça call() routerait vers le mauvais démon.
  const forkBtn = add(t("ctx_fork"), "", () => {
    hideSessCtx();
    if (current?.id === s.id) addSysline(t("fork_started"));
    void call<{ session: string }>("fork_session", { session: s.id, conn: s.conn })
      .then(async (r) => {
        await refreshSessions();
        if (r.session) await openSession(r.session);
      })
      .catch((e) => showErr(String(e)));
  });
  // Fork = respawn depuis la session native — impossible sans id natif.
  forkBtn.disabled = !s.native_session_id;
  add(t("ctx_group"), "", () => {
    hideSessCtx();
    openGroupDlg("session", s);
  });
  // Exit = tue le process agent (kill_session) — la session reste, reprenable.
  // Action destructive → double-clic armé comme delete_path / sess-del.
  add(t("ctx_exit"), "danger", (b) => {
    if (!b.dataset.armed) {
      b.dataset.armed = "1";
      b.textContent = t("ctx_confirm");
      return;
    }
    hideSessCtx();
    void call("kill_session", { session: s.id, conn: s.conn })
      .then(() => refreshSessions())
      .catch((e) => showErr(String(e)));
  });
  sessCtx.hidden = false;
  navPush("sctx");
  if (!IS_ANDROID) {
    const r = sessCtx.getBoundingClientRect();
    sessCtx.style.left = `${Math.min(x, window.innerWidth - r.width - 8)}px`;
    sessCtx.style.top = `${Math.min(y, window.innerHeight - r.height - 8)}px`;
  }
}

/** Renommage inline du titre dans la ligne session (même pattern que le
 *  double-clic sur chat-title). */
function renameSessRow(s: SessionMeta, row: HTMLElement) {
  const span = row.querySelector<HTMLElement>(".t");
  if (!span) return;
  const inp = document.createElement("input");
  inp.id = "title-edit";
  inp.value = s.title;
  span.replaceWith(inp);
  row.draggable = false; // sinon un drag depuis l'input part en drag de ligne
  inp.focus();
  inp.select();
  // blur ré-entrant : retirer l'input focusé (Escape, ou refreshSessions
  // qui rebuild la liste) redéclenche blur → sans garde, une annulation
  // persisterait le texte tapé. !isConnected = ligne déjà reconstruite.
  let finished = false;
  const done = async (save: boolean) => {
    if (finished || !inp.isConnected) { finished = true; return; }
    finished = true;
    if (!IS_ANDROID) row.draggable = true;
    const v = inp.value.trim();
    inp.replaceWith(span);
    if (save && v && v !== s.title) {
      s.title = v;
      span.textContent = v;
      if (current?.id === s.id) {
        current.title = v;
        $("chat-title").textContent = v;
      }
      await call("rename_session", { session: s.id, title: v, conn: s.conn })
        .catch((e) => showErr(String(e)));
      refreshSessions();
    }
  };
  inp.addEventListener("click", (e) => e.stopPropagation());
  inp.addEventListener("keydown", (e) => {
    if (e.key === "Enter") done(true);
    if (e.key === "Escape") done(false);
  });
  inp.addEventListener("blur", () => done(true));
}

// Fermeture : clic ailleurs / Échap / scroll de la liste. Un clic DANS le
// menu ne doit pas le fermer avant l'action du bouton.
sessCtx.addEventListener("click", (e) => e.stopPropagation());
window.addEventListener("click", hideSessCtx);
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") hideSessCtx();
});
$("session-list").addEventListener("scroll", hideSessCtx);

function sessRow(s: SessionMeta, conn: string, grp: string, closed: boolean): HTMLElement {
  const div = document.createElement("div");
  div.className = "sess" + (grp ? " in-grp" : "") + (s.offline ? " offline" : "") +
    (current?.id === s.id ? " active" : "");
  if (closed) div.style.display = "none";
  div.innerHTML = `<span class="dot st-${s.state}" title="${esc(s.state)}">${dotIcon(s.state)}</span><span class="t">${esc(s.title)}</span><span class="a">${esc(s.agent)}</span><button class="sess-del" title="Supprimer">✕</button>`;
  div.onclick = () => {
    openSession(s.id);
    // Mobile : fermer le drawer après sélection.
    closeDrawer();
  };
  bindCtx(div, (x, y) => openSessionCtx(x, y, s, div));
  if (!IS_ANDROID) {
    div.draggable = true;
    div.addEventListener("dragstart", (e) => {
      e.dataTransfer!.setData(DTYPE, JSON.stringify({ id: s.id, conn }));
      e.dataTransfer!.effectAllowed = "move";
    });
  }
  const del = div.querySelector<HTMLElement>(".sess-del")!;
  del.addEventListener("click", async (e) => {
    e.stopPropagation();
    if (!del.dataset.armed) {
      del.dataset.armed = "1";
      del.textContent = t("delete_armed");
      setTimeout(() => { del.dataset.armed = ""; del.textContent = "✕"; }, 2500);
      return;
    }
    await call("delete_session", { session: s.id, conn: s.conn }).catch((er) => showErr(String(er)));
    if (current?.id === s.id) {
      current = null;
      eventsEl.innerHTML = "";
    }
    refreshSessions();
  });
  return div;
}

// ---------------------------------------------------------------------------
// Ouverture d'une session : session_info + dernière page + subscribe
// ---------------------------------------------------------------------------

let openSeq = 0;

async function openSession(id: string) {
  const seq = ++openSeq;
  const stale = () => seq !== openSeq; // un openSession plus récent a pris le relais
  // La meta démon ne porte pas sa conn (notion client) — la reprendre de
  // la liste (stampée par le merge) pour router session_info sur le bon
  // serveur ; "" → connexion par défaut.
  const connHint = lastSessions.find((s) => s.id === id)?.conn ?? "";
  const info = await call<{ meta: SessionMeta; pending: string[]; alive: boolean; last_seq: number }>(
    "session_info",
    { session: id, conn: connHint },
  );
  if (stale()) return;
  alive = info.alive;
  current = info.meta;
  if (!current.conn && connHint) current.conn = connHint;
  minRendered = Number.MAX_SAFE_INTEGER;
  renderedCount = 0;
  hasMoreHistory = true;
  toolBlocks.clear();
  answered.clear();
  unseenBelow = 0;
  $("btn-bottom").hidden = true;
  for (const c of eventsEl.querySelectorAll(".ev")) c.remove();

  $("chat-title").textContent = `${current.title} — ${current.agent}`;
  updateHeader();

  // Auto-resume : agent mort (ex. restart démon) → reprise silencieuse
  // plutôt qu'un "interrupted" qui demande un clic.
  if (!alive && current.native_session_id) {
    const sys = document.createElement("div");
    sys.className = "sysline";
    sys.textContent = "⏳ reprise de l'agent…";
    eventsEl.appendChild(sys);
    try {
      await call("resume_session", { session: id });
      if (stale()) return;
      const info2 = await call<{ meta: SessionMeta; alive: boolean }>("session_info", {
        session: id,
      });
      if (stale()) return;
      alive = info2.alive;
      current = info2.meta;
      if (!current.conn && connHint) current.conn = connHint;
      sys.textContent = "✔ agent repris";
      updateHeader();
    } catch (e) {
      sys.textContent = `⚠ reprise: ${e}`;
    }
  }

  // Charge la dernière page d'événements — un échec (journal absent/corrompu)
  // ne doit PAS tuer le subscribe ni la vue live derrière.
  let page = { events: [] as AcEvent[], last_seq: 0 };
  try {
    page = await call<{ events: AcEvent[]; last_seq: number }>("events", {
      session: id,
      limit: INITIAL_PAGE,
    });
  } catch (e) {
    showErr(`events: ${e}`);
  }
  if (stale()) return;
  for (const ev of page.events) appendEvent(ev, false);
  scrollBottom();

  // Subscribe live à partir du dernier event connu
  const afterSeq = page.events.length ? page.events[page.events.length - 1].seq : 0;
  if (stale()) return;
  await invoke("subscribe", { session: id, afterSeq, conn: current?.conn ?? connHint }).catch((e) => {
    // Le flux live est mort (ex. restart démon) → erreur visible
    showErr(`subscribe: ${e}`);
  });
  refreshSessions();
  // L'arbre suit le workspace de la session nouvellement ouverte
  if (treeOpen) void loadTreeRoot();
}

const fmtTok = (n: number) =>
  n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${Math.round(n / 1e3)}k` : String(n);

/** Jauge de contexte dans le header : "53k/200k" + tooltip détaillé. */
function updateCtx() {
  const el = $("chat-ctx");
  const used = current?.context_used;
  const size = current?.context_size;
  if (used == null || !size) {
    el.hidden = true;
    return;
  }
  const pct = Math.round((used / size) * 100);
  el.hidden = false;
  el.textContent = `${fmtTok(used)}/${fmtTok(size)}`;
  el.title = t("ctx_title", {
    used: used.toLocaleString(),
    size: size.toLocaleString(),
    pct,
  });
  el.classList.toggle("ctx-hot", pct >= 80);
}

function updateHeader() {
  $("btn-files").hidden = !current;
  if (!current) {
    $("chat-ctx").hidden = true;
    return;
  }
  refreshQueue();
  updateCtx();
  const st = $("chat-state");
  st.textContent = current.state;
  st.className = `badge st-${current.state}`;
  // Le gem tourne aussi pendant "compacting" (le démon retient les
  // envois en file jusqu'au signal de fin du compact).
  $("chat-gem").hidden = current.state !== "working" && current.state !== "compacting";
  // Reprendre = process mort, quel que soit l'état persisté dans meta.json
  $("btn-resume").hidden = alive;
  const busySt =
    current.state === "working" ||
    current.state === "waiting_input" ||
    current.state === "compacting";
  $("btn-cancel").hidden = !busySt;
  // Steer : n'a de sens que pendant un vrai tour (sinon = envoi normal ;
  // pendant compact un envoi est simplement retenu en file).
  $("btn-steer").hidden = current.state !== "working";

  // Options de config dynamiques (ex. Devin : config_option_update → mode + model)
  const copts = current.config_options ?? [];
  const modeOpt = copts.find((o) => o.id === "mode" || o.category === "mode");
  const modelOpt = copts.find((o) => o.id === "model" || o.category === "model");

  const ms = $("mode-select") as HTMLSelectElement;
  if (modeOpt || current.available_modes.length) {
    ms.hidden = false;
    ms.innerHTML = "";
    const items = modeOpt
      ? modeOpt.options.map((o) => ({ v: o.value, label: o.name || o.value }))
      : current.available_modes.map((m) => ({ v: m, label: m }));
    for (const it of items) {
      const o = document.createElement("option");
      o.value = it.v;
      o.textContent = it.label;
      ms.appendChild(o);
    }
    ms.dataset.cfgid = modeOpt ? modeOpt.id : "";
    ms.value = modeOpt?.currentValue || current.mode || "";
  } else {
    ms.hidden = true;
  }

  const md = $("model-select") as HTMLSelectElement;
  const es = $("effort-select") as HTMLSelectElement;
  const mf = $("model-free") as HTMLInputElement;
  const rawOpts = modelOpt
    ? modelOpt.options.map((o) => ({ v: o.value, label: o.name || o.value }))
    : current.available_models.map((m) => ({ v: m, label: m }));
  if (rawOpts.length) {
    md.hidden = false;
    mf.hidden = true;
    // Modèles de base dédupliqués (effort géré à part) + entrée Fusion
    const bases = new Map<string, string>();
    for (const o of rawOpts) {
      if (o.v.startsWith("fusion-")) continue;
      const [b] = splitModel(o.v);
      if (!bases.has(b)) bases.set(b, o.label);
    }
    md.innerHTML = "";
    for (const [b, label] of bases) {
      const o = document.createElement("option");
      o.value = b;
      o.textContent = "🧠 " + label;
      md.appendChild(o);
    }
    // Entrée Fusion : uniquement si l'agent annonce des modèles fusion-*
    // (Devin — cursor/codex n'en ont pas, inutile d'afficher une option KO).
    const hasFusion = rawOpts.some((o) => o.v.startsWith("fusion-"));
    const fo = document.createElement("option");
    fo.value = "__fusion";
    fo.textContent = t("fusion_entry");
    if (hasFusion) md.appendChild(fo);
    md.dataset.cfgid = modelOpt ? modelOpt.id : "";
    md.dataset.opts = JSON.stringify(rawOpts);
    const curVal = modelOpt?.currentValue || current.model || "";
    const curIsFusion = curVal.startsWith("fusion-");
    if (curIsFusion) {
      const fp = rawOpts.find((o) => o.v === curVal);
      if (fp) fo.textContent = `✨ ${fp.label}`;
    }
    const [curBase, curEff] = splitModel(curVal);
    md.value = curIsFusion ? "__fusion" : curBase;
    // Bouton ⚙ : reconfigurer la fusion quand elle est déjà active
    // (re-sélectionner la même option dans un <select> ne tire pas change)
    $("btn-fusion-edit").hidden = !curIsFusion;
    // Select d'effort : suffixes réellement présents pour la base choisie
    // (masqué en fusion — les efforts se choisissent dans le dialogue)
    fillEfforts(md.value, curEff);
    es.hidden = curIsFusion || es.options.length <= 1;
  } else {
    md.hidden = true;
    es.hidden = true;
    $("btn-fusion-edit").hidden = true;
    mf.hidden = false;
    mf.value = current.model ?? "";
  }

  // Autres options de config exposées par l'agent (ex. Devin : thought_level,
  // speed) — rendues dynamiquement, aucun hardcode.
  const dyn = $("dyn-opts");
  dyn.innerHTML = "";
  for (const o of copts) {
    if (o === modeOpt || o === modelOpt || !(o.options?.length)) continue;
    const sel = document.createElement("select");
    sel.title = o.name || o.id;
    for (const opt of o.options) {
      const el = document.createElement("option");
      el.value = opt.value;
      el.textContent = opt.name || opt.value;
      sel.appendChild(el);
    }
    sel.value = o.currentValue ?? "";
    sel.onchange = () =>
      call("set_config_option", {
        session: current!.id,
        config_id: o.id,
        value: sel.value,
      }).catch((e) => showErr(String(e)));
    dyn.appendChild(sel);
  }
}

// ---------------------------------------------------------------------------
// Rendu des événements
// ---------------------------------------------------------------------------

function esc(s: string): string {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

// Pour les valeurs insérées dans un attribut "…" : esc() n'échappe pas '"'.
const q = (s: string) => s.replace(/"/g, "&quot;");

// Chemins de fichiers cliquables → panneau latéral (read_file côté démon)
const FILE_RE =
  /((?:\/[\w.\-~]+){2,}|[\w.\-]+(?:\/[\w.\-]+){2,}|[\w.\-/]+\.(?:rs|ts|tsx|js|jsx|py|toml|json|md|html?|css|ya?ml|sh|c|cpp|h|hpp|go|java|rb|txt|log|svg|cfg|ini|sql|vue|svelte|lock|service))(?::(\d+))?/g;

function linkify(escaped: string): string {
  // URLs d'abord → vrais liens (placeholder \x00U pour ne pas être re-matchées
  // par FILE_RE ni corrompues par le markup)
  const urls: string[] = [];
  let work = escaped.replace(/https?:\/\/[^\s< ]+/g, (m) => {
    const clean = m.replace(/(?:&(?:quot|#39|lt|gt);|[.,;:!?)\]])+$/, "");
    const inner =
      inlineImages() && IMG_URL_RE.test(clean)
        ? `<img class="md-img" src="${q(clean)}" alt="${q(clean)}" loading="lazy">`
        : clean;
    urls.push(
      `<a class="ulink" href="${q(clean)}" target="_blank" rel="noopener noreferrer">${inner}</a>`,
    );
    return `\x00U${urls.length - 1}\x00${m.slice(clean.length)}`;
  });
  work = work.replace(FILE_RE, (m, p: string) => {
    const clean = p.replace(/[.,;:!?'")\]]+$/, "");
    // lien = chemin sans ponctuation finale ; le reste (:ligne, ponct.) reste dehors
    return `<span class="flink" data-path="${q(clean)}">${clean}</span>${m.slice(clean.length)}`;
  });
  return work.replace(/\x00U(\d+)\x00/g, (_m, i: string) => urls[+i]);
}

/** Colorise les lignes de diff (+add / -del / @@ hunks) dans le texte. */
function diffFmt(escaped: string): string {
  if (!/^(?:\+{3}|-{3}|@@|[-+]\S)/m.test(escaped)) return escaped;
  return escaped
    .split("\n")
    .map((l) => {
      const cls =
        l.startsWith("@@") || l.startsWith("+++") || l.startsWith("---")
          ? "df-hd"
          : l.startsWith("+")
            ? "df-add"
            : l.startsWith("-")
              ? "df-del"
              : "";
      return cls ? `<span class="${cls}">${l}</span>` : l;
    })
    .join("\n");
}

/** Coloration syntaxique légère par extension (kw/str/com/num). */
const SYN_KWS: Record<string, string> = {
  rs: "fn let mut pub mod use impl struct enum trait match loop where dyn ref box unsafe const static type as in for while if else return break continue move self Self super crate true false",
  py: "def class import from as if elif else for while return pass with try except finally raise yield lambda global nonlocal assert is in not and or None True False self",
  ts: "function const let var if else for while of in return break continue new class extends import export from as async await try catch finally throw switch case default typeof instanceof delete void this null undefined true false public private protected readonly interface type enum implements satisfies",
  sh: "if then else elif fi for while do done case esac function in echo exit return local export source readonly unset shift",
  c: "auto break case char const continue default do double else enum extern float for goto if int long register return short signed sizeof static struct switch typedef union unsigned void volatile while bool inline restrict",
  java: "abstract boolean break byte case catch char class const continue default do double else enum extends final finally float for if implements import instanceof int interface long native new package private protected public return short static strictfp super switch synchronized this throw throws transient try void volatile while true false null var record sealed permits",
};
const SYN_EXT: Record<string, string> = {
  rs: "rs", py: "py", ts: "ts", tsx: "ts", js: "ts", jsx: "ts", mjs: "ts",
  sh: "sh", bash: "sh", zsh: "sh",
  c: "c", h: "c", cpp: "c", cxx: "c", cc: "c", hpp: "c", hxx: "c",
  java: "java", kt: "java", kts: "java",
};
function highlight(code: string, ext: string): string {
  const lang = SYN_EXT[ext] ?? "";
  const kws = (SYN_KWS[lang] ?? "").split(" ").filter(Boolean);
  const comSrc =
    lang === "rs" || lang === "ts" || lang === "c" || lang === "java"
      ? "\\/\\/[^\\n]*"
      : "#[^\\n]*";
  const strSrc = '"[^"\\n]*"|\'[^\'\\n]*\'|`[^`\\n]*`';
  const kwSrc = kws.length ? `\\b(?:${kws.join("|")})\\b` : "a^"; // a^ = jamais
  const re = new RegExp(`(${comSrc})|(${strSrc})|(${kwSrc})|(\\b\\d+(?:\\.\\d+)?\\b)`, "g");
  return esc(code).replace(
    re,
    (m, c, s, k, n) =>
      c ? `<span class="syn-com">${m}</span>`
      : s ? `<span class="syn-str">${m}</span>`
      : k ? `<span class="syn-kw">${m}</span>`
      : `<span class="syn-num">${m}</span>`,
  );
}

/** Rendu markdown minimal et sûr : échappement HTML d'abord, blocs de code
 *  et liens extraits en placeholders (ni linkify ni inline-md ne les corrompt). */
function mdRender(src: string): string {
  const blocks: string[] = [];
  const stash = (html: string) => `\x00B${blocks.push(html) - 1}\x00`;

  // 1) blocs ```lang (fermé ou non — fin de texte tolérée) + code inline `x`
  let work = src
    .replace(/```(\w*)[ \t]*\n?([\s\S]*?)(?:```|$)/g, (_m, lang: string, code: string) =>
      stash(`<pre class="md-code">${highlight(code.replace(/\n$/, ""), lang.toLowerCase())}</pre>`))
    .replace(/`([^`\n]+)`/g, (_m, c: string) => stash(`<code class="md-ic">${esc(c)}</code>`));

  // 2) échappement du reste
  work = esc(work);

  // 3) images ![alt](https://…) avant les liens (le ![…] matcherait […](…))
  work = work.replace(
    /!\[([^\]]*)\]\((https?:\/\/[^)\s]+)\)/g,
    (_m, alt: string, url: string) =>
      inlineImages()
        ? stash(
            `<a href="${q(url)}" target="_blank" rel="noopener noreferrer">` +
              `<img class="md-img" src="${q(url)}" alt="${q(alt)}" loading="lazy"></a>`,
          )
        : stash(
            `<a href="${q(url)}" target="_blank" rel="noopener noreferrer">${alt || url}</a>`,
          ),
  );

  // 4) liens markdown [t](https://…) → placeholder (avant linkify)
  work = work.replace(
    /\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g,
    (_m, tx: string, url: string) =>
      stash(`<a href="${q(url)}" target="_blank" rel="noopener noreferrer">${tx}</a>`),
  );

  // 5) inline : gras / italique / barré
  work = work
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\b__([^_]+)__\b/g, "<strong>$1</strong>")
    .replace(/(^|[^\w*])\*([^*\n]+)\*(?![*\w])/g, "$1<em>$2</em>")
    .replace(/(?<![\w_])_([^_\n]+)_(?![\w_])/g, "<em>$1</em>")
    .replace(/~~([^~]+)~~/g, "<del>$1</del>");

  // 6) chemins de fichiers cliquables (hors blocs extraits)
  work = linkify(work);

  // 7) structure de blocs : headers, hr, citations, tables, listes, paragraphes
  const lines = work.split("\n");
  const isBlockStart = (l: string) =>
    /^\x00B\d+\x00\s*$/.test(l.trim()) ||
    /^#{1,6}\s/.test(l) ||
    /^\s*(?:\*{3,}|-{3,}|_{3,})$/.test(l) ||
    /^&gt;/.test(l) ||
    /^\s*(?:[-*+]|\d+[.)])\s+/.test(l);
  const cellSplit = (l: string) =>
    l.replace(/^\s*\||\|\s*$/g, "").split("|").map((c) => c.trim());
  const out: string[] = [];
  let i = 0;
  while (i < lines.length) {
    const l = lines[i];
    if (!l.trim()) { i++; continue; }
    if (/^\x00B\d+\x00\s*$/.test(l.trim())) { out.push(l.trim()); i++; continue; }
    const h = /^(#{1,6})\s+(.*)/.exec(l);
    if (h) { out.push(`<div class="mdh mdh${h[1].length}">${h[2]}</div>`); i++; continue; }
    if (/^\s*(?:\*{3,}|-{3,}|_{3,})$/.test(l)) { out.push("<hr>"); i++; continue; }
    if (/^&gt;/.test(l)) {
      const q: string[] = [];
      while (i < lines.length && /^&gt;/.test(lines[i])) {
        q.push(lines[i].replace(/^&gt;\s?/, ""));
        i++;
      }
      out.push(`<blockquote>${q.join("<br>")}</blockquote>`);
      continue;
    }
    // table : ligne |a|b| suivie d'une ligne |---|
    if (l.includes("|") && i + 1 < lines.length && /^\s*\|?[\s:|-]+\|?[\s:|-]*$/.test(lines[i + 1]) && lines[i + 1].includes("-") && lines[i + 1].includes("|")) {
      const hdr = cellSplit(l);
      i += 2;
      const rows: string[][] = [];
      while (i < lines.length && lines[i].includes("|") && lines[i].trim()) {
        rows.push(cellSplit(lines[i]));
        i++;
      }
      out.push(
        `<table class="mdt"><thead><tr>${hdr.map((c) => `<th>${c}</th>`).join("")}</tr></thead>` +
          `<tbody>${rows.map((r) => `<tr>${r.map((c) => `<td>${c}</td>`).join("")}</tr>`).join("")}</tbody></table>`,
      );
      continue;
    }
    const lm = /^(\s*)([-*+]|\d+[.)])\s+(.*)/.exec(l);
    if (lm) {
      const ordered = /^\d/.test(lm[2]);
      const items: string[] = [];
      while (i < lines.length) {
        const m2 = /^(\s*)([-*+]|\d+[.)])\s+(.*)/.exec(lines[i]);
        if (!m2) break;
        items.push(`<li${m2[1].length >= 2 ? ' class="md-sub"' : ""}>${m2[3]}</li>`);
        i++;
      }
      out.push(`<${ordered ? "ol" : "ul"} class="mdl">${items.join("")}</${ordered ? "ol" : "ul"}>`);
      continue;
    }
    const para: string[] = [];
    while (i < lines.length && lines[i].trim() && !isBlockStart(lines[i])) {
      para.push(diffFmt(lines[i]));
      i++;
    }
    out.push(`<p>${para.join("<br>")}</p>`);
  }
  // Placeholders imbriqués ([![](img)](url)) : la valeur insérée peut contenir
  // un \x00B — résoudre jusqu'à point fixe (index invalide : laissé tel quel).
  let html = out.join("");
  for (;;) {
    const next = html.replace(/\x00B(\d+)\x00/g, (m, n: string) =>
      +n < blocks.length ? blocks[+n] : m,
    );
    if (next === html) return html;
    html = next;
  }
}

async function openFilePanel(path: string) {
  if (!current) return;
  if ($("file-panel").hidden) { $("file-panel").hidden = false; navPush("files"); }
  // Android : le viewer glisse par-dessus la liste (navigation plein écran).
  if (IS_ANDROID) {
    $("file-viewer").classList.add("open");
    $("fv-name").textContent = path.split("/").pop() ?? path;
    navPush("fileview");
  }
  $("file-path").textContent = path;
  $("file-path").title = path;
  $("file-view").textContent = "…";
  // Marque le noeud de l'arborescence correspondant
  for (const n of $("file-tree").querySelectorAll<HTMLElement>(".fnode")) {
    n.classList.toggle("sel", n.dataset.rel === path);
  }
  try {
    const r = await call<{ content: string; truncated: boolean; path: string }>("read_file", {
      session: current.id,
      path,
    });
    $("file-path").title = r.path;
    const ext = /\.([a-zA-Z0-9]+)$/.exec(path)?.[1]?.toLowerCase() ?? "";
    $("file-view").innerHTML =
      highlight(r.content, ext) + (r.truncated ? "\n… (tronqué)" : "");
  } catch (e) {
    $("file-view").textContent = String(e);
  }
}
function closeFilePanel() {
  // Le viewer glissé par-dessus doit être dépilé aussi (pile back propre).
  navClose("fileview");
  $("file-viewer").classList.remove("open");
  navClose("files");
  $("file-panel").hidden = true;
}
$("file-close").onclick = closeFilePanel;
// Retour viewer → liste (Android)
$("fv-back").onclick = () => {
  navClose("fileview");
  $("file-viewer").classList.remove("open");
};
// Redimensionnement du panneau par son bord gauche
{
  const panel = $("file-panel");
  const hd = $("file-resize");
  hd.addEventListener("mousedown", (e) => {
    e.preventDefault();
    const move = (ev: MouseEvent) => {
      const w = Math.min(
        Math.max(window.innerWidth - ev.clientX, 240),
        window.innerWidth - 120,
      );
      panel.style.width = `${w}px`;
      localStorage.setItem("ac-filepanel-w", String(w));
    };
    const up = () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  });
  const saved = Number(localStorage.getItem("ac-filepanel-w"));
  if (saved > 240) panel.style.width = `${saved}px`;
}
eventsEl.addEventListener("click", (e) => {
  const el = (e.target as HTMLElement).closest<HTMLElement>(".flink");
  if (el?.dataset.path) void openFilePanel(el.dataset.path);
});

// ---------------------------------------------------------------------------
// Explorateur de fichiers du workspace (list_dir/read_file/delete_path)
// ---------------------------------------------------------------------------

interface DirEntry { name: string; dir: boolean; size: number }

const fileCtx = $("fctx");
let treeOpen = false;

const fmtSize = (n: number) =>
  n >= 1 << 20 ? `${(n / (1 << 20)).toFixed(1)} MB`
  : n >= 1024 ? `${(n / 1024).toFixed(0)} kB`
  : `${n} B`;

async function copyText(s: string) {
  try {
    await navigator.clipboard.writeText(s);
  } catch {
    const ta = document.createElement("textarea");
    ta.value = s;
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
}

/** Nœud de l'arbre : dossier (pliable) ou fichier (ouvre le viewer). */
function treeNode(rel: string, name: string, en: DirEntry, depth: number, abs: string): HTMLElement {
  const d = document.createElement("div");
  d.className = "fnode" + (en.dir ? " is-dir" : "") + (name.startsWith(".") ? " fdot" : "");
  d.style.paddingLeft = `${6 + depth * 14}px`;
  d.dataset.rel = rel;
  d.dataset.abs = abs;
  d.innerHTML =
    `<span class="farrow">${en.dir ? "\u25b8" : ""}</span>` +
    `<span class="fico">${en.dir ? "\u{1f4c1}" : "\u{1f4c4}"}</span>` +
    `<span class="fname">${esc(name)}</span>` +
    (en.dir ? "" : `<span class="fsize">${fmtSize(en.size)}</span>`);
  d.title = abs;
  if (en.dir) {
    d.onclick = () => toggleDir(d);
  } else {
    d.onclick = () => openFilePanel(rel);
  }
  bindCtx(d, (x, y) => openFileCtx(x, y, d));
  return d;
}

/** Ouvre/replie un dossier — chargement paresseux à chaque expansion
 *  (le contenu change pendant que l'agent travaille, pas de cache). */
async function toggleDir(el: HTMLElement) {
  const arrow = el.querySelector<HTMLElement>(".farrow")!;
  const next = el.nextElementSibling;
  if (next?.classList.contains("fchildren")) {
    next.remove();
    arrow.textContent = "\u25b8";
    return;
  }
  const rel = el.dataset.rel ?? "";
  arrow.textContent = "…";
  const box = document.createElement("div");
  box.className = "fchildren";
  try {
    const r = await call<{ dir: string; entries: DirEntry[] }>("list_dir", {
      session: current!.id,
      path: rel,
    });
    if (!r.entries.length) {
      box.innerHTML = `<div class="fempty">${t("tree_empty")}</div>`;
    }
    const depth = rel ? rel.split("/").length : 0;
    for (const en of r.entries) {
      const childRel = rel ? `${rel}/${en.name}` : en.name;
      box.appendChild(treeNode(childRel, en.name, en, depth, `${r.dir}/${en.name}`));
    }
  } catch (e) {
    box.innerHTML = `<div class="ferr">${esc(String(e))}</div>`;
  }
  if (el.isConnected) {
    el.after(box);
    arrow.textContent = "\u25be";
  }
}

/** Racine = cwd canonique de la session (renvoyé par list_dir ""). */
async function loadTreeRoot() {
  const tree = $("file-tree");
  tree.innerHTML = "";
  if (!current) return;
  try {
    const r = await call<{ dir: string; entries: DirEntry[] }>("list_dir", {
      session: current.id,
      path: "",
    });
    const hd = document.createElement("div");
    hd.className = "froot";
    hd.innerHTML = `<span class="fico">\u{1f4c1}</span><span class="fname">${esc(r.dir)}</span>`;
    hd.title = r.dir;
    tree.appendChild(hd);
    for (const en of r.entries) {
      tree.appendChild(treeNode(en.name, en.name, en, 0, `${r.dir}/${en.name}`));
    }
    if (!r.entries.length) {
      tree.insertAdjacentHTML("beforeend", `<div class="fempty">${t("tree_empty")}</div>`);
    }
  } catch (e) {
    tree.innerHTML = `<div class="ferr">${esc(String(e))}</div>`;
  }
}

function setTreeVisible(v: boolean) {
  treeOpen = v;
  $("file-tree").hidden = !v;
  $("btn-tree-refresh").hidden = !v;
  if (v && !$("file-panel").hidden) void loadTreeRoot();
}

// 📁 dans le header : ouvre le panneau complet (arbre + viewer)
$("btn-files").onclick = () => {
  const p = $("file-panel");
  const opening = p.hidden;
  if (opening) {
    p.hidden = false;
    navPush("files");
    // L'arbre + le viewer ont besoin de largeur (desktop seulement :
    // sur Android le panneau est fixe plein écran, pas de width inline).
    if (!IS_ANDROID) {
      const w = Math.max(Number(p.style.width.replace("px", "")) || 0, 620);
      p.style.width = `${Math.min(w, window.innerWidth - 120)}px`;
    }
    setTreeVisible(true);
  } else {
    closeFilePanel();
  }
};
// 🗂 dans le panneau : bascule juste la colonne arborescence
$("btn-tree").onclick = () => setTreeVisible($("file-tree").hidden);
$("btn-tree-refresh").onclick = () => void loadTreeRoot();

function hideFileCtx() {
  navClose("fctx");
  fileCtx.hidden = true;
}
/** Menu contextuel clic droit : copier contenu / chemin absolu / supprimer.
 *  Android : rendu en action-sheet ancré en bas (pas de positionnement x/y). */
function openFileCtx(x: number, y: number, el: HTMLElement) {
  const rel = el.dataset.rel ?? "";
  const abs = el.dataset.abs ?? rel;
  const isDir = el.classList.contains("is-dir");
  fileCtx.innerHTML = `<div class="fctx-path">${esc(abs)}</div>`;
  const add = (label: string, cls: string, fn: (b: HTMLButtonElement) => void) => {
    const b = document.createElement("button");
    b.textContent = label;
    b.className = cls;
    b.onclick = () => fn(b);
    fileCtx.appendChild(b);
    return b;
  };
  if (!isDir) {
    add(t("ctx_copy"), "", () => {
      hideFileCtx();
      void call<{ content: string }>("read_file", { session: current!.id, path: rel })
        .then((r) => copyText(r.content))
        .catch((e) => showErr(String(e)));
    });
  }
  add(t("ctx_copy_path"), "", () => {
    hideFileCtx();
    void copyText(abs);
  });
  add(t("ctx_download"), "", () => {
    hideFileCtx();
    void downloadEntry(rel);
  });
  add(t("ctx_delete"), "danger", (b) => {
    // Double-clic armé : "Supprimer" → "confirmer ?" → suppression
    if (!b.dataset.armed) {
      b.dataset.armed = "1";
      b.textContent = t("ctx_confirm");
      return;
    }
    hideFileCtx();
    void call("delete_path", { session: current!.id, path: rel })
      .then(() => {
        const nx = el.nextElementSibling;
        if (nx?.classList.contains("fchildren")) nx.remove();
        el.remove();
      })
      .catch((e) => showErr(String(e)));
  });
  fileCtx.hidden = false;
  navPush("fctx");
  if (!IS_ANDROID) {
    const r = fileCtx.getBoundingClientRect();
    fileCtx.style.left = `${Math.min(x, window.innerWidth - r.width - 8)}px`;
    fileCtx.style.top = `${Math.min(y, window.innerHeight - r.height - 8)}px`;
  }
}

/** Télécharge un fichier/dossier du workspace vers le disque local :
 *  infos (nom suggéré — .zip pour un dossier) → dialogue « Enregistrer
 *  sous » (Android : Download/AgentControl/ → repli app data) → tranches
 *  base64 démon → write_download. Session/conn figées : un changement de
 *  session en cours de route n'envoie pas les tranches sur le mauvais
 *  serveur (la sysline disparaît au switch, le téléchargement finit). */
async function downloadEntry(rel: string) {
  if (!current) return;
  const sid = current.id;
  const conn = current.conn;
  const info = await call<{ name: string; dir: boolean; size: number | null }>(
    "download_path",
    { session: sid, conn, path: rel, offset: 0, limit: 0 },
  ).catch((e) => {
    showErr(String(e));
    return null;
  });
  if (!info) return;
  let dest: string | null = IS_ANDROID
    ? `${await downloadDir()}/AgentControl/${info.name}`
    : await save({ defaultPath: info.name, title: t("ctx_download") });
  if (!dest) return; // dialogue annulé
  const sys = document.createElement("div");
  sys.className = "ev sysline";
  sys.textContent = `⏳ ${t("dl_progress", { name: info.name, pct: 0 })}`;
  eventsEl.appendChild(sys);
  scrollBottom();
  let offset = 0;
  let archive: string | undefined;
  try {
    for (;;) {
      const c = await call<{ total: number; data: string; archive?: string }>(
        "download_path",
        { session: sid, conn, path: rel, offset, limit: 1 << 20, archive },
      );
      archive ??= c.archive;
      const n = await invoke<number>("write_download", {
        path: dest,
        data: c.data,
        append: offset > 0,
      }).catch(async (e): Promise<number> => {
        // Android : Download public peut refuser (pas de SAF) → app data.
        if (!IS_ANDROID || offset > 0) throw e;
        dest = `${await appDataDir()}/downloads/${info.name}`;
        return invoke<number>("write_download", { path: dest, data: c.data, append: false });
      });
      offset += n;
      sys.textContent = `⏳ ${t("dl_progress", {
        name: info.name,
        pct: Math.round((offset * 100) / Math.max(c.total, 1)),
      })}`;
      if (offset >= c.total) break;
    }
    sys.textContent = `✔ ${t("dl_done", { name: info.name, dest })}`;
  } catch (e) {
    sys.textContent = `⚠ ${t("dl_failed", { name: info.name, err: String(e) })}`;
    showErr(String(e));
  }
}
// Le menu se ferme au clic ailleurs / Échap / scroll de l'arbre — mais un
// clic DANS le menu ne doit pas le fermer avant l'action (delete armé).
fileCtx.addEventListener("click", (e) => e.stopPropagation());
window.addEventListener("click", hideFileCtx);
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") hideFileCtx();
});
$("file-tree").addEventListener("scroll", hideFileCtx);

function mkEv(cls: string, seq: number): HTMLElement {
  const d = document.createElement("div");
  d.className = `ev ${cls}`;
  d.dataset.seq = String(seq);
  return d;
}

function appendEvent(ev: AcEvent, live: boolean) {
  // tool_call_update modifie le bloc existant
  if (ev.type === "tool_call_update") {
    updateToolBlock(ev);
    return;
  }
  const nearBottom =
    eventsEl.scrollHeight - eventsEl.scrollTop - eventsEl.clientHeight < 120;
  const node = renderEvent(ev, live);
  eventsEl.appendChild(node);
  renderedCount++;
  if (ev.seq < minRendered) minRendered = ev.seq;
  if (ev.type === "tool_call") {
    toolBlocks.set(ev.call_id as string, node);
  }
  if (live && nearBottom) scrollBottom();
  if (live && !nearBottom) {
    unseenBelow++;
    updateBottomBtn();
  }
  trimRendered();
}

/** Supprime les plus anciens events rendus au-delà du cap (buffer). */
function trimRendered() {
  while (renderedCount > MAX_RENDERED) {
    const first = eventsEl.querySelector<HTMLElement>(".ev");
    if (!first) break;
    // retirer aussi son bloc de la map si tool_call
    for (const [k, v] of toolBlocks) if (v === first) toolBlocks.delete(k);
    first.remove();
    renderedCount--;
    minRendered = Number(
      eventsEl.querySelector<HTMLElement>(".ev")?.dataset.seq ?? Number.MAX_SAFE_INTEGER,
    );
  }
}

function prependEvents(events: AcEvent[]) {
  if (!events.length) {
    hasMoreHistory = false;
    return;
  }
  const oldHeight = eventsEl.scrollHeight;
  for (const ev of events) {
    const node = renderEvent(ev, false);
    eventsEl.insertBefore(node, eventsEl.children[1]); // après sentinel
    renderedCount++;
    if (ev.seq < minRendered) minRendered = ev.seq;
    if (ev.type === "tool_call") toolBlocks.set(ev.call_id as string, node);
    if (ev.type === "tool_call_update") updateToolBlock(ev);
  }
  // garder la position visuelle
  eventsEl.scrollTop += eventsEl.scrollHeight - oldHeight;
  updateBottomBtn();
}

function renderEvent(ev: AcEvent, live: boolean): HTMLElement {
  switch (ev.type) {
    case "user_message": {
      const d = mkEv("msg user", ev.seq);
      d.innerHTML =
        (ev.steer ? `<span class="steer-badge">${t("steer_badge")}</span> ` : "") +
        linkify(esc(ev.text as string));
      return d;
    }
    case "agent_text": {
      const d = mkEv("msg agent", ev.seq);
      d.innerHTML = `<div class="md">${mdRender(ev.text as string)}</div>`;
      return d;
    }
    case "peer_message": {
      // Message agent→agent : journalisé dans les deux sessions — la
      // direction se déduit de from_session vs la session ouverte.
      const outgoing = current?.id === ev.from_session;
      const peerId = (outgoing ? ev.to_session : ev.from_session) as string;
      const d = mkEv(`msg peer ${outgoing ? "out" : "in"}`, ev.seq);
      const label = outgoing
        ? `→ ${t("peer_to")} «${ev.to_title}»`
        : `💬 ${t("peer_from")} «${ev.from_title}»`;
      d.innerHTML =
        `<div class="peer-head" title="${esc(peerId)}">${esc(label)}</div>` +
        `<div class="peer-body">${linkify(esc(ev.text as string))}</div>`;
      d.querySelector(".peer-head")?.addEventListener("click", () => openSession(peerId));
      return d;
    }
    case "agent_thought": {
      const d = mkEv("thought", ev.seq);
      const txt = ev.text as string;
      const head = txt.trim().split("\n")[0].slice(0, 110);
      d.innerHTML = `<details class="th"><summary>💭 ${esc(head)}</summary><div class="md th-body">${mdRender(txt)}</div></details>`;
      return d;
    }
    case "tool_call":
      return renderToolCall(ev);
    case "permission_request":
      return renderPermission(ev);
    case "question_request":
      return renderQuestion(ev);
    case "permission_resolved":
    case "question_resolved": {
      const d = mkEv("resolved", ev.seq);
      const rid = ev.request_id as string;
      answered.add(rid);
      d.textContent = t("resolved") + ((ev.outcome ?? ev.answer) as string);
      const card = eventsEl.querySelector(`[data-req="${rid}"]`);
      if (card) card.classList.add("done");
      return d;
    }
    case "mode_changed": {
      const d = mkEv("sysline", ev.seq);
      d.textContent = t("mode_arrow") + (ev.mode as string);
      if (current) {
        current.mode = ev.mode as string;
        updateHeader();
      }
      return d;
    }
    case "config_options": {
      if (current) {
        current.config_options = ev.options as ConfigOption[];
        const mo = current.config_options.find((o) => o.id === "model" || o.category === "model");
        if (mo) current.model = mo.currentValue;
        const md = current.config_options.find((o) => o.id === "mode" || o.category === "mode");
        if (md) current.mode = md.currentValue;
        updateHeader();
      }
      const d = mkEv("sysline", ev.seq);
      d.hidden = true;
      return d;
    }
    case "usage_update": {
      // Jauge contexte — pas rendue comme message ; met à jour le badge
      // du header (replay : la dernière valeur de la page gagne).
      if (current) {
        current.context_used = ev.used as number;
        current.context_size = ev.size as number;
        updateCtx();
      }
      const d = mkEv("sysline", ev.seq);
      d.hidden = true;
      return d;
    }
    case "commands_available": {
      const d = mkEv("sysline", ev.seq);
      const cmds = ev.commands as CmdInfo[];
      d.textContent = t("cmds_avail", { n: cmds.length });
      if (current) {
        current.available_commands = cmds;
        updateHeader();
      }
      return d;
    }
    case "status_changed": {
      const d = mkEv("sysline", ev.seq);
      d.textContent = t("state_arrow") + (ev.state as string);
      // Seul un event LIVE met à jour l'état — rejouer l'historique du
      // journal écraserait alive/meta avec un état périmé ("working"
      // d'une session morte → bouton Reprendre masqué).
      if (live && current) {
        current.state = ev.state as string;
        alive = ev.state !== "dead";
        updateHeader();
        refreshSessions();
      }
      return d;
    }
    case "error": {
      const d = mkEv("errline", ev.seq);
      d.textContent = `⚠ ${ev.message}`;
      return d;
    }
    case "system": {
      const d = mkEv("sysline", ev.seq);
      d.textContent = `⦿ ${ev.text}`;
      return d;
    }
    default: {
      const d = mkEv("sysline", ev.seq);
      d.textContent = ev.type;
      return d;
    }
  }
}

// ---------------------------------------------------------------------------
// Tool calls : bloc pliable + expand blob
// ---------------------------------------------------------------------------

function renderToolCall(ev: AcEvent): HTMLElement {
  const d = mkEv("tool", ev.seq);
  const status = ev.status as string;
  d.dataset.callId = ev.call_id as string;
  d.innerHTML = `
    <div class="tool-head">
      <span class="tool-ico">⚙</span>
      <span class="tool-title">${esc(ev.title as string)}</span>
      <span class="badge st-${status}">${status}</span>
    </div>
    <div class="tool-body" hidden>
      <pre class="tool-out"></pre>
      ${ev.blob ? '<button class="btn-more">charger la sortie complète…</button>' : ""}
      ${ev.diff ? `<pre class="tool-diff">${esc(ev.diff as string)}</pre>` : ""}
    </div>`;
  const body = d.querySelector<HTMLElement>(".tool-body")!;
  const pre = d.querySelector<HTMLElement>(".tool-out")!;
  if (ev.excerpt) pre.textContent = ev.excerpt as string;
  d.querySelector(".tool-head")!.addEventListener("click", () => {
    body.hidden = !body.hidden;
    d.classList.toggle("open", !body.hidden);
  });
  const btn = d.querySelector<HTMLButtonElement>(".btn-more");
  if (btn && ev.blob) {
    btn.addEventListener("click", async () => {
      btn.disabled = true;
      btn.textContent = t("loading");
      const r = await call<{ data: string; total: number }>("get_blob", {
        session: current!.id,
        blob: ev.blob,
        offset: 0,
        limit: 1 << 20,
      });
      pre.textContent = r.data + (r.total > r.data.length ? `\n… (${r.total} o total)` : "");
      btn.remove();
    });
  }
  return d;
}

function updateToolBlock(ev: AcEvent) {
  const d = toolBlocks.get(ev.call_id as string);
  if (!d) return;
  const badge = d.querySelector<HTMLElement>(".tool-head .badge");
  if (badge && ev.status) {
    badge.textContent = ev.status as string;
    badge.className = `badge st-${ev.status}`;
  }
  const body = d.querySelector<HTMLElement>(".tool-body")!;
  const pre = d.querySelector<HTMLElement>(".tool-out")!;
  if (ev.excerpt) {
    pre.textContent += ev.excerpt as string;
    if (body.hidden) {
      // signaler du nouveau contenu sans ouvrir
      d.querySelector(".tool-head")!.classList.add("has-new");
    }
  }
  if (ev.blob && !d.querySelector(".btn-more")) {
    const b = document.createElement("button");
    b.className = "btn-more";
    b.textContent = t("load_full");
    b.addEventListener("click", async () => {
      b.disabled = true;
      const r = await call<{ data: string; total: number }>("get_blob", {
        session: current!.id,
        blob: ev.blob,
        offset: 0,
        limit: 1 << 20,
      });
      pre.textContent = r.data + (r.total > r.data.length ? `\n… (${r.total} o total)` : "");
      b.remove();
    });
    body.appendChild(b);
  }
}

// ---------------------------------------------------------------------------
// Permissions & questions
// ---------------------------------------------------------------------------

function renderPermission(ev: AcEvent): HTMLElement {
  const d = mkEv("perm", ev.seq);
  d.dataset.req = ev.request_id as string;
  const opts = (ev.options as { option_id: string; name: string; kind: string }[]) ?? [];
  const kind = ev.kind as string | undefined;
  const detail = ev.detail as string | undefined;
  d.innerHTML =
    `<div class="perm-title">🔐 ${esc(ev.title as string)}` +
    (kind ? ` <span class="perm-kind">${esc(kind)}</span>` : "") +
    `</div>` +
    (detail ? `<pre class="perm-detail">${esc(detail)}</pre>` : "") +
    `<div class="perm-opts"></div>`;
  const box = d.querySelector<HTMLElement>(".perm-opts")!;
  for (const o of opts) {
    const b = document.createElement("button");
    b.textContent = o.name || o.option_id;
    b.className = o.kind.startsWith("reject") ? "btn-reject" : "btn-allow";
    b.onclick = async () => {
      if (answered.has(ev.request_id as string)) return;
      await call("respond", {
        session: current!.id,
        request_id: ev.request_id,
        option_id: o.option_id,
      }).catch((e) => showErr(String(e)));
      d.classList.add("done");
    };
    box.appendChild(b);
  }
  if (answered.has(ev.request_id as string)) d.classList.add("done");
  return d;
}

function renderQuestion(ev: AcEvent): HTMLElement {
  const d = mkEv("perm question", ev.seq);
  d.dataset.req = ev.request_id as string;
  d.innerHTML = `
    <div class="perm-title">❓ ${esc(ev.message as string)}</div>
    <div class="q-row">
      <input class="q-input" placeholder='${t("answer_ph")}' />
      <button class="btn-allow">${t("answer_btn")}</button>
      <button class="btn-reject">${t("ignore_btn")}</button>
    </div>`;
  const input = d.querySelector<HTMLInputElement>(".q-input")!;
  const answer = async (val: string | null) => {
    if (answered.has(ev.request_id as string)) return;
    let parsed: unknown = null;
    if (val) {
      try {
        parsed = JSON.parse(val);
      } catch {
        parsed = { answer: val };
      }
    }
    await call("respond", {
      session: current!.id,
      request_id: ev.request_id,
      answer: parsed,
    }).catch((e) => showErr(String(e)));
    d.classList.add("done");
  };
  d.querySelector<HTMLButtonElement>(".btn-allow")!.onclick = () => answer(input.value || "{}");
  d.querySelector<HTMLButtonElement>(".btn-reject")!.onclick = () => answer(null);
  if (answered.has(ev.request_id as string)) d.classList.add("done");
  return d;
}

// ---------------------------------------------------------------------------
// Scroll : chargement des pages anciennes via sentinel
// ---------------------------------------------------------------------------

const observer = new IntersectionObserver(async (entries) => {
  if (!entries[0].isIntersecting || loadingOld || !current || !hasMoreHistory) return;
  loadingOld = true;
  try {
    const page = await call<{ events: AcEvent[] }>("events", {
      session: current.id,
      before_seq: minRendered,
      limit: PAGE,
    });
    prependEvents(page.events);
  } finally {
    loadingOld = false;
  }
});
observer.observe(sentinel);

function scrollBottom() {
  eventsEl.scrollTop = eventsEl.scrollHeight;
  updateBottomBtn();
}

// Bouton flottant "↓" quand on n'est pas en bas ; compteur d'events non vus.
let unseenBelow = 0;
function updateBottomBtn() {
  const dist = eventsEl.scrollHeight - eventsEl.scrollTop - eventsEl.clientHeight;
  const b = $("btn-bottom");
  if (dist < 120) {
    b.hidden = true;
    unseenBelow = 0;
    return;
  }
  b.hidden = false;
  b.textContent = unseenBelow ? `↓ ${unseenBelow}` : "↓";
}
eventsEl.addEventListener("scroll", updateBottomBtn);
$("btn-bottom").onclick = () => {
  eventsEl.scrollTo({ top: eventsEl.scrollHeight, behavior: "smooth" });
};

// ---------------------------------------------------------------------------
// Push events du démon
// ---------------------------------------------------------------------------

await listen<{ session: string; event: AcEvent }>("session-event", (msg) => {
  const { session, event } = msg.payload;
  // Suppression côté serveur : l'event terminal arrive avant la coupure.
  if (event.type === "session_deleted") {
    if (current && session === current.id) markSessionGone();
    refreshSessions();
    return;
  }
  if (current && session === current.id) {
    appendEvent(event, true);
  }
  if (event.type === "status_changed") refreshSessions();
});

// ---------------------------------------------------------------------------
// Actions UI
// ---------------------------------------------------------------------------

$("btn-send").onclick = () => sendInput("normal");
$("btn-steer").onclick = () => sendInput("steer");
$("input").addEventListener("keydown", (e) => {
  const ac = $("cmd-ac");
  if (!ac.hidden) {
    const sel = ac.querySelector<HTMLElement>(".ac-item.sel");
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      moveAcSel(e.key === "ArrowDown" ? 1 : -1);
      return;
    }
    if ((e.key === "Tab" || e.key === "Enter") && sel) {
      e.preventDefault();
      applyAc(sel.dataset.name!);
      return;
    }
    if (e.key === "Escape") {
      ac.hidden = true;
      return;
    }
  }
  if (e.key === "Enter" && e.ctrlKey) {
    e.preventDefault();
    sendInput("force"); // Ctrl+Entrée = envoi forcé (annule le tour en cours)
    return;
  }
  if (e.key === "Enter" && e.altKey) {
    e.preventDefault();
    sendInput("steer"); // Alt+Entrée = injecter après le tool call courant
    return;
  }
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    sendInput("normal");
  }
});

// ---- Commandes built-in (toujours dispo, exécutées côté client) ----
const BUILTINS = (): { name: string; description: string; hint: string }[] => [
  { name: "model", description: t("builtin_model"), hint: "<model>" },
  { name: "resume", description: t("builtin_resume"), hint: "" },
  { name: "cancel", description: t("builtin_cancel"), hint: "" },
  { name: "fork", description: t("builtin_fork"), hint: "" },
];

async function runBuiltin(name: string, arg: string): Promise<boolean> {
  if (!current) return false;
  switch (name) {
    case "model":
      if (!arg) return false;
      await call("set_model", { session: current.id, model: arg });
      return true;
    case "resume":
      await openNativePicker();
      return true;
    case "cancel":
      await call("cancel", { session: current.id });
      return true;
    case "fork": {
      // Le daemon libère la session native, /fork via le TUI de l'agent,
      // puis attache la session forkée (même groupe, titre + "_fork").
      addSysline(t("fork_started"));
      const r = await call<{ session: string }>("fork_session", {
        session: current.id,
      });
      await refreshSessions();
      if (r.session) await openSession(r.session);
      return true;
    }
  }
  return false;
}

// ---- Autocomplete des slash-commands ----
let acSel = 0;

function moveAcSel(dir: number) {
  const items = [...$("cmd-ac").querySelectorAll<HTMLElement>(".ac-item")];
  if (!items.length) return;
  acSel = (acSel + dir + items.length) % items.length;
  items.forEach((el, i) => el.classList.toggle("sel", i === acSel));
  items[acSel].scrollIntoView({ block: "nearest" });
}

function applyAc(name: string) {
  const inp = $("input") as HTMLTextAreaElement;
  inp.value = "/" + name + " ";
  $("cmd-ac").hidden = true;
  inp.focus();
}

function refreshAc() {
  const inp = $("input") as HTMLTextAreaElement;
  const ac = $("cmd-ac");
  const v = inp.value;
  const m = /^\/([\w-]*)$/.exec(v);
  if (!m || !current) {
    ac.hidden = true;
    return;
  }
  const q = m[1].toLowerCase();
  const matches = [
    ...current.available_commands,
    ...BUILTINS(),
  ]
    .filter((c) => cmdName(c).toLowerCase().startsWith(q))
    .slice(0, 12);
  if (!matches.length) {
    ac.hidden = true;
    return;
  }
  ac.innerHTML = "";
  acSel = 0;
  matches.forEach((c, i) => {
    const n = cmdName(c);
    const el = document.createElement("div");
    el.className = "ac-item" + (i === 0 ? " sel" : "");
    el.dataset.name = n;
    const h = cmdHint(c);
    el.innerHTML = `<span class="n">/${esc(n)}</span>` +
      (h ? `<span class="h">${esc(h)}</span>` : "") +
      `<span class="d">${esc(cmdDesc(c))}</span>`;
    el.title = cmdDesc(c);
    el.onclick = () => applyAc(n);
    ac.appendChild(el);
  });
  ac.hidden = false;
}

$("input").addEventListener("input", refreshAc);
$("input").addEventListener("blur", () => {
  // délai pour laisser le click sur un item passer
  setTimeout(() => ($("cmd-ac").hidden = true), 150);
});

/** Dialogue : sessions natives de l'agent (ACP session/list) → reprise. */
async function openNativePicker() {
  if (!current) return;
  const box = $("native-list");
  box.innerHTML = "…";
  ($("dlg-resume") as HTMLDialogElement).showModal();
  try {
    const r = await call<{ sessions?: { sessionId: string; title?: string; updatedAt?: string; cwd?: string }[] }>(
      "list_native_sessions",
      { session: current.id },
    );
    const list = r.sessions ?? [];
    box.innerHTML = list.length
      ? ""
      : t("no_native");
    for (const s of list) {
      const row = document.createElement("div");
      row.className = "ac-item";
      row.innerHTML = `<span class="n">${esc(s.sessionId.slice(0, 12))}</span>` +
        `<span class="d">${esc(s.title || s.cwd || "")}</span>` +
        `<span class="h">${esc((s.updatedAt || "").slice(0, 16).replace("T", " "))}</span>`;
      row.onclick = async () => {
        ($("dlg-resume") as HTMLDialogElement).close();
        await call("resume_session", { session: current!.id, native_id: s.sessionId })
          .catch((e) => showErr(String(e)));
        await openSession(current!.id);
      };
      box.appendChild(row);
    }
  } catch (e) {
    // Agent mort → impossible de lister ; proposer la reprise de la session courante
    box.innerHTML = `<div class="errline">${esc(String(e))}</div>`;
    const row = document.createElement("div");
    row.className = "ac-item";
    row.innerHTML = `<span class="n">↻</span><span class="d">${t("resume_current")}</span>`;
    row.onclick = async () => {
      ($("dlg-resume") as HTMLDialogElement).close();
      await call("resume_session", { session: current!.id }).catch((er) => showErr(String(er)));
      await openSession(current!.id);
    };
    box.appendChild(row);
  }
}

// ---- File d'attente des prompts (queue_list/edit/drop/promote) ----
async function refreshQueue() {
  const bar = $("queue-bar");
  if (!current) {
    bar.hidden = true;
    return;
  }
  try {
    const r = await call<{ queue: string[] }>("queue_list", {
      session: current.id,
    });
    bar.innerHTML = "";
    bar.hidden = r.queue.length === 0;
    r.queue.forEach((txt, i) => {
      const d = document.createElement("div");
      d.className = "q-item";
      const sp = document.createElement("span");
      sp.className = "q-txt";
      sp.textContent = txt;
      sp.title = txt;
      d.appendChild(sp);
      // ✎ = reprendre dans le champ + retirer de la file
      const e = qBtn("✎", "Edit");
      e.onclick = () => {
        ($("input") as HTMLTextAreaElement).value = txt;
        call("queue_drop", { session: current!.id, index: i }).then(refreshQueue);
      };
      // ⏫ = tête de file + cancel → envoi forcé (reste de file préservée)
      const p = qBtn("⏫", "Send now");
      p.onclick = () =>
        call("queue_promote", { session: current!.id, index: i }).then(refreshQueue);
      const x = qBtn("✕", "Remove");
      x.onclick = () =>
        call("queue_drop", { session: current!.id, index: i }).then(refreshQueue);
      d.append(e, p, x);
      bar.appendChild(d);
    });
  } catch {
    bar.hidden = true;
  }
}
function qBtn(txt: string, title: string): HTMLButtonElement {
  const b = document.createElement("button");
  b.textContent = txt;
  b.title = title;
  return b;
}

// ---- /compact : la compaction tourne en arrière-plan côté agent après un
// idle immédiat — le démon passe la session en "compacting" et retient
// les prompts en file (visibles dans la barre de file) jusqu'au signal
// de fin (contexte réduit, message de fin, cancel, timeout). Un prompt
// livré pendant le compact l'annulerait.

function addSysline(text: string) {
  const d = document.createElement("div");
  d.className = "ev sysline";
  d.textContent = `⦿ ${text}`;
  eventsEl.appendChild(d);
  scrollBottom();
}

type SendMode = "normal" | "steer" | "force";

async function sendInput(mode: SendMode = "normal") {
  const txt = ($("input") as HTMLTextAreaElement).value.trim();
  if (!txt || !current) return;
  // Built-ins client (interceptent avant envoi à l'agent)
  const bm = /^\/(\w+)(?:\s+(.*))?$/.exec(txt);
  if (bm && BUILTINS().some((b) => b.name === bm[1])) {
    ($("input") as HTMLTextAreaElement).value = "";
    try {
      await runBuiltin(bm[1], (bm[2] ?? "").trim());
    } catch (e) {
      showErr(String(e));
    }
    return;
  }
  // Slash-commande ni builtin ni exposée par l'agent (available_commands) :
  // l'envoyer en texte brut coûte un tour pour rien (ex. /fork chez Devin =
  // TUI-only, jamais routé). Avertir localement. Liste vide = agent sans
  // annonce → on ne peut pas trancher, on laisse passer.
  if (
    bm &&
    current.available_commands.length > 0 &&
    !current.available_commands.some(
      (c) => cmdName(c).toLowerCase() === bm[1].toLowerCase(),
    )
  ) {
    ($("input") as HTMLTextAreaElement).value = "";
    addSysline(t("cmd_unknown", { cmd: bm[1] }));
    return;
  }
  ($("input") as HTMLTextAreaElement).value = "";
  await call("send", {
    session: current.id,
    text: txt,
    force: mode === "force",
    steer: mode === "steer",
  }).catch((e) => showErr(String(e)));
  // Commande slash non-builtin : l'agent l'applique au prochain tour (ex.
  // /compact → "context compacted" n'arrive qu'au message suivant) → hint.
  // (/compact : le démon arme la retenue "compacting" à la livraison.)
  if (bm) {
    addSysline(t("cmd_sent_hint", { cmd: bm[1] }));
  }
  refreshQueue();
}

$("btn-cancel").onclick = () =>
  current && call("cancel", { session: current.id }).catch((e) => showErr(String(e)));

$("btn-resume").onclick = () =>
  current &&
  call("resume_session", { session: current.id })
    .then(() => openSession(current!.id))
    .catch((e) => showErr(String(e)));

($("mode-select") as HTMLSelectElement).onchange = (e) => {
  if (!current) return;
  const sel = e.target as HTMLSelectElement;
  const req = sel.dataset.cfgid
    ? call("set_config_option", { session: current.id, config_id: sel.dataset.cfgid, value: sel.value })
    : call("set_mode", { session: current.id, mode: sel.value });
  req.catch((er) => showErr(String(er)));
};

/** Remplit le select d'effort pour une base de modèle donnée. */
function effortsFor(base: string): string[] {
  const opts: { v: string }[] = JSON.parse(($("model-select") as HTMLSelectElement).dataset.opts || "[]");
  const effs = new Set<string>();
  for (const o of opts) {
    if (o.v === base || o.v.startsWith(base + "-") || o.v.startsWith(base + "[")) {
      const e = splitModel(o.v)[1];
      if (e) effs.add(e);
    }
  }
  return [...effs];
}

function fillEfforts(base: string, cur: string) {
  const es = $("effort-select") as HTMLSelectElement;
  const effs = effortsFor(base);
  es.innerHTML = "";
  if (effs.length <= 1) return;
  for (const e of effs) {
    const o = document.createElement("option");
    o.value = e;
    // "[context=…,effort=…]" → contenu lisible sans les crochets
    // ("[]" de cursor = variante par défaut → garder "[]" plutôt que vide)
    o.textContent = e.startsWith("[") ? e.slice(1, -1) || e : e;
    es.appendChild(o);
  }
  if (effs.includes(cur)) es.value = cur;
}

function applyModelValue(v: string) {
  if (!current) return;
  const md = $("model-select") as HTMLSelectElement;
  const req = md.dataset.cfgid
    ? call("set_config_option", { session: current.id, config_id: md.dataset.cfgid, value: v })
    : call("set_model", { session: current.id, model: v });
  req.catch((er) => showErr(String(er)));
}

/** Combinaisons Fusion réellement annoncées par l'agent (liste fermée :
 *  composer librement fusion-A-eff-sidekick-B-eff renvoie Invalid params). */
function fusionTable() {
  const opts: { v: string; label: string }[] = JSON.parse(
    ($("model-select") as HTMLSelectElement).dataset.opts || "[]",
  );
  const valid = new Set<string>();
  const prims = new Map<string, Set<string>>();               // base primaire → efforts
  const sides = new Map<string, Map<string, Set<string>>>(); // "pb|pe" → sidekick → efforts
  for (const o of opts) {
    const p = fusionParts(o.v);
    if (!p) continue;
    valid.add(o.v);
    const [pb, pe, sb, se] = p;
    if (!prims.has(pb)) prims.set(pb, new Set());
    prims.get(pb)!.add(pe);
    const key = `${pb}|${pe}`;
    if (!sides.has(key)) sides.set(key, new Map());
    const m = sides.get(key)!;
    if (!m.has(sb)) m.set(sb, new Set());
    m.get(sb)!.add(se);
  }
  return { valid, prims, sides };
}

function fillSel(s: HTMLSelectElement, items: [string, string][]) {
  s.innerHTML = "";
  for (const [v, label] of items) {
    const o = document.createElement("option");
    o.value = v;
    o.textContent = label;
    s.appendChild(o);
  }
}

let fusionApplied = false;

/** Dialogue Fusion : seules les paires réellement proposées par l'agent
 *  sont sélectionnables. Pré-rempli avec la fusion courante le cas échéant. */
function openFusion() {
  const { prims, sides } = fusionTable();
  if (!prims.size) {
    showErr(t("fusion_none"));
    return;
  }
  const a = $("fu-a") as HTMLSelectElement;
  const ea = $("fu-ea") as HTMLSelectElement;
  const b = $("fu-b") as HTMLSelectElement;
  const eb = $("fu-eb") as HTMLSelectElement;
  const effLabel = (x: string) => x || "défaut";

  const refreshEB = () => {
    const effs = [...(sides.get(`${a.value}|${ea.value}`)?.get(b.value) ?? new Set<string>())];
    fillSel(eb, effs.map((e) => [e, effLabel(e)]));
  };
  const refreshB = () => {
    const m = sides.get(`${a.value}|${ea.value}`) ?? new Map<string, Set<string>>();
    fillSel(b, [...m.keys()].map((k) => [k, k]));
    refreshEB();
  };
  const refreshEA = () => {
    fillSel(ea, [...(prims.get(a.value) ?? new Set<string>())].map((e) => [e, effLabel(e)]));
    refreshB();
  };

  fillSel(a, [...prims.keys()].map((k) => [k, k]));
  a.onchange = refreshEA;
  ea.onchange = refreshB;
  b.onchange = refreshEB;

  // Pré-remplir depuis la valeur fusion courante (reconfiguration)
  const curVal =
    current?.config_options?.find((o) => o.id === "model" || o.category === "model")
      ?.currentValue || current?.model || "";
  const p = fusionParts(curVal);
  if (p && prims.has(p[0])) {
    a.value = p[0];
    refreshEA();
    if ([...ea.options].some((o) => o.value === p[1])) ea.value = p[1];
    refreshB();
    if ([...b.options].some((o) => o.value === p[2])) b.value = p[2];
    refreshEB();
    if ([...eb.options].some((o) => o.value === p[3])) eb.value = p[3];
  } else {
    refreshEA();
  }
  fusionApplied = false;
  ($("dlg-fusion") as HTMLDialogElement).showModal();
}

$("fu-cancel").onclick = () => ($("dlg-fusion") as HTMLDialogElement).close("cancel");

$("fu-ok").addEventListener("click", (e) => {
  e.preventDefault();
  const a = ($("fu-a") as HTMLSelectElement).value;
  const ea = ($("fu-ea") as HTMLSelectElement).value;
  const b = ($("fu-b") as HTMLSelectElement).value;
  const eb = ($("fu-eb") as HTMLSelectElement).value;
  const v = `fusion-${a}${ea ? "-" + ea : ""}-sidekick-${b}${eb ? "-" + eb : ""}`;
  if (!fusionTable().valid.has(v)) {
    showErr(t("fusion_bad") + v);
    return;
  }
  fusionApplied = true;
  ($("dlg-fusion") as HTMLDialogElement).close();
  applyModelValue(v);
});

// Annulation du dialogue : si le select affichait "✨ Fusion…" alors que le
// modèle courant n'est pas une fusion, restaurer sa valeur réelle.
($("dlg-fusion") as HTMLDialogElement).addEventListener("close", () => {
  const md = $("model-select") as HTMLSelectElement;
  const curVal = current?.model ?? "";
  if (!fusionApplied && md.value === "__fusion" && !curVal.startsWith("fusion-")) {
    md.value = splitModel(curVal)[0];
  }
});

$("btn-fusion-edit").onclick = () => openFusion();

($("model-select") as HTMLSelectElement).onchange = (e) => {
  const sel = e.target as HTMLSelectElement;
  if (sel.value === "__fusion") {
    openFusion();
    return;
  }
  fillEfforts(sel.value, "");
  const eff = ($("effort-select") as HTMLSelectElement).value;
  applyModelValue(joinModel(sel.value, eff));
};

($("effort-select") as HTMLSelectElement).onchange = (e) => {
  const base = ($("model-select") as HTMLSelectElement).value;
  const eff = (e.target as HTMLSelectElement).value;
  if (base && base !== "__fusion") applyModelValue(joinModel(base, eff));
};

$("btn-refresh").onclick = () => current && openSession(current.id);

// Double-clic sur le titre → renommage inline
$("chat-title").addEventListener("dblclick", () => {
  if (!current) return;
  const t = $("chat-title");
  const inp = document.createElement("input");
  inp.id = "title-edit";
  inp.value = current.title;
  t.replaceWith(inp);
  inp.focus();
  inp.select();
  const done = async (save: boolean) => {
    const v = inp.value.trim();
    inp.replaceWith(t);
    if (save && v && v !== current!.title) {
      current!.title = v;
      t.textContent = v;
      await call("rename_session", { session: current!.id, title: v })
        .catch((e) => showErr(String(e)));
      refreshSessions();
    }
  };
  inp.addEventListener("keydown", (e) => {
    if (e.key === "Enter") done(true);
    if (e.key === "Escape") done(false);
  });
  inp.addEventListener("blur", () => done(true));
});

($("model-free") as HTMLInputElement).addEventListener("keydown", async (e) => {
  if (e.key !== "Enter" || !current) return;
  const v = (e.target as HTMLInputElement).value.trim();
  if (v) await call("set_model", { session: current.id, model: v }).catch((er) => showErr(String(er)));
});

// Groupe cible d'une création lancée par le "+" d'un groupe — la session
// créée y est assignée automatiquement. conn = clé de routage (undefined =
// connexion par défaut).
let pendingGroup: { conn?: string; name: string } | null = null;

/** Dialogue "nouvelle session". `conn` route list_agents/new_session vers le
 *  bon serveur ; `ref` préremplit agent + workspace (1er membre du groupe). */
async function openNewSessionDlg(conn?: string, ref?: SessionMeta) {
  const agents = await call<{ id: string; name: string; available?: boolean }[]>("list_agents", conn ? { conn } : {});
  const sel = $("f-agent") as HTMLSelectElement;
  sel.innerHTML = "";
  for (const a of agents) {
    const o = document.createElement("option");
    o.value = a.id;
    o.textContent = a.name + (a.available === false ? ` (${t("agent_missing")})` : "");
    o.disabled = a.available === false;
    sel.appendChild(o);
  }
  if (ref) {
    if ([...sel.options].some((o) => o.value === ref.agent)) sel.value = ref.agent;
    ($("f-cwd") as HTMLInputElement).value = ref.cwd;
  }
  // Suggestions de modèles : union des config_options connues des sessions
  const sess = await call<SessionMeta[]>("list_sessions").catch(() => [] as SessionMeta[]);
  const seen = new Set<string>();
  const dl = $("model-list");
  dl.innerHTML = "";
  for (const s of sess) {
    for (const co of s.config_options ?? []) {
      if (co.id !== "model" && co.category !== "model") continue;
      for (const o of co.options) {
        if (seen.has(o.value)) continue;
        seen.add(o.value);
        const el = document.createElement("option");
        el.value = o.value;
        el.label = o.name || o.value;
        dl.appendChild(el);
      }
    }
  }
  ($("dlg-new") as HTMLDialogElement).showModal();
}

$("btn-new-session").onclick = () => {
  pendingGroup = null;
  void openNewSessionDlg();
};

// type=button → Enter dans le form valide via f-create (seul submit restant).
$("f-cancel").onclick = () => ($("dlg-new") as HTMLDialogElement).close("cancel");

$("f-create").addEventListener("click", async (e) => {
  e.preventDefault();
  const cwd = ($("f-cwd") as HTMLInputElement).value.trim();
  if (!cwd) return;
  const attach = ($("f-attach") as HTMLInputElement).value.trim();
  const grp = pendingGroup;
  const res = await (attach
    ? call<{ session: string }>("attach_session", {
        agent: ($("f-agent") as HTMLSelectElement).value,
        cwd,
        conn: grp?.conn,
        native_id: attach,
        title: ($("f-title") as HTMLInputElement).value || cwd.split("/").pop(),
      })
    : call<{ session: string }>("new_session", {
        agent: ($("f-agent") as HTMLSelectElement).value,
        cwd,
        conn: grp?.conn,
        title: ($("f-title") as HTMLInputElement).value || cwd.split("/").pop(),
        mode: ($("f-mode") as HTMLInputElement).value || undefined,
        model: ($("f-model") as HTMLInputElement).value || undefined,
        prompt: ($("f-prompt") as HTMLTextAreaElement).value || undefined,
      })
  ).catch((er) => {
    showErr(String(er));
    return null;
  });
  if (!res) return;
  if (grp) {
    // conn "" = connexion par défaut côté routage — la même que celle qui
    // vient de créer la session (évite l'injection de current.conn).
    await call("set_session_group", {
      session: res.session,
      conn: grp.conn ?? "",
      group: grp.name,
    }).catch((er) => showErr(String(er)));
    addStoredGroup(grp.conn ?? "default", grp.name);
    pendingGroup = null;
  }
  ($("dlg-new") as HTMLDialogElement).close();
  await refreshSessions();
});

interface SavedConn {
  kind: string;
  host?: string;
  addr?: string;
  path?: string;
  remote_port?: number;
  args?: string[];
  port?: number;
  user?: string;
  password?: string;
  key?: string;
  native?: boolean;
}
let savedConns: SavedConn[] = [];

// ---------------------------------------------------------------------------
// Surveillance connexions : ok | reconnecting | down
// ---------------------------------------------------------------------------
// Mort détectée via `conn-dead` (Rust : EOF du reader, instantané) ou une
// entrée {error} de la sonde fusionnée → down → retry en backoff
// exponentiel (2 s → 60 s max). Le retry est un `connect` simple : le démon
// est relancé par systemd (Restart=always) — jamais de réinstall en boucle.
type ConnLive = "ok" | "reconnecting" | "down";
const connLive = new Map<string, ConnLive>();
const connCfg = new Map<string, Record<string, unknown>>();
const connTimer = new Map<string, ReturnType<typeof setTimeout>>();
const connDelay = new Map<string, number>();
const RETRY_MIN = 2_000, RETRY_MAX = 60_000;

/** Tooltip du badge selon l'état. */
function connTip(st: ConnLive): string {
  if (st === "ok") return t("conn_ok");
  return `${t(st === "reconnecting" ? "reconnecting" : "conn_offline")} — ${t("retry_now")}`;
}

function markAlive(key: string) {
  const t0 = connTimer.get(key);
  if (t0) {
    clearTimeout(t0);
    connTimer.delete(key);
  }
  connDelay.delete(key);
  const was = connLive.get(key);
  connLive.set(key, "ok");
  if (was !== "ok") renderConnState();
}

function markDead(key: string) {
  // connCfg n'est posé qu'à la fin d'un doConnect réussi — une conn dont
  // le tout premier connect a échoué en route n'est pas suivie (fenêtre
  // minime, pas de config retenue à rejouer de toute façon).
  if (!connCfg.has(key)) return;
  if (connLive.get(key) !== "reconnecting") connLive.set(key, "down");
  renderConnState();
  scheduleRetry(key);
}

function scheduleRetry(key: string) {
  if (connTimer.has(key) || connLive.get(key) === "reconnecting") return;
  const d = connDelay.get(key) ?? RETRY_MIN;
  connDelay.set(key, Math.min(d * 2, RETRY_MAX));
  connTimer.set(key, setTimeout(() => {
    connTimer.delete(key);
    void retryConn(key);
  }, d));
}

/** Reconnexion simple (connect, pas d'ensure_daemon) — auto + bouton. */
async function retryConn(key: string) {
  const cfg = connCfg.get(key);
  // Pas de retry parallèle : un connect déjà en vol suffit — un second
  // tunnel SSH orphelin resterait vivant après remplacement.
  if (!cfg || connLive.get(key) === "reconnecting") return;
  connLive.set(key, "reconnecting");
  renderConnState();
  try {
    await invoke("connect", { cfg });
    markAlive(key);
    // Réouvrir la session affichée si elle vivait sur cette connexion.
    if (current && (current.conn ?? "default") === key) openSession(current.id);
    await refreshSessions();
  } catch {
    if (connLive.get(key) === "reconnecting") connLive.set(key, "down");
    scheduleRetry(key);
    renderConnState();
  }
}

/** Bouton "reconnecter" : annule l'attente backoff, retry immédiat. */
function manualRetry(key: string) {
  const t0 = connTimer.get(key);
  if (t0) {
    clearTimeout(t0);
    connTimer.delete(key);
  }
  void retryConn(key);
}

/** État agrégé → chip header ; re-render des badges de la sidebar. */
function renderConnState() {
  const bad = [...connLive.values()].some((s) => s !== "ok");
  const chip = $("conn-chip") as HTMLButtonElement;
  chip.hidden = !bad;
  if (bad) {
    const rc = [...connLive.values()].some((s) => s === "reconnecting");
    chip.textContent = rc ? `⟳ ${t("reconnecting")}` : `⚠ ${t("conn_offline")}`;
    chip.className = "badge " + (rc ? "warn" : "st-dead");
    chip.title = t("retry_now");
  }
  refreshSessions().catch(() => {});
}
$("conn-chip").addEventListener("click", () => {
  for (const [k, s] of connLive) if (s !== "ok") manualRetry(k);
});
// Mort instantanée du transport (EOF reader côté Rust).
listen<string>("conn-dead", (e) => markDead(e.payload));

// Watchdog : sonde fusionnée toutes les 10 s (timeout 8 s/conn côté Rust) —
// filet de sécurité pour les blackholes réseau (pas d'EOF → pas de
// conn-dead). Entrées {error} → markDead → backoff de reconnexion.
setInterval(async () => {
  try {
    const st = await call<Record<string, unknown>>("daemon_status");
    let anyOk = false;
    for (const [k, v] of Object.entries(st)) {
      if (k === "servers" || k === "sessions") continue;
      if ((v as { error?: string }).error) markDead(k);
      else {
        anyOk = true;
        if ((connLive.get(k) ?? "ok") !== "ok") markAlive(k);
      }
    }
    // Aucune conn saine → démon(s) injoignable(s), icône grise.
    if (!anyOk) setAppIcon("grey");
  } catch {
    // L'appel fusionné lui-même a échoué — le backoff gère les retry.
    setAppIcon("grey");
  }
  refreshSessions().catch(() => {});
}, 10_000);

// Barre d'état : mémoire + CPU du client, toutes les 3 s
async function updateStatusbar() {
  try {
    const s = await invoke<{ rss_mb: number; cpu: number; rx_bps: number; tx_bps: number }>("sys_stats");
    const fmt = (b: number) => b >= 1_048_576 ? `${(b / 1_048_576).toFixed(1)} MB/s` : b >= 1024 ? `${(b / 1024).toFixed(0)} kB/s` : `${Math.round(b)} B/s`;
    $("sb-stats").textContent = `mem ${Math.round(s.rss_mb)} MB · cpu ${s.cpu}% · ↓${fmt(s.rx_bps)} ↑${fmt(s.tx_bps)}`;
  } catch { /* silencieux */ }
}
setInterval(updateStatusbar, 3000);
updateStatusbar();

// Auto-reconnexion au démarrage avec la dernière connexion utilisée
invoke<SavedConn[]>("saved_connections")
  .then(async (conns) => {
    savedConns = conns;
    if (localStorage.getItem("ac-autoconnect") === "0") return;
    for (const c of conns) await doConnect({ ...c }).catch((e) => showErr(String(e)));
  })
  .catch(() => {});

function fillConnForm(c: SavedConn) {
  ($("c-kind") as HTMLSelectElement).value = c.kind;
  ($("c-host") as HTMLInputElement).value = c.host ?? c.addr ?? c.path ?? "";
  ($("c-port") as HTMLInputElement).value = String(c.remote_port ?? 7331);
  ($("c-args") as HTMLInputElement).value = (c.args ?? []).join(" ");
  ($("c-sport") as HTMLInputElement).value = String(c.port ?? 22);
  ($("c-user") as HTMLInputElement).value = c.user ?? "";
  ($("c-pass") as HTMLInputElement).value = c.password ?? "";
  ($("c-native") as HTMLInputElement).checked = c.native === true || IS_ANDROID;
  syncSshFields();
}

/// Champs SSH natifs visibles si kind=ssh && (native || android) ;
/// args CLI sinon (desktop). Local → presque rien.
function syncSshFields() {
  const kind = ($("c-kind") as HTMLSelectElement).value;
  const ssh = kind === "ssh";
  const nat = ($("c-native") as HTMLInputElement).checked || IS_ANDROID;
  document.querySelectorAll<HTMLElement>(".nat-row").forEach((r) => {
    r.style.display = ssh && nat ? "" : "none";
  });
  ($("c-args-wrap") as HTMLElement).style.display = ssh && !nat ? "" : "none";
  ($("c-native-wrap") as HTMLElement).style.display = ssh && !IS_ANDROID ? "" : "none";
  ($("c-host") as HTMLInputElement).style.display = kind === "local" ? "none" : "";
  ($("c-port") as HTMLInputElement).style.display = ssh ? "" : "none";
  if (ssh && nat) void showPubkey();
}

async function showPubkey() {
  const box = $("c-pubkey-box");
  if (!box.hidden) return;
  box.hidden = false;
  const pre = $("c-pubkey");
  pre.textContent = await invoke<string>("ssh_pubkey").catch((e) => `⚠ ${e}`);
}
$("c-pubkey-copy").onclick = async () => {
  const txt = $("c-pubkey").textContent ?? "";
  try {
    await navigator.clipboard.writeText(txt);
    $("c-pubkey-copy").textContent = "✓";
  } catch {
    // Clipboard refusé → sélectionner le texte pour copie manuelle.
    const r = document.createRange();
    r.selectNodeContents($("c-pubkey"));
    getSelection()?.removeAllRanges();
    getSelection()?.addRange(r);
  }
  setTimeout(() => { $("c-pubkey-copy").textContent = t("copy"); }, 1500);
};
($("c-kind") as HTMLSelectElement).onchange = syncSshFields;
($("c-native") as HTMLInputElement).onchange = syncSshFields;

$("btn-connect").onclick = async () => {
  // Android : pas de socket local ni de binaire ssh → SSH natif par défaut.
  if (IS_ANDROID) {
    const ks = $("c-kind") as HTMLSelectElement;
    ks.querySelector('option[value="local"]')?.remove();
    ks.value = "ssh";
    ($("c-native") as HTMLInputElement).checked = true;
  }
  syncSshFields();
  // Alias Host de ~/.ssh/config en suggestions
  const dl = $("ssh-hosts");
  dl.innerHTML = "";
  for (const h of await invoke<string[]>("ssh_hosts").catch(() => [] as string[])) {
    const o = document.createElement("option");
    o.value = h;
    dl.appendChild(o);
  }
  // Connexions récentes sauvegardées
  savedConns = await invoke<SavedConn[]>("saved_connections").catch(() => []);
  const sv = $("c-saved") as HTMLSelectElement;
  sv.innerHTML = `<option value="">${t("choose")}</option>`;
  savedConns.forEach((c, i) => {
    const o = document.createElement("option");
    o.value = String(i);
    o.textContent = `${c.kind} ${c.host ?? c.addr ?? c.path ?? ""}`;
    sv.appendChild(o);
  });
  // Pré-remplir avec la dernière utilisée → un clic sur Connecter suffit
  if (savedConns.length) {
    sv.value = "0";
    fillConnForm(savedConns[0]);
  }
  ($("dlg-conn") as HTMLDialogElement).showModal();
};

($("c-saved") as HTMLSelectElement).onchange = (e) => {
  const i = Number((e.target as HTMLSelectElement).value);
  if (savedConns[i]) fillConnForm(savedConns[i]);
};
$("c-logs").addEventListener("click", async () => {
  const v = $("log-view");
  v.hidden = !v.hidden;
  if (!v.hidden) v.textContent = await invoke<string>("log_tail", { n: 200 });
});

$("c-cancel").onclick = () => ($("dlg-conn") as HTMLDialogElement).close("cancel");

$("c-go").addEventListener("click", async (e) => {
  e.preventDefault();
  const btn = e.currentTarget as HTMLButtonElement;
  btn.disabled = true;
  btn.classList.add("spin");
  const kind = ($("c-kind") as HTMLSelectElement).value;
  const cfg: Record<string, unknown> =
    kind === "ssh"
      ? {
          kind: "ssh",
          host: ($("c-host") as HTMLInputElement).value,
          remote_port: Number(($("c-port") as HTMLInputElement).value) || 7331,
          args: ($("c-args") as HTMLInputElement).value.split(" ").filter(Boolean),
          port: Number(($("c-sport") as HTMLInputElement).value) || 22,
          user: ($("c-user") as HTMLInputElement).value || undefined,
          password: ($("c-pass") as HTMLInputElement).value || undefined,
          native: ($("c-native") as HTMLInputElement).checked || IS_ANDROID || undefined,
        }
      : kind === "tcp"
        ? { kind: "tcp", addr: ($("c-host") as HTMLInputElement).value }
        : { kind: "local" };
  try {
    await doConnect(cfg);
    invoke("save_connection", { cfg }).catch(console.error);
    ($("dlg-conn") as HTMLDialogElement).close();
  } catch (err) {
    showErr(String(err));
  } finally {
    btn.disabled = false;
    btn.classList.remove("spin");
  }
});
