// ---------------------------------------------------------------------------
// Menu « Application » mobile : entrées du ⋮ de la titlebar — partagées
// entre ce ⋮ (hors session) et la section Application de la feuille
// Session (en session, titlebar masquée sous m-in-session).
// ---------------------------------------------------------------------------

import { $ } from "../core/dom";
import { t } from "../i18n";
import type { SheetItem } from "../ui/nav";

/** Mêmes entrées et même gate capability que les boutons titlebar
 *  masqués sur mobile. `compsCapable` est injecté (ComponentsPanel). */
export function appMenuItems(
  compsCapable: () => boolean,
  openUpdates: () => void,
): SheetItem[] {
  const items: SheetItem[] = [
    { icon: "map", label: t("map_title"), fn: () => $("tb-map").click() },
    { icon: "scroll", label: t("logs"), fn: () => $("tb-logs").click() },
  ];
  if (compsCapable())
    items.push({ icon: "tool", label: t("comp_h"), fn: () => $("tb-components").click() });
  items.push(
    { icon: "settings", label: t("settings_title"), fn: () => $("tb-settings").click() },
    { icon: "up", label: t("upd_menu"), fn: openUpdates },
    { icon: "info", label: t("about_title"), fn: () => $("tb-about").click() },
  );
  return items;
}
