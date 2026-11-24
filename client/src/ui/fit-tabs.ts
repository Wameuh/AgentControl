// ---------------------------------------------------------------------------
// Onglets de vue de la barre latérale (Agents / Repos / Backlog /
// Diagramme) trop étroits pour leur libellé (« A… », « D… ») : icônes
// seules, le libellé reste en infobulle (title). Mesuré sur les libellés
// réels — dépend de la langue, des compteurs et de la largeur glissée.
// ---------------------------------------------------------------------------

import { $ } from "../core/dom";

export function fitViewTabs() {
  const sv = $("side-view");
  sv.classList.remove("sv-narrow");
  const cut = [...sv.querySelectorAll<HTMLElement>(".sv .sv-ic + span")].some(
    (s) => s.scrollWidth > s.clientWidth + 1,
  );
  sv.classList.toggle("sv-narrow", cut);
}

/** Recalcule à chaque redimensionnement de la barre (poignée, fenêtre). */
export function initViewTabsFit() {
  new ResizeObserver(() => fitViewTabs()).observe($("side-view"));
}
