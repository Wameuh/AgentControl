// ---------------------------------------------------------------------------
// Garde-fou des dialogues modaux (<dialog> top-layer).
//
// Un showModal rend TOUTE la page inerte : un dialogue resté ouvert pendant
// une opération longue — ou invisible sous un autre — gèle l'UI sans cause
// apparente (« impossible d'écrire dans la zone de saisie »).
//
// - Journalise open/close de chaque dialogue dans client.log (qui, quand,
//   depuis combien de temps) — indispensable pour diagnostiquer un gel.
// - Watchdog périodique : dialogue `open` mais non rendu (hors viewport /
//   display:none) → fermé ; `inert` résiduel alors que #settings-view est
//   masqué → retiré. Chaque réparation est loguée en warn.
// ---------------------------------------------------------------------------

import { $, logUi } from "./dom";

/** Horodatage d'ouverture par dialogue — pour loguer la durée à la fermeture. */
const openedAt = new Map<string, number>();

/** Dialogue `open` mais non rendu : display:none ou entièrement hors viewport.
 *  Il reste dans le top layer et garde la page inerte — cas « app normale
 *  qui n'accepte plus rien ». */
function invisible(d: HTMLDialogElement): boolean {
  const r = d.getBoundingClientRect();
  if (r.width === 0 && r.height === 0) return true;
  return (
    r.bottom < 0 || r.right < 0 ||
    r.top > window.innerHeight || r.left > window.innerWidth
  );
}

function watch() {
  for (const d of document.querySelectorAll<HTMLDialogElement>("dialog[open]")) {
    if (!invisible(d)) continue;
    logUi("warn", `modal-watchdog: #${d.id} ouvert mais invisible → fermé`);
    d.close();
  }
  // `inert` n'a de raison d'être que sous un overlay ouvert de #chat
  // (#settings-view, #conn-view ou #map-view — enfants directs, posés
  // par setChatInert) : s'ils sont tous masqués par un chemin qui saute
  // close(), les éléments recouverts restent inertes → composer/clics
  // morts sans rien à l'écran.
  const overlayUp =
    ($("settings-view") as HTMLElement | null)?.hidden === false ||
    ($("conn-view") as HTMLElement | null)?.hidden === false ||
    ($("map-view") as HTMLElement | null)?.hidden === false ||
    ($("components-view") as HTMLElement | null)?.hidden === false;
  if (!overlayUp) {
    for (const el of document.querySelectorAll<HTMLElement>("#chat > [inert]")) {
      el.inert = false;
      logUi("warn", `modal-watchdog: inert résiduel retiré de #${el.id || el.tagName}`);
    }
  }
}

/** À appeler une fois au boot. */
export function initModalGuard() {
  const origShowModal = HTMLDialogElement.prototype.showModal;
  HTMLDialogElement.prototype.showModal = function (this: HTMLDialogElement) {
    origShowModal.call(this);
    if (this.id) openedAt.set(this.id, Date.now()); // close skip les sans-id
    const under = document.querySelectorAll("dialog[open]").length - 1;
    logUi(
      "info",
      `modal +#${this.id || "(anonyme)"}${under > 0 ? ` (${under} autre(s) dessous)` : ""}`,
    );
  };
  // `close` est un event capture : natif (Échap, method="dialog") comme
  // programmatique, sans patcher close().
  document.addEventListener(
    "close",
    (e) => {
      const d = e.target as HTMLDialogElement;
      if (!(d instanceof HTMLDialogElement) || !d.id) return;
      const t0 = openedAt.get(d.id);
      openedAt.delete(d.id);
      logUi(
        "info",
        `modal -#${d.id}` + (t0 ? ` (ouvert ${Math.round((Date.now() - t0) / 1000)}s)` : ""),
      );
    },
    true,
  );
  setInterval(watch, 2000);
}
