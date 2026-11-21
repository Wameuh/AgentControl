// ---------------------------------------------------------------------------
// Paramètres en liste (Android, Y4) : à l'ouverture les sections se ferment
// pour n'afficher que leurs lignes ; un tap ouvre la section en sous-écran
// (titre repris dans l'en-tête, les autres sections masquées) et pousse
// « settings-sec » sur la pile nav. Le retour système referme la section →
// retour à la liste ; un second retour sort des Paramètres. Enregistrer /
// Annuler restent dans l'en-tête, visibles dans les deux modes.
// ---------------------------------------------------------------------------

import { $ } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { t } from "../i18n";
import { nav } from "../ui/nav";

/** La section actuellement ouverte (une seule à la fois en sous-écran). */
const openSec = (): HTMLDetailsElement | null =>
  document.querySelector<HTMLDetailsElement>(
    "#settings-body details.s-sec[open]",
  );

function enterSub(d: HTMLDetailsElement) {
  const view = $("settings-view");
  view.classList.add("m-sub");
  // Identifiant stable de la section : son id, sinon la clé i18n du titre.
  view.dataset.sub =
    d.id ||
    d.querySelector(".s-sec-title")?.getAttribute("data-i18n") ||
    "";
  $("settings-title").textContent =
    d.querySelector(".s-sec-title")?.textContent ?? "";
  nav.push("settings-sec");
}

function exitSub() {
  const view = $("settings-view");
  if (!view.classList.contains("m-sub")) return;
  view.classList.remove("m-sub");
  delete view.dataset.sub;
  $("settings-title").textContent = t("settings_h");
  // Purge l'entrée de pile si la fermeture ne vient pas du back système
  // (✕ du panneau, fermeture programmatique) — nav.close est idempotent.
  nav.close("settings-sec");
}

/** Back depuis un sous-écran : referme la section ouverte — son « toggle »
 *  rend la liste (retrait de m-sub + titre restauré). */
export function closeSettingsSection() {
  const d = openSec();
  if (d) d.open = false;
}

export function initSettingsMobile() {
  if (!isAndroid()) return;
  const view = $("settings-view");
  const secs = [
    ...view.querySelectorAll<HTMLDetailsElement>("details.s-sec"),
  ];
  for (const d of secs) {
    d.addEventListener("toggle", () => {
      if (d.open) enterSub(d);
      else exitSub();
    });
  }
  // Ouverture/fermeture du panneau = bascule de l'attribut `hidden`.
  // À l'ouverture : sections toutes fermées → la vue montre la liste ;
  // un appelant qui pose `sec.open = true` dans le même tour (openPairSettings)
  // obtient le sous-écran direct — on ne détruit pas cette intention.
  // À la fermeture : purge de l'état sous-écran (dont l'entrée nav
  // « settings-sec ») pour ne pas laisser de waypoint mort dans la pile.
  new MutationObserver(() => {
    if (view.hidden) {
      for (const d of secs) d.open = false;
      exitSub();
    } else if (!openSec()) {
      for (const d of secs) d.open = false;
    }
  }).observe(view, { attributes: true, attributeFilter: ["hidden"] });
}
