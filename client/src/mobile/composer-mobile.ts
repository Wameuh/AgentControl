// ---------------------------------------------------------------------------
// Composer mobile (C1) : + intégré au champ, micro quand le champ est vide,
// bouton Envoyer rond sinon ; le steer passe par appui long sur Envoyer.
// `m-input-empty` sur body pilote le swap micro ↔ envoyer dans session.css.
// ---------------------------------------------------------------------------

import { $ } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { t } from "../i18n";
import { nav } from "../ui/nav";
import { bindCtx } from "../ui/ctxmenu";
import type { Composer } from "../features/composer";

/** Champ vide + aucune pièce jointe en attente → micro visible à la place
 *  du bouton Envoyer (classe body lue par les règles C1). */
export function syncSendMic() {
  if (!isAndroid()) return;
  const inp = $("input") as HTMLTextAreaElement;
  const attach = $("attach-bar") as HTMLElement;
  document.body.classList.toggle(
    "m-input-empty", !inp.value.trim() && attach.hidden);
}

export function initComposerMobile(composer: Composer) {
  if (!isAndroid()) return;
  // Appui long sur Envoyer → feuille steer/file — seulement quand le mode
  // alternatif existe (btn-alt visible en desktop, masqué par CSS mobile).
  bindCtx($("btn-send"), () => {
    if (($("btn-alt") as HTMLElement).hidden) return;
    nav.sheet([
      { icon: "steer", label: t("send_steer_now"), fn: () => composer.sendWith("steer") },
      { icon: "clock", label: t("send_queue"), fn: () => composer.sendWith("normal") },
    ]);
  });
  // La barre de chips (pièces jointes) bascule hidden sans passer par
  // fitInput — observer pour garder m-input-empty à jour.
  new MutationObserver(syncSendMic).observe($("attach-bar"), {
    attributes: true,
    attributeFilter: ["hidden"],
  });
  // État initial (le champ peut être pré-rempli au boot — brouillon).
  syncSendMic();
}
