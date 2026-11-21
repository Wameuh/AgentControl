// Glisser des lignes session vers la gauche (N4) : révèle une bande
// « Endormir » / « Supprimer » plaquée à droite de la ligne. Délégation
// sur #sidebar — les lignes sont reconstruites à chaque rendu et la
// bande injectée disparaît avec elles (aucun nettoyage nécessaire).

import { $, esc } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { t } from "../i18n";
import { app } from "../core/state";
import type { Sidebar } from "../features/sidebar";

/** Largeur de la bande d'actions — doit matcher le CSS `.sess-acts`. */
const W = 132;

let sb: Sidebar;
let row: HTMLElement | null = null;
let acts: HTMLElement | null = null;
let sx = 0;
let sy = 0;
let engaged = false;
let reveal = 0;
/** Ligne dont la bande est restée ouverte après relâchement. */
let openRow: HTMLElement | null = null;

function closeActs(r: HTMLElement | null) {
  const a = r?.querySelector<HTMLElement>(".sess-acts");
  if (!a) return;
  a.style.transition = "";
  a.style.transform = `translateX(${W}px)`;
  setTimeout(() => a.remove(), 180);
  if (openRow === r) openRow = null;
}

function ensureActs(r: HTMLElement): HTMLElement {
  const a = r.querySelector<HTMLElement>(".sess-acts");
  if (a) return a;
  const s = app.lastSessions.find((x) => x.id === r.dataset.sid);
  const el = document.createElement("span");
  el.className = "sess-acts";
  el.innerHTML =
    `<button class="sact sact-sleep">${esc(t("ctx_sleep"))}</button>` +
    `<button class="sact sact-del">${esc(t("ctx_delete_sess"))}</button>`;
  r.style.position = "relative";
  r.style.overflow = "hidden";
  el.style.transition = "none";
  el.style.transform = `translateX(${W}px)`;
  r.appendChild(el);
  // Le tap sur la bande ne doit ni ouvrir la session ni armer le
  // clic long ; on absorbe tout.
  el.addEventListener("click", (e) => e.stopPropagation());
  const [bs, bd] = [...el.querySelectorAll<HTMLButtonElement>(".sact")];
  bs.disabled = !s || !!s.offline ||
    s.state === "asleep" || s.state === "wait-asleep" ||
    s.state === "dead" || s.state === "interrupted";
  bs.onclick = () => {
    if (s) sb.sessionAction(s, "sleep");
    closeActs(r);
  };
  // Supprimer garde le double tap armé — même sémantique que le menu.
  bd.onclick = () => {
    if (!bd.dataset.armed) {
      bd.dataset.armed = "1";
      bd.textContent = t("ctx_confirm");
      return;
    }
    if (s) sb.sessionAction(s, "delete", r);
    openRow = null;
  };
  return el;
}

export function initGestures(sidebar: Sidebar) {
  if (!isAndroid()) return;
  sb = sidebar;
  const host = $("sidebar");

  host.addEventListener("touchstart", (e) => {
    const tt = e.touches[0];
    const r = (e.target as HTMLElement).closest<HTMLElement>(".sess");
    if (openRow && openRow !== r) closeActs(openRow);
    row = r;
    acts = r?.querySelector<HTMLElement>(".sess-acts") ?? null;
    sx = tt.clientX;
    sy = tt.clientY;
    engaged = false;
    reveal = acts && openRow === r ? W : 0;
  }, { passive: true });

  host.addEventListener("touchmove", (e) => {
    if (!row) return;
    const tt = e.touches[0];
    const dx = tt.clientX - sx;
    const dy = tt.clientY - sy;
    if (!engaged) {
      // Un geste dominé par la verticale = défilement, jamais glisser.
      if (Math.abs(dy) >= Math.abs(dx)) { row = null; return; }
      if (dx < -12) { engaged = true; acts = ensureActs(row); }
    }
    if (engaged && acts) {
      e.preventDefault();
      const open = Math.min(Math.max((openRow === row ? W : 0) - dx, 0), W);
      reveal = open;
      acts.style.transition = "none";
      acts.style.transform = `translateX(${W - open}px)`;
    }
  }, { passive: false });

  host.addEventListener("touchend", () => {
    if (!row) return;
    const a = acts;
    const r = row;
    const rev = reveal;
    row = null;
    acts = null;
    engaged = false;
    reveal = 0;
    if (!a || rev <= 0) return;
    a.style.transition = "";
    if (rev > W / 2) {
      // Relâché au-delà de la moitié : la bande reste ouverte.
      a.style.transform = "translateX(0)";
      openRow = r;
    } else {
      closeActs(r);
    }
  });

  // Tap hors bande ouverte = fermeture ; un tap sur la ligne elle-même
  // referme sans ouvrir la session (capture, avant le onclick de ligne)
  // ; ailleurs le clic passe quand même (il referme juste la bande).
  document.addEventListener("click", (e) => {
    if (!openRow) return;
    const el = e.target as HTMLElement;
    if (el.closest(".sess-acts")) return;
    if (openRow.contains(el)) {
      e.preventDefault();
      e.stopPropagation();
    }
    closeActs(openRow);
  }, true);
}
