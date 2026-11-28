// ---------------------------------------------------------------------------
// Feuille « connexion » de la puce d'accueil (N1) : actions réseau sans
// #conn-bar (masqué sur mobile) — reconnecter le chip d'alerte, nouvelle
// connexion, appairage et capacités agents délèguent chacun au bouton
// d'origine. Bottom-sheet via nav.sheet comme le ⋮ de l'accueil.
// ---------------------------------------------------------------------------

import { $ } from "../core/dom";
import { t } from "../i18n";
import { nav, type SheetItem } from "../ui/nav";

export interface ConnSheetDeps {
  /** « Appairer un appareil » : ouvre les Paramètres dépliés sur la
   *  section Connexion (même motif que les hints STT/TTS de main.ts). */
  openPairSettings: () => void;
}

let deps: ConnSheetDeps;

export function initConnSheet(d: ConnSheetDeps) {
  deps = d;
}

/** Actions de la puce #m-conn : « Reconnecter » n'apparaît que quand le
 *  chip d'alerte connexion l'est aussi (conn down/en essai), « Capacités
 *  des agents » que si le bouton caché est affiché (capability connue). */
export function openConnSheet() {
  const items: SheetItem[] = [];
  if (!($("conn-chip") as HTMLElement).hidden) {
    items.push({
      icon: "refresh",
      label: t("home_conn_retry"),
      fn: () => $("conn-chip").click(),
    });
  }
  items.push(
    { icon: "plus", label: t("home_conn_new"), fn: () => $("btn-connect").click() },
    { icon: "qr", label: t("home_conn_pair"), fn: () => deps.openPairSettings() },
  );
  if (!($("btn-agent-caps") as HTMLElement).hidden) {
    items.push({
      icon: "sliders",
      label: t("agent_caps_h"),
      fn: () => $("btn-agent-caps").click(),
    });
  }
  nav.sheet(items);
}
