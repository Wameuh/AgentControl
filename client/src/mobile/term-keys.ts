// ---------------------------------------------------------------------------
// Rangée de touches du terminal (S5) : sous Android le clavier virtuel
// n'offre ni Échap ni Ctrl ni flèches — la rangée les fournit. Chaque
// touche envoie sa séquence au terminal courant ; Ctrl est un verrou
// (tap = une fois, double-tap = maintenu, re-tap = relâché).
// pointerdown preventDefault : sans ça le focus quitte le textarea
// xterm et le clavier se ferme à chaque appui.
// ---------------------------------------------------------------------------

import { $, esc } from "../core/dom";
import { isAndroid } from "../core/chrome";
import { t } from "../i18n";
import type { TermPanel } from "../features/terminal";

const KEYS: { label: string; seq: string | "ctrl" }[] = [
  { label: "esc", seq: "\x1b" },
  { label: "⇥", seq: "\t" },
  { label: "ctrl", seq: "ctrl" },
  { label: "←", seq: "\x1b[D" },
  { label: "↑", seq: "\x1b[A" },
  { label: "↓", seq: "\x1b[B" },
  { label: "→", seq: "\x1b[C" },
  { label: "|", seq: "|" },
  { label: "~", seq: "~" },
  { label: "/", seq: "/" },
  { label: "-", seq: "-" },
];

export function initTermKeys(term: TermPanel) {
  if (!isAndroid()) return;
  const bar = $("term-keys");
  let ctrl: "off" | "once" | "held" = "off";
  let lastTap = 0;
  let ctrlBtn: HTMLButtonElement;

  const setCtrl = (next: typeof ctrl) => {
    ctrl = next;
    term.setCtrl(ctrl);
    ctrlBtn.dataset.state = ctrl;
  };

  for (const k of KEYS) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "tkey" + (k.seq === "ctrl" ? " tkey-ctrl" : "");
    b.dataset.state = "off";
    const label = k.seq === "ctrl" ? t("term_key_ctrl")
      : k.seq === "\x1b" ? t("term_key_esc") : k.label;
    b.innerHTML = esc(label);
    if (k.seq === "ctrl") {
      ctrlBtn = b;
      b.onclick = () => {
        // Double-tap (<350 ms) = maintenu ; tap = une fois / relâcher.
        const now = Date.now();
        const dbl = now - lastTap < 350;
        lastTap = now;
        setCtrl(dbl ? (ctrl === "held" ? "off" : "held")
          : (ctrl === "off" ? "once" : "off"));
      };
    } else {
      b.onclick = () => {
        term.sendToCurrent(k.seq as string);
        // « Une fois » se relâche après n'importe quelle touche utile.
        if (ctrl === "once") setCtrl("off");
      };
    }
    b.addEventListener("pointerdown", (e) => e.preventDefault());
    bar.appendChild(b);
  }
}
