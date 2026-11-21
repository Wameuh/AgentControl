// ---------------------------------------------------------------------------
// Indices de découverte mobile : un toast « info » montré une seule fois
// par clé (persisté en localStorage sous "ac-hint-<key>"), pour les
// gestes invisibles à l'écran (appui long…).
// ---------------------------------------------------------------------------

import { notify } from "../core/dom";

/** Affiche `msg` une seule fois par `key` : si la marque est absente le
 *  toast part et la clé est posée. localStorage peut être indisponible
 *  (mode privé, quota) → lecture/écriture en try/catch, l'indice est un
 *  confort, jamais un point de panne. */
export function hintOnce(key: string, msg: string) {
  const k = `ac-hint-${key}`;
  try {
    if (localStorage.getItem(k)) return;
    notify("info", msg);
    localStorage.setItem(k, "1");
  } catch { /* noop */ }
}
