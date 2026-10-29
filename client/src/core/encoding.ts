// ---------------------------------------------------------------------------
// Encodages texte du viewer/éditeur de fichiers : décodage TextDecoder
// streamé (les séquences multi-octets coupées entre tranches réseau sont
// assemblées), encodage maison (TextEncoder ne couvre que l'UTF-8),
// détection BOM + sniff binaire. Le démon reste agnostique : il ne voit
// que des octets (read_file paginé / write_file en base64).
// ---------------------------------------------------------------------------

export type Encoding = "utf-8" | "windows-1252" | "utf-16le" | "utf-16be";

export const ENCODINGS: { id: Encoding; label: string }[] = [
  { id: "utf-8", label: "UTF-8" },
  // TextDecoder("iso-8859-1") = windows-1252 per WHATWG — un seul choix
  // pour les deux noms.
  { id: "windows-1252", label: "Windows-1252 / Latin-1" },
  { id: "utf-16le", label: "UTF-16 LE" },
  { id: "utf-16be", label: "UTF-16 BE" },
];

/** Encodage par défaut (settings) — inconnu → utf-8. */
export function defaultEncoding(): Encoding {
  const v = localStorage.getItem("ac-encoding") ?? "";
  return ENCODINGS.some((e) => e.id === v) ? (v as Encoding) : "utf-8";
}

/** Décodeur streamé réutilisé tranche par tranche (le BOM de tête est
 *  consommé par TextDecoder — il est ré-émis à l'encodage si présent). */
export function makeDecoder(enc: Encoding): (b: Uint8Array, last?: boolean) => string {
  const d = new TextDecoder(enc);
  return (b, last = false) => d.decode(b, { stream: !last });
}

/** BOM → encodage imposé (prioritaire sur le défaut). */
export function sniffBom(b: Uint8Array): Encoding | null {
  if (b[0] === 0xef && b[1] === 0xbb && b[2] === 0xbf) return "utf-8";
  if (b[0] === 0xff && b[1] === 0xfe) return "utf-16le";
  if (b[0] === 0xfe && b[1] === 0xff) return "utf-16be";
  return null;
}

// Table haute windows-1252 (0x80–0x9F) — inverse pour encoder.
const W1252_TABLE: [number, number][] = [
  [0x80, 0x20ac], [0x82, 0x201a], [0x83, 0x0192], [0x84, 0x201e],
  [0x85, 0x2026], [0x86, 0x2020], [0x87, 0x2021], [0x88, 0x02c6],
  [0x89, 0x2030], [0x8a, 0x0160], [0x8b, 0x2039], [0x8c, 0x0152],
  [0x8e, 0x017d], [0x91, 0x2018], [0x92, 0x2019], [0x93, 0x201c],
  [0x94, 0x201d], [0x95, 0x2022], [0x96, 0x2013], [0x97, 0x2014],
  [0x98, 0x02dc], [0x99, 0x2122], [0x9a, 0x0161], [0x9b, 0x203a],
  [0x9c, 0x0153], [0x9e, 0x017e], [0x9f, 0x0178],
];
const W1252_REV = new Map(W1252_TABLE.map(([b, c]) => [c, b]));
const UTF8_BOM = [0xef, 0xbb, 0xbf];

/** Encode `text` dans l'encodage choisi ; `bom` ré-écrit la marque
 *  initiale si le fichier en avait une (utf-16 → quasi-systématique). */
export function encode(enc: Encoding, text: string, bom = false): Uint8Array {
  if (enc === "utf-8") {
    const body = new TextEncoder().encode(text);
    if (!bom) return body;
    const out = new Uint8Array(3 + body.length);
    out.set(UTF8_BOM);
    out.set(body, 3);
    return out;
  }
  if (enc === "windows-1252") {
    const out = new Uint8Array(text.length);
    for (let i = 0; i < text.length; i++) {
      const c = text.charCodeAt(i);
      out[i] = c < 0x100 ? c : (W1252_REV.get(c) ?? 0x3f);
    }
    return out;
  }
  const le = enc === "utf-16le";
  const out = new Uint8Array(text.length * 2 + (bom ? 2 : 0));
  let o = 0;
  if (bom) {
    out[o++] = le ? 0xff : 0xfe;
    out[o++] = le ? 0xfe : 0xff;
  }
  for (let i = 0; i < text.length; i++) {
    // charCodeAt rend les unités UTF-16 (surrogates incluses) — l'ordre
    // des octets est le seul paramètre.
    const c = text.charCodeAt(i);
    out[o++] = le ? c & 0xff : c >> 8;
    out[o++] = le ? c >> 8 : c & 0xff;
  }
  return out;
}

/** Heuristique binaire sur le texte décodé : contrôles (hors \t\n\r\f)
 *  ou U+FFFD > ~2 % → pas un fichier texte éditable. Fonctionne pour les
 *  UTF-16 (le sniff d'octets nuls les prendrait tous pour du binaire). */
export function sniffBinary(text: string): boolean {
  const n = Math.min(text.length, 8192);
  if (!n) return false;
  let ctl = 0;
  for (let i = 0; i < n; i++) {
    const c = text.charCodeAt(i);
    if (c === 0xfffd || c === 0 || (c < 0x20 && c !== 0x09 && c !== 0x0a && c !== 0x0d && c !== 0x0c)) {
      ctl++;
    }
  }
  return ctl / n > 0.02;
}

// Base64 sans dépendance (chunks de 32 Kio — spread limit de fromCharCode).
export function b64decode(s: string): Uint8Array {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function b64encode(b: Uint8Array): string {
  let s = "";
  for (let i = 0; i < b.length; i += 0x8000) {
    s += String.fromCharCode(...b.subarray(i, i + 0x8000));
  }
  return btoa(s);
}
