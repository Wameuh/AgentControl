// ---------------------------------------------------------------------------
// Environnement applicatif : plateforme (Android/desktop), thème, fond,
// fenêtre custom (drag, min/max/close), ouverture de liens externes.
// ---------------------------------------------------------------------------

import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { gemSVG } from "../crystal";
import { t } from "../i18n";
import { icon } from "../ui/icons";
import { $ } from "./dom";

const mesh = (window as { MeshBackground?: { setMood: (m: string, o: object) => void } })
  .MeshBackground;
export const bgMood = () => (document.documentElement.classList.contains("light") ? "paper" : "sand");
export const meshSetMood = (o: object) => mesh?.setMood(bgMood(), o);

export function applyTheme(name: "dark" | "light") {
  document.documentElement.classList.toggle("light", name === "light");
  localStorage.setItem("ac-theme", name);
  mesh?.setMood(bgMood(), { intensity: 0.55 });
}

/** Gemme cristal : verte (neutral) = identité app ; dorée (human) = travail. */
export function initGems() {
  for (const id of ["titlebar-gem", "about-gem"]) {
    const el = document.getElementById(id);
    if (el) el.innerHTML = gemSVG("neutral");
  }
  const cg = document.getElementById("chat-gem");
  if (cg) {
    cg.dataset.role = "human";
    cg.innerHTML = gemSVG("human");
  }
}

// Plateforme (android | windows | linux) — gates UI mobile.
// Détection synchrone par UA (avant le resolve de platform()).
let android = /Android/i.test(navigator.userAgent);
export const isAndroid = () => android;

export function applyAndroidChrome() {
  // Pas de fenêtre à minimizer/drag sous Android : ☰ + ⋮ seulement.
  $("tb-menu").hidden = false;
  $("tb-more").hidden = false;
  $("chat-more").hidden = false;
  $("titlebar").removeAttribute("data-tauri-drag-region");
  ($("input") as HTMLTextAreaElement).enterKeyHint = "send";
  ($("input") as HTMLTextAreaElement).placeholder = t("input_ph_mobile");
  // Page fichiers → flèche retour. dataset.icon + innerHTML : déclaratif et
  // rendu d'un coup, quel que soit l'ordre avec hydrateIcons() du boot.
  $("file-close").dataset.icon = "back";
  $("file-close").innerHTML = icon("back");
}

export function initPlatform() {
  invoke<string>("platform").then((p) => {
    android = p === "android";
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

// Fenêtre custom (decorations: false) : drag natif via data-tauri-drag-region
const appWin = getCurrentWindow();

// dragDropEnabled:false → WebView2 garde AllowExternalDrop=true : un
// fichier OS droppé sur la fenêtre ferait naviguer la webview vers
// file:///… — on avale les drops de fichiers au niveau window (les
// cibles internes ont déjà traité les leurs en amont ; restreint à
// "Files" pour garder curseur + drop de texte natif intacts).
const isFileDrag = (e: DragEvent) => e.dataTransfer?.types.includes("Files") ?? false;

export function initWindowChrome() {
  $("tb-min").onclick = () => appWin.minimize();
  $("tb-max").onclick = () => appWin.toggleMaximize();
  $("tb-close").onclick = () => appWin.close();

  window.addEventListener("dragover", (e) => { if (isFileDrag(e)) e.preventDefault(); });
  window.addEventListener("drop", (e) => { if (isFileDrag(e)) e.preventDefault(); });

  $("titlebar").addEventListener("dblclick", (e) => {
    if (!android && !(e.target as HTMLElement).closest("button")) appWin.toggleMaximize();
  });
  // Drag fenêtre explicite (plus fiable que data-tauri-drag-region seul)
  $("titlebar").addEventListener("mousedown", (e) => {
    if (!android && e.button === 0 && !(e.target as HTMLElement).closest("button"))
      void appWin.startDragging();
  });
  document.querySelectorAll<HTMLElement>(".resize-edge").forEach((el) => {
    el.addEventListener("mousedown", (e) => {
      e.preventDefault();
      void appWin.startResizeDragging(el.dataset.dir as never);
    });
  });
}
