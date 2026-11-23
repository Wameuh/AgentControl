// ---------------------------------------------------------------------------
// Dialogue « modifications non enregistrées » partagé (#dlg-unsaved) :
// Paramètres et paramètres de composant demandent avant de jeter des
// changements en attente — rester / ignorer / enregistrer. Esc, back
// Android ou fermeture sans bouton = « rester » : jamais de perte muette.
// ---------------------------------------------------------------------------

import { $ } from "./dom";
import { t } from "../i18n";

export type UnsavedChoice = "stay" | "discard" | "save";

let pending: ((c: UnsavedChoice) => void) | null = null;

function settle(c: UnsavedChoice) {
  const r = pending;
  pending = null;
  const dlg = $("dlg-unsaved") as HTMLDialogElement;
  if (dlg.open) dlg.close();
  r?.(c);
}

/** Ouvre le dialogue et résout le choix. `saveLabel` renomme le bouton
 *  d'enregistrement (« Appliquer » pour une config démon). Un appel
 *  pendant qu'un dialogue attend déjà réutilise la même réponse. */
export function askUnsaved(msg: string, saveLabel?: string): Promise<UnsavedChoice> {
  const dlg = $("dlg-unsaved") as HTMLDialogElement;
  if (pending && dlg.open) {
    return new Promise((res) => {
      const prev = pending!;
      pending = (c) => {
        prev(c);
        res(c);
      };
    });
  }
  $("unsaved-msg").textContent = msg;
  $("us-save").textContent = saveLabel ?? t("dirty_save");
  return new Promise((res) => {
    pending = res;
    dlg.showModal();
    ($("us-stay") as HTMLButtonElement).focus();
  });
}

/** Branche les boutons une fois au boot. */
export function initUnsavedDialog() {
  $("us-stay").onclick = () => settle("stay");
  $("us-discard").onclick = () => settle("discard");
  $("us-save").onclick = () => settle("save");
  ($("dlg-unsaved") as HTMLDialogElement).addEventListener("close", () => {
    if (pending) settle("stay");
  });
}
