// ---------------------------------------------------------------------------
// Coquille d'accueil mobile (N1/N2) : barre haute — titre de la vue
// active, puce de connexion (● hôte ⌄ → feuille d'actions), icône
// recherche → #sess-filter. m-home / m-map sont dérivés de la pile
// OverlayNav (homeSync) ; la visibilité de la barre session en dépend,
// d'où le callback nav.onHomeChange.
// ---------------------------------------------------------------------------

import { $, esc } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { connLabel } from "../core/conn";
import { t } from "../i18n";
import { icon } from "../ui/icons";
import { nav } from "../ui/nav";
import { gemSVG } from "../crystal";
import type { ConnectionManager } from "../features/connections";
import { renderSessionBar } from "./session-bar";
import { openConnSheet } from "./conn-sheet";

let conns: ConnectionManager;

export function initHomeShell(c: ConnectionManager) {
  if (!isAndroid()) return;
  conns = c;
  // m-in-session = session chargée + écran Session affiché : re-rendu à
  // chaque bascule de pile (push/pop « session », overlays, map).
  nav.onHomeChange = () => renderSessionBar();
  $("m-conn").onclick = () => openConnSheet();
  // Loupe → ligne de filtre (partagée par les trois vues de l'accueil).
  $("m-search").onclick = () => {
    const on = document.body.classList.toggle("m-filter");
    if (on) ($("sess-filter-input") as HTMLInputElement).focus();
  };
  const fIn = $("sess-filter-input") as HTMLInputElement;
  fIn.addEventListener("keydown", (e) => {
    if (e.key === "Escape") document.body.classList.remove("m-filter");
  });
  // Gemme de l'onglet Agents : neutre avant le premier refresh réel.
  const ic = $("sv-agents").querySelector(".sv-ic");
  if (ic) ic.innerHTML = gemSVG("neutral");
  syncHomeConn();
}

/** Puce connexion de l'accueil : « ● hôte ⌄ » — point coloré selon
 *  l'état live de la conn liée (mêmes classes .conn-st que la statusbar
 *  et les en-têtes de groupe). Sans conn liée la puce se masque : la vue
 *  connexion auto-ouverte est alors l'accueil. Re-rendue par le
 *  liveChanged de main.ts. */
export function syncHomeConn() {
  if (!isAndroid()) return;
  const el = $("m-conn");
  const k = conns.knownConns()[0];
  if (!k) {
    el.hidden = true;
    return;
  }
  el.hidden = false;
  const st = conns.isGone(k) ? "off" : (conns.state(k) ?? "ok");
  const cls =
    st === "ok" ? "ok" : st === "reconnecting" ? "warn" : st === "down" ? "bad" : "off";
  el.innerHTML =
    `<span class="conn-st ${cls}">${icon(st === "reconnecting" ? "refresh" : "dot")}</span>` +
    `<span class="m-conn-l">${esc(connLabel(k))}</span>${icon("chev-down")}`;
  el.title = connLabel(k);
}
