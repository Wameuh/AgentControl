// ---------------------------------------------------------------------------
// Échappement HTML pur (sans DOM) — dom.ts les ré-exporte pour l'existant,
// format.ts les importe directement pour rester chargeable en test Node
// (type stripping : la chaîne dom → bridge → @tauri-apps n'y est pas
// résolvable).
// ---------------------------------------------------------------------------

/** Texte → HTML sûr : & < > uniquement (équivalent de
 *  `div.textContent = s; div.innerHTML` — les guillemets n'ont pas à
 *  l'être dans un nœud texte). */
export function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

// Pour les valeurs insérées dans un attribut "…" : esc() n'échappe pas '"'.
export const q = (s: string) => s.replace(/"/g, "&quot;");
