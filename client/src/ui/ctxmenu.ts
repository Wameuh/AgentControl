// ---------------------------------------------------------------------------
// Menus contextuels : bind clic droit / appui long + helpers de remplissage
// et positionnement partagés par les menus session/fichier/connexion.
// ---------------------------------------------------------------------------

import { $ } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { nav } from "./nav";

/// Clic droit (desktop) ou appui long ~500 ms (tactile) → menu contextuel.
/// `filter` optionnel : l'événement est ignoré s'il ne le passe pas —
/// utile sur un conteneur dont les enfants ont leur propre bindCtx (le
/// touch des enfants remonte par bubbling, le contextmenu non — il est
/// stopPropagation'd — d'où le filtre des deux chemins).
export function bindCtx(
  el: HTMLElement,
  cb: (x: number, y: number) => void,
  filter?: (e: Event) => boolean,
) {
  el.addEventListener("contextmenu", (e) => {
    if (filter && !filter(e)) return;
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
    if (filter && !filter(e)) return;
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

/** Ajoute un bouton d'action au menu `host` (même pattern que les ctx existants). */
export function menuBtn(
  host: HTMLElement,
  label: string,
  cls: string,
  fn: (b: HTMLButtonElement) => void,
): HTMLButtonElement {
  const b = document.createElement("button");
  b.textContent = label;
  b.className = cls;
  b.onclick = () => fn(b);
  host.appendChild(b);
  return b;
}

/** Affiche le menu `el` au point (x, y) — fixe desktop, bottom-sheet Android. */
export function placeMenu(el: HTMLElement, navId: string, x: number, y: number) {
  el.hidden = false;
  nav.push(navId);
  if (!isAndroid()) {
    const r = el.getBoundingClientRect();
    el.style.left = `${Math.min(x, window.innerWidth - r.width - 8)}px`;
    el.style.top = `${Math.min(y, window.innerHeight - r.height - 8)}px`;
  }
}

/** Ferme un menu contextuel (pile nav + hidden). */
export function hideMenu(el: HTMLElement, navId: string) {
  nav.close(navId);
  el.hidden = true;
}

/** Ferme le menu contextuel partagé de la sidebar (#sctx). */
export function hideSessCtx() {
  hideMenu($("sctx"), "sctx");
}
