// ---------------------------------------------------------------------------
// Pipeline de formatage : markdown minimal sûr, linkify, coloration
// syntaxique, diffs, tailles/compteurs. Fonctions pures (DOM pour esc()).
// ---------------------------------------------------------------------------

import { esc, q } from "./dom";

// Option « images en ligne » (Settings, défaut activé) : les URLs http(s)
// finissant par une extension image sont rendues en <img> cliquable.
export const IMG_URL_RE = /\.(?:png|jpe?g|gif|webp|avif|bmp|svg|ico)(?:[?#][^\s<]*)?$/i;
export const inlineImages = () => localStorage.getItem("ac-inline-images") !== "0";

// Chemins de fichiers cliquables → panneau latéral (read_file côté démon)
const FILE_RE =
  /((?:\/[\w.\-~]+){2,}|[\w.\-]+(?:\/[\w.\-]+){2,}|[\w.\-/]+\.(?:rs|ts|tsx|js|jsx|py|toml|json|md|html?|css|ya?ml|sh|c|cpp|h|hpp|go|java|rb|txt|log|svg|cfg|ini|sql|vue|svelte|lock|service|png|jpe?g|gif|webp|bmp|ico|avif|mp4|m4v|mov|webm|mkv|ogv|avi))(?::(\d+))?/g;

export function linkify(escaped: string): string {
  // URLs d'abord → vrais liens (placeholder \x00U pour ne pas être re-matchées
  // par FILE_RE ni corrompues par le markup)
  const urls: string[] = [];
  let work = escaped.replace(/https?:\/\/[^\s<\x00]+/g, (m) => {
    const clean = m.replace(/(?:&(?:quot|#39|lt|gt);|[.,;:!?)\]])+$/, "");
    const inner =
      inlineImages() && IMG_URL_RE.test(clean)
        ? `<img class="md-img" src="${q(clean)}" alt="${q(clean)}" loading="lazy">`
        : clean;
    urls.push(
      `<a class="ulink" href="${q(clean)}" target="_blank" rel="noopener noreferrer">${inner}</a>`,
    );
    return `\x00U${urls.length - 1}\x00${m.slice(clean.length)}`;
  });
  work = work.replace(FILE_RE, (m, p: string) => {
    const clean = p.replace(/[.,;:!?'")\]]+$/, "");
    // lien = chemin sans ponctuation finale ; le reste (:ligne, ponct.) reste dehors
    return `<span class="flink" data-path="${q(clean)}">${clean}</span>${m.slice(clean.length)}`;
  });
  return work.replace(/\x00U(\d+)\x00/g, (_m, i: string) => urls[+i]);
}

/** Colorise les lignes de diff (+add / -del / @@ hunks) dans le texte. */
export function diffFmt(escaped: string): string {
  if (!/^(?:\+{3}|-{3}|@@|[-+]\S)/m.test(escaped)) return escaped;
  return escaped
    .split("\n")
    .map((l) => {
      const cls =
        l.startsWith("@@") || l.startsWith("+++") || l.startsWith("---")
          ? "df-hd"
          : l.startsWith("+")
            ? "df-add"
            : l.startsWith("-")
              ? "df-del"
              : "";
      return cls ? `<span class="${cls}">${l}</span>` : l;
    })
    .join("\n");
}

/** Coloration syntaxique légère par extension (kw/str/com/num). */
const SYN_KWS: Record<string, string> = {
  rs: "fn let mut pub mod use impl struct enum trait match loop where dyn ref box unsafe const static type as in for while if else return break continue move self Self super crate true false",
  py: "def class import from as if elif else for while return pass with try except finally raise yield lambda global nonlocal assert is in not and or None True False self",
  ts: "function const let var if else for while of in return break continue new class extends import export from as async await try catch finally throw switch case default typeof instanceof delete void this null undefined true false public private protected readonly interface type enum implements satisfies",
  sh: "if then else elif fi for while do done case esac function in echo exit return local export source readonly unset shift",
  c: "auto break case char const continue default do double else enum extern float for goto if int long register return short signed sizeof static struct switch typedef union unsigned void volatile while bool inline restrict",
  java: "abstract boolean break byte case catch char class const continue default do double else enum extends final finally float for if implements import instanceof int interface long native new package private protected public return short static strictfp super switch synchronized this throw throws transient try void volatile while true false null var record sealed permits",
};
const SYN_EXT: Record<string, string> = {
  rs: "rs", py: "py", ts: "ts", tsx: "ts", js: "ts", jsx: "ts", mjs: "ts",
  sh: "sh", bash: "sh", zsh: "sh",
  c: "c", h: "c", cpp: "c", cxx: "c", cc: "c", hpp: "c", hxx: "c",
  java: "java", kt: "java", kts: "java",
};
export function highlight(code: string, ext: string): string {
  const lang = SYN_EXT[ext] ?? "";
  const kws = (SYN_KWS[lang] ?? "").split(" ").filter(Boolean);
  const comSrc =
    lang === "rs" || lang === "ts" || lang === "c" || lang === "java"
      ? "\\/\\/[^\\n]*"
      : "#[^\\n]*";
  const strSrc = '"[^"\\n]*"|\'[^\'\\n]*\'|`[^`\\n]*`';
  const kwSrc = kws.length ? `\\b(?:${kws.join("|")})\\b` : "a^"; // a^ = jamais
  const re = new RegExp(`(${comSrc})|(${strSrc})|(${kwSrc})|(\\b\\d+(?:\\.\\d+)?\\b)`, "g");
  return esc(code).replace(
    re,
    (m, c, s, k, n) =>
      c ? `<span class="syn-com">${m}</span>`
      : s ? `<span class="syn-str">${m}</span>`
      : k ? `<span class="syn-kw">${m}</span>`
      : `<span class="syn-num">${m}</span>`,
  );
}

/** Rendu markdown minimal et sûr : échappement HTML d'abord, blocs de code
 *  et liens extraits en placeholders (ni linkify ni inline-md ne les corrompt). */
export function mdRender(src: string): string {
  const blocks: string[] = [];
  const stash = (html: string) => `\x00B${blocks.push(html) - 1}\x00`;

  // 1) blocs ```lang (fermé ou non — fin de texte tolérée) + code inline `x`
  let work = src
    .replace(/```(\w*)[ \t]*\n?([\s\S]*?)(?:```|$)/g, (_m, lang: string, code: string) =>
      stash(`<pre class="md-code">${highlight(code.replace(/\n$/, ""), lang.toLowerCase())}</pre>`))
    .replace(/`([^`\n]+)`/g, (_m, c: string) => stash(`<code class="md-ic">${esc(c)}</code>`));

  // 2) échappement du reste
  work = esc(work);

  // 3) images ![alt](https://…) avant les liens (le ![…] matcherait […](…))
  work = work.replace(
    /!\[([^\]]*)\]\((https?:\/\/[^)\s]+)\)/g,
    (_m, alt: string, url: string) =>
      inlineImages()
        ? stash(
            `<a href="${q(url)}" target="_blank" rel="noopener noreferrer">` +
              `<img class="md-img" src="${q(url)}" alt="${q(alt)}" loading="lazy"></a>`,
          )
        : stash(
            `<a href="${q(url)}" target="_blank" rel="noopener noreferrer">${alt || url}</a>`,
          ),
  );

  // 4) liens markdown [t](https://…) → placeholder (avant linkify)
  work = work.replace(
    /\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g,
    (_m, tx: string, url: string) =>
      stash(`<a href="${q(url)}" target="_blank" rel="noopener noreferrer">${tx}</a>`),
  );

  // 5) inline : gras / italique / barré
  work = work
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\b__([^_]+)__\b/g, "<strong>$1</strong>")
    .replace(/(^|[^\w*])\*([^*\n]+)\*(?![*\w])/g, "$1<em>$2</em>")
    .replace(/(?<![\w_])_([^_\n]+)_(?![\w_])/g, "<em>$1</em>")
    .replace(/~~([^~]+)~~/g, "<del>$1</del>");

  // 6) chemins de fichiers cliquables (hors blocs extraits)
  work = linkify(work);

  // 7) structure de blocs : headers, hr, citations, tables, listes, paragraphes
  const lines = work.split("\n");
  const isBlockStart = (l: string) =>
    /^\x00B\d+\x00\s*$/.test(l.trim()) ||
    /^#{1,6}\s/.test(l) ||
    /^\s*(?:\*{3,}|-{3,}|_{3,})$/.test(l) ||
    /^&gt;/.test(l) ||
    /^\s*(?:[-*+]|\d+[.)])\s+/.test(l);
  const cellSplit = (l: string) =>
    l.replace(/^\s*\||\|\s*$/g, "").split("|").map((c) => c.trim());
  const out: string[] = [];
  let i = 0;
  while (i < lines.length) {
    const l = lines[i];
    if (!l.trim()) { i++; continue; }
    if (/^\x00B\d+\x00\s*$/.test(l.trim())) { out.push(l.trim()); i++; continue; }
    const h = /^(#{1,6})\s+(.*)/.exec(l);
    if (h) { out.push(`<div class="mdh mdh${h[1].length}">${h[2]}</div>`); i++; continue; }
    if (/^\s*(?:\*{3,}|-{3,}|_{3,})$/.test(l)) { out.push("<hr>"); i++; continue; }
    if (/^&gt;/.test(l)) {
      const q: string[] = [];
      while (i < lines.length && /^&gt;/.test(lines[i])) {
        q.push(lines[i].replace(/^&gt;\s?/, ""));
        i++;
      }
      out.push(`<blockquote>${q.join("<br>")}</blockquote>`);
      continue;
    }
    // table : ligne |a|b| suivie d'une ligne |---|
    if (l.includes("|") && i + 1 < lines.length && /^\s*\|?[\s:|-]+\|?[\s:|-]*$/.test(lines[i + 1]) && lines[i + 1].includes("-") && lines[i + 1].includes("|")) {
      const hdr = cellSplit(l);
      i += 2;
      const rows: string[][] = [];
      while (i < lines.length && lines[i].includes("|") && lines[i].trim()) {
        rows.push(cellSplit(lines[i]));
        i++;
      }
      out.push(
        `<table class="mdt"><thead><tr>${hdr.map((c) => `<th>${c}</th>`).join("")}</tr></thead>` +
          `<tbody>${rows.map((r) => `<tr>${r.map((c) => `<td>${c}</td>`).join("")}</tr>`).join("")}</tbody></table>`,
      );
      continue;
    }
    const lm = /^(\s*)([-*+]|\d+[.)])\s+(.*)/.exec(l);
    if (lm) {
      const ordered = /^\d/.test(lm[2]);
      const items: string[] = [];
      while (i < lines.length) {
        const m2 = /^(\s*)([-*+]|\d+[.)])\s+(.*)/.exec(lines[i]);
        if (!m2) break;
        items.push(`<li${m2[1].length >= 2 ? ' class="md-sub"' : ""}>${m2[3]}</li>`);
        i++;
      }
      out.push(`<${ordered ? "ol" : "ul"} class="mdl">${items.join("")}</${ordered ? "ol" : "ul"}>`);
      continue;
    }
    const para: string[] = [];
    while (i < lines.length && lines[i].trim() && !isBlockStart(lines[i])) {
      para.push(diffFmt(lines[i]));
      i++;
    }
    out.push(`<p>${para.join("<br>")}</p>`);
  }
  // Placeholders imbriqués ([![](img)](url)) : la valeur insérée peut contenir
  // un \x00B — résoudre jusqu'à point fixe (index invalide : laissé tel quel).
  let html = out.join("");
  for (;;) {
    const next = html.replace(/\x00B(\d+)\x00/g, (m, n: string) =>
      +n < blocks.length ? blocks[+n] : m,
    );
    if (next === html) return html;
    html = next;
  }
}

/** Extensions affichées en aperçu image (data URL — pas de script dans <img>). */
export const IMG_MIME: Record<string, string> = {
  png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif",
  webp: "image/webp", bmp: "image/bmp", ico: "image/x-icon", avif: "image/avif",
  svg: "image/svg+xml",
};

/** Extensions ouvertes dans le lecteur vidéo (schéma acvideo — ranges
 *  streamées depuis le démon, pas de chargement complet du fichier). */
export const VID_MIME: Record<string, string> = {
  mp4: "video/mp4", m4v: "video/mp4", mov: "video/quicktime",
  webm: "video/webm", ogv: "video/ogg", mkv: "video/x-matroska",
  avi: "video/x-msvideo",
};

export const fmtSize = (n: number) =>
  n >= 1 << 20 ? `${(n / (1 << 20)).toFixed(1)} MB`
  : n >= 1024 ? `${(n / 1024).toFixed(0)} kB`
  : `${n} B`;

export const fmtTok = (n: number) =>
  n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${Math.round(n / 1e3)}k` : String(n);
