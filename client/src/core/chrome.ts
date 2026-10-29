// ---------------------------------------------------------------------------
// Environnement applicatif : plateforme (Android/desktop), thème, fond,
// fenêtre custom (drag, min/max/close), ouverture de liens externes.
// ---------------------------------------------------------------------------

import { invoke, isWeb } from "./bridge";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { gemSVG } from "../crystal";
import { t } from "../i18n";
import { icon } from "../ui/icons";
import { $ } from "./dom";
import { connLabel } from "./conn";

const mesh = (window as {
  MeshBackground?: {
    setMood: (m: string, o: object) => void;
    setAnimated: (on: boolean) => void;
    isAnimated: () => boolean;
  };
}).MeshBackground;
export const bgMood = () => (document.documentElement.classList.contains("light") ? "paper" : "sand");

/* Sonde de résolution couleur : getPropertyValue rend les tokens bruts
   (les var() sont substitués, pas les color-mix) — seule une vraie
   propriété donne la valeur utilisée. Réutilisée par le mesh (triplets
   canvas) et le thème xterm (chaîne résolue). */
let probeEl: HTMLElement | null = null;
function cssColor(varName: string): string {
  if (!probeEl) {
    probeEl = document.createElement("i");
    probeEl.style.display = "none";
    document.body.appendChild(probeEl);
  }
  probeEl.style.color = "";
  probeEl.style.color = `var(${varName})`;
  return getComputedStyle(probeEl).color;
}
/** --var résolue → [r, g, b] 0-255 (« rgb(…) » direct ; « color(srgb …) » = 0-1). */
function rgbOf(varName: string): number[] | null {
  const v = cssColor(varName);
  const nums = v.match(/[\d.]+/g)?.map(Number);
  if (!nums || nums.length < 3) return null;
  const f = v.startsWith("color(") ? 255 : 1;
  return nums.slice(0, 3).map((n) => Math.round(n * f));
}
/** --var résolue en « rgb(r, g, b) » — sûre pour canvas (xterm). */
export function themeRgb(varName: string): string | null {
  const rgb = rgbOf(varName);
  return rgb ? `rgb(${rgb[0]}, ${rgb[1]}, ${rgb[2]})` : null;
}

// Couleurs du mesh = palette résolue courante (--bg + --accent, dérivés
// du thème en base.css). Conservées en état : tout appel meshSetMood les
// repasse — sinon un update « agents » écraserait la teinte d'accent.
let meshColors: { base?: number[]; light?: number[] } = {};
// Fond figé (« Fond animé » off) : pas de repaint — mais la cible de mood
// suit quand même (freeze) pour que la réactivation reparte à jour.
export const meshSetMood = (o: object) => {
  const freeze = mesh?.isAnimated() === false;
  mesh?.setMood(bgMood(), { ...meshColors, ...(freeze ? { freeze } : {}), ...o });
};
/** Re-lit la palette résolue et la pousse au mesh : sa lumière principale
 *  = accent (thème, ou couleur de connexion quand l'instance est liée),
 *  sa base = --bg dérivé. « sauge » retombe pile sur les presets.
 *  Appel direct à setMood (pas meshSetMood) : changement de couleur
 *  explicite → repaint immédiat même quand l'animation est off — le
 *  freeze n'est que pour les mises à jour de fond (compteur agents). */
function syncMeshAccent() {
  const base = rgbOf("--bg");
  const light = rgbOf("--accent");
  if (base && light) meshColors = { base, light };
  mesh?.setMood(bgMood(), { ...meshColors, intensity: 0.55 });
}

export function applyTheme(name: "dark" | "light") {
  document.documentElement.classList.toggle("light", name === "light");
  localStorage.setItem("ac-theme", name);
  // La palette dérivée dépend du mode : recalcul après le toggle.
  syncMeshAccent();
}

// Couleur d'accent — dimension orthogonale à dark/light (cf. styles/base.css).
// « sauge » = défaut, famille khaki/olive de la gemme neutre (identité app).
export type Accent = "sage" | "gold" | "grey" | "violet" | "red";
const ACCENTS: Accent[] = ["sage", "gold", "grey", "violet", "red"];

/** Accent valide ou undefined — validation d'un champ libre (cfg conn). */
export const accentOf = (v: unknown): Accent | undefined =>
  (ACCENTS as string[]).includes(v as string) ? (v as Accent) : undefined;

/** Accent persisté, « sage » si absent ou inconnu. */
export function accentPref(): Accent {
  const v = localStorage.getItem("ac-accent");
  return (ACCENTS as string[]).includes(v ?? "") ? (v as Accent) : "sage";
}

/** Applique l'accent ; `persist:false` pour un accent temporaire (couleur
 *  de connexion — le réglage global ac-accent n'est pas écrasé). */
export function applyAccent(name: Accent, persist = true) {
  const el = document.documentElement;
  for (const a of ACCENTS) if (a !== "sage") el.classList.toggle(`theme-${a}`, name === a);
  if (persist) localStorage.setItem("ac-accent", name);
  syncMeshAccent();
}

/** Chrome par connexion liée (modèle multi-instances : 1 instance = 1
 *  connexion) : accent propre à la conn (sinon celui des réglages),
 *  liseré d'identification (:root.conn-tinted, styles/layout.css) et
 *  titre de fenêtre « AgentControl — hôte » (taskbar + titlebar custom).
 *  Arbitrage couleur : la couleur de connexion DEVIENT l'accent de
 *  l'instance — elle a la priorité sur l'accent global partout (palette
 *  de surfaces et fond mesh inclus), ce qui fait de toute la fenêtre
 *  l'identifiant visuel de la connexion. Sans couleur de conn, l'accent
 *  global des réglages s'applique.
 *  conn = clé connKey ; null = aucune connexion liée (retour au neutre). */
export function applyConnChrome(conn: string | null, color?: Accent | null) {
  document.documentElement.classList.toggle("conn-tinted", !!color);
  applyAccent(color ?? accentPref(), false);
  const label = conn && conn !== "web" ? ` — ${connLabel(conn)}` : "";
  const title = `AgentControl${label}`;
  document.title = title;
  const tt = $("titlebar-title");
  if (tt) tt.textContent = title;
  void appWin?.setTitle(title).catch(() => {});
}

/** « Fond animé » : setAnimated applique + persiste (propriétaire unique). */
export function applyBgAnim(on: boolean) {
  mesh?.setAnimated(on);
}

/** État effectif du fond animé (résout ac-bganim / reduced-motion / Android). */
export const bgAnimOn = () => mesh?.isAnimated() ?? false;

/** Gemme cristal : verte (neutral) = identité app ; dorée (human) = travail. */
export function initGems() {
  for (const id of ["titlebar-gem", "about-gem", "empty-gem"]) {
    const el = document.getElementById(id);
    if (el) el.innerHTML = gemSVG("neutral");
  }
  const cg = document.getElementById("chat-gem");
  if (cg) {
    cg.dataset.role = "human";
    cg.innerHTML = gemSVG("human");
  }
}

// Plateforme (android | windows | macos | linux) — gates UI mobile.
// Détection synchrone par UA (avant le resolve de platform()).
let android = /Android/i.test(navigator.userAgent);
export const isAndroid = () => android;
// Windows + Android : les schémas custom sont servis en
// http://<scheme>.localhost (pas <scheme>://…) — forme de l'URL acvideo.
let windows = /Windows/i.test(navigator.userAgent);
export const isWindows = () => windows;

export function applyAndroidChrome() {
  // Pas de fenêtre à minimizer/drag sous Android : ☰ + ⋮ seulement.
  $("tb-menu").hidden = false;
  $("tb-more").hidden = false;
  $("chat-more").hidden = false;
  ($("input") as HTMLTextAreaElement).enterKeyHint = "send";
  ($("input") as HTMLTextAreaElement).placeholder = t("input_ph_mobile");
  // Page fichiers → flèche retour. dataset.icon + innerHTML : déclaratif et
  // rendu d'un coup, quel que soit l'ordre avec hydrateIcons() du boot.
  $("file-close").dataset.icon = "back";
  $("file-close").innerHTML = icon("back");
}

export function initPlatform() {
  // Web : le navigateur gère sa fenêtre — pas de chrome custom ni de
  // connexion multi-démons (le démon qui sert la page est celui piloté).
  if (isWeb) document.body.classList.add("web");
  invoke<string>("platform").then((p) => {
    android = p === "android";
    windows = p === "windows";
    document.body.classList.toggle("android", android);
    if (android) applyAndroidChrome();
  }).catch(() => {});
  document.body.classList.toggle("android", android);
  if (android) applyAndroidChrome();
}

/// Ouvre une URL externe (plugin opener — navigateur système sur Android).
export function shellOpen(url: string) {
  invoke("plugin:opener|open_url", { url }).catch(() => {});
}

// Fenêtre custom (decorations: false) : drag natif via startDragging()
// explicite — pas de data-tauri-drag-region : son handler WRY n'est pas
// gardé par e.detail et relancerait un move-drag au double-clic (grab
// tué par toggleMaximize → curseur "main fermée" résiduel sous Wayland).
// En webview navigateur (isWeb), getCurrentWindow() lèverait — gardé.
const appWin = isWeb ? null : getCurrentWindow();

// dragDropEnabled:false → WebView2 garde AllowExternalDrop=true : un
// fichier OS droppé sur la fenêtre ferait naviguer la webview vers
// file:///… — on avale les drops de fichiers au niveau window (les
// cibles internes ont déjà traité les leurs en amont ; restreint à
// "Files" pour garder curseur + drop de texte natif intacts).
const isFileDrag = (e: DragEvent) => e.dataTransfer?.types.includes("Files") ?? false;

export function initWindowChrome() {
  if (!appWin) return; // navigateur : la fenêtre n'est pas pilotable
  $("tb-min").onclick = () => appWin.minimize();
  $("tb-max").onclick = () => appWin.toggleMaximize();
  $("tb-close").onclick = () => appWin.close();

  window.addEventListener("dragover", (e) => { if (isFileDrag(e)) e.preventDefault(); });
  window.addEventListener("drop", (e) => { if (isFileDrag(e)) e.preventDefault(); });

  $("titlebar").addEventListener("dblclick", (e) => {
    if (!android && !(e.target as HTMLElement).closest("button")) appWin.toggleMaximize();
  });
  // Drag fenêtre explicite (garantit le garde-fou e.detail, absent de
  // l'attribut WRY). e.detail === 1 : au double-clic, le 2e mousedown
  // relancerait un move-drag pendant que toggleMaximize() change la
  // géométrie — le grab WM tué en plein vol laisse le curseur "main
  // fermée" collé sous Wayland/GTK (famille des drags interrompus).
  $("titlebar").addEventListener("mousedown", (e) => {
    if (!android && e.button === 0 && e.detail === 1 &&
        !(e.target as HTMLElement).closest("button"))
      void appWin.startDragging();
  });
  document.querySelectorAll<HTMLElement>(".resize-edge").forEach((el) => {
    el.addEventListener("mousedown", (e) => {
      e.preventDefault();
      void appWin.startResizeDragging(el.dataset.dir as never);
    });
  });
}
