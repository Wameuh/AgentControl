// ---------------------------------------------------------------------------
// Actions de message par appui long (S4) : sur Android les boutons survol
// (.pin-tag, .msg-tts, .md-copy) sont masqués — l'appui long sur un
// message ouvre #evctx avec les mêmes entrées, en déléguant aux boutons
// réels (jamais de logique dupliquée). Avec une sélection, openSelCtx
// garde « Copier » et reçoit ces entrées en plus.
// ---------------------------------------------------------------------------

import { $, copyText } from "../core/dom";
import { t } from "../i18n";
import { hideMenu, menuBtn, placeMenu } from "../ui/ctxmenu";

/** Appui long sans sélection sur un .ev.msg → menu d'actions du message. */
export function openMsgCtx(x: number, y: number, msgEl: HTMLElement) {
  const ctx = $("evctx");
  ctx.innerHTML = "";
  msgMenuItems(ctx, msgEl);
  placeMenu(ctx, "evctx", x, y);
}

/** Entrées « message » du #evctx — partagées entre openMsgCtx (appui
 *  long nu) et openSelCtx (appui long avec sélection). Chaque action
 *  délègue au bouton réel du message : un bouton en display:none
 *  répond quand même à .click(). */
export function msgMenuItems(ctx: HTMLElement, msgEl: HTMLElement) {
  menuBtn(ctx, t("msg_copy"), "", () => {
    hideMenu(ctx, "evctx");
    const clone = msgEl.cloneNode(true) as HTMLElement;
    // Boutons d'action et affordances — pas du texte de message.
    for (const b of clone.querySelectorAll(
      ".pin-tag,.msg-tts,.md-copy,.md-wrap,.um-toggle",
    )) b.remove();
    void copyText((clone.textContent ?? "").trim());
  });
  const pin = msgEl.querySelector<HTMLElement>(".pin-tag");
  if (pin) {
    // Le titre du pin-tag suit l'état épinglé — source unique du libellé.
    menuBtn(ctx, pin.title || t("pin_add"), "", () => {
      hideMenu(ctx, "evctx");
      pin.click();
    });
  }
  const tts = msgEl.querySelector<HTMLElement>(".msg-tts");
  if (tts) {
    menuBtn(ctx, tts.title || t("tts_speak"), "", () => {
      hideMenu(ctx, "evctx");
      tts.click();
    });
  }
}
