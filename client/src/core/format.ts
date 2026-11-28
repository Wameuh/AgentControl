// ---------------------------------------------------------------------------
// Pipeline de formatage : markdown minimal sûr, linkify, coloration
// syntaxique, diffs, tailles/compteurs. Fonctions pures (DOM pour esc()).
// ---------------------------------------------------------------------------

import { esc, q } from "./escape.ts";
import { t } from "../i18n/index.ts";

// Option « images en ligne » (Settings) : politique de chargement des
// images DISTANTES (http/https) dans les rendus markdown et linkify —
// une URL d'image peut être une injection de prompt (fuite de données et
// pistage par la requête elle-même), donc jamais de fetch sans geste :
//  - "never"  : lien seul, aucun chargement possible ;
//  - "click"  : défaut — placeholder cliquable « Image: host — show »,
//               l'<img> n'est inséré qu'au clic ;
//  - "always" : <img> dès l'affichage (comportement historique).
// Persistance ac-inline-images — compat legacy : "0" → never,
// "1" → always, absent → click. Les images data: et celles servies par
// le démon ne passent pas par ce chemin (pas de match http/https).
export type ImgMode = "never" | "click" | "always";
export const IMG_URL_RE = /\.(?:png|jpe?g|gif|webp|avif|bmp|svg|ico)(?:[?#][^\s<]*)?$/i;
export const imgMode = (): ImgMode => {
  const v = localStorage.getItem("ac-inline-images");
  if (v === "never" || v === "always" || v === "click") return v;
  if (v === "0") return "never";
  if (v === "1") return "always";
  return "click";
};

/** Domaines de confiance (réglage CSV ac-img-hosts) : chargés sans clic
 *  en mode « click » — match exact ou sous-domaine. */
export const imgTrustedHosts = (): string[] =>
  (localStorage.getItem("ac-img-hosts") ?? "")
    .split(",")
    .map((h) => h.trim().toLowerCase())
    .filter(Boolean);

/** Hôte d'une URL distante — `hostname` (sans port) : la whitelist
 *  fait confiance à un domaine, pas à un couple hôte:port —
 *  `example.com` couvre `example.com:8443` et ses sous-domaines. */
const imgHost = (url: string): string => {
  try {
    return new URL(url).hostname.toLowerCase();
  } catch {
    return "";
  }
};

/** Décision de rendu d'une image distante : "img" = <img> direct
 *  (always, hôte de confiance ou même origine « 'self' » — le daemon
 *  sert le client, une image à la même adresse n'est pas distante,
 *  comme data:), "ph" = placeholder cliquable, "none" = lien seul.
 *  La whitelist s'applique au mode click — never reste absolu pour
 *  les hôtes distants (aucun fetch, quel que soit l'hôte). */
const imgRender = (url: string): "img" | "ph" | "none" => {
  const m = imgMode();
  if (m === "always") return "img";
  try {
    // Origine complète (schéma + hôte + port) : https://même-hôte sur
    // un client http n'est PAS « 'self' » — c'est une origine différente.
    if (new URL(url).origin === location.origin) return "img";
  } catch {
    /* URL illisible → politique standard. */
  }
  const host = imgHost(url);
  if (m === "never" || !host) return "none";
  return imgTrustedHosts().some((d) => host === d || host.endsWith(`.${d}`))
    ? "img"
    : "ph";
};

/** Placeholder « Image: host — show » inséré à la place de l'<img> en
 *  mode click — data-src porte l'URL, le clic la remplace par l'image
 *  (expandImgPh, listener délégué chat + viewer). */
const imgPh = (url: string, alt: string): string =>
  `<button type="button" class="md-img-ph" data-src="${q(url)}"` +
  ` data-alt="${q(alt)}">${esc(t("img_ph", { host: imgHost(url) || url }))}</button>`;

/** Lien simple remplaçant un placeholder ou une image retirée sous
 *  « never » — même forme que le rendu "none" (texte = alt ou URL). */
const imgLinkEl = (src: string, label: string): HTMLAnchorElement => {
  const a = document.createElement("a");
  a.href = src;
  a.target = "_blank";
  a.rel = "noopener noreferrer";
  a.textContent = label || src;
  return a;
};

/** Clic délégué sur un placeholder image (#events chat, #file-md
 *  viewer) : le bouton est remplacé par <a><img></a> — la requête
 *  réseau ne part qu'ici, jamais au rendu. true = clic consommé.
 *  La politique est revalidée au geste : « never » reste absolu même
 *  si le réglage a basculé après le rendu du placeholder. */
export function expandImgPh(target: HTMLElement): boolean {
  const btn = target.closest<HTMLButtonElement>(".md-img-ph");
  const src = btn?.dataset.src;
  if (!btn || !src) return false;
  if (imgRender(src) === "none") {
    btn.replaceWith(imgLinkEl(src, btn.dataset.alt ?? src));
    return true;
  }
  const a = document.createElement("a");
  a.href = src;
  a.target = "_blank";
  a.rel = "noopener noreferrer";
  const img = document.createElement("img");
  img.className = "md-img";
  img.src = src;
  img.alt = btn.dataset.alt ?? src;
  img.loading = "lazy";
  a.append(img);
  btn.replaceWith(a);
  return true;
}

/** Version DOM de imgPh — pour la rétrogradation <img>→placeholder
 *  du sweep réglages (même bouton que le rendu, dataset/textContent). */
const imgPhEl = (src: string, alt: string): HTMLButtonElement => {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "md-img-ph";
  b.dataset.src = src;
  b.dataset.alt = alt;
  b.textContent = t("img_ph", { host: imgHost(src) || src });
  return b;
};

/** Ré-applique la politique courante au contenu déjà rendu — appelé à
 *  l'enregistrement des réglages, le chat/viewer n'étant pas re-rendus :
 *  « never » est absolu — placeholders restants et <img> distantes
 *  déjà insérées redeviennent de simples liens ; « click » rétrograde
 *  les <img> distantes non fiables en placeholders (un lazy-load
 *  différé ne doit pas partir sans geste) ; « always » ou une
 *  whitelist élargie ouvre les placeholders restants. */
export function refreshImgPolicy(root: ParentNode = document): void {
  root.querySelectorAll<HTMLButtonElement>(".md-img-ph").forEach((btn) => {
    const src = btn.dataset.src;
    if (!src) return;
    const r = imgRender(src);
    if (r === "img") expandImgPh(btn);
    else if (r === "none")
      btn.replaceWith(imgLinkEl(src, btn.dataset.alt ?? src));
  });
  root.querySelectorAll<HTMLImageElement>("img.md-img").forEach((img) => {
    const src = img.getAttribute("src") ?? "";
    if (!/^https?:/i.test(src)) return;
    const r = imgRender(src);
    if (r === "img") return;
    const nodes: Node[] =
      r === "none"
        ? [imgLinkEl(src, img.alt)]
        : [imgLinkEl(src, img.alt), imgPhEl(src, img.alt)];
    const wrap = img.parentElement;
    if (wrap?.tagName === "A" && wrap.childNodes.length === 1)
      wrap.replaceWith(...nodes);
    else img.replaceWith(...nodes);
  });
}

// Chemins de fichiers cliquables → panneau latéral (read_file côté démon)
const FILE_RE =
  /((?:\/[\w.\-~]+){2,}|[\w.\-]+(?:\/[\w.\-]+){2,}|[\w.\-/]+\.(?:rs|ts|tsx|js|jsx|py|toml|json|md|html?|css|ya?ml|sh|c|cpp|h|hpp|go|java|rb|txt|log|svg|cfg|ini|sql|vue|svelte|lock|service|png|jpe?g|gif|webp|bmp|ico|avif|mp4|m4v|mov|webm|mkv|ogv|avi))(?::(\d+))?/g;

export function linkify(escaped: string): string {
  // URLs d'abord → vrais liens (placeholder \x00U pour ne pas être re-matchées
  // par FILE_RE ni corrompues par le markup)
  const urls: string[] = [];
  let work = escaped.replace(/https?:\/\/[^\s<\x00]+/g, (m) => {
    const clean = trimUrlEnd(m);
    const r = IMG_URL_RE.test(clean) ? imgRender(clean) : "none";
    const inner =
      r === "img"
        ? `<img class="md-img" src="${q(clean)}" alt="${q(clean)}" loading="lazy">`
        : clean;
    urls.push(
      `<a class="ulink" href="${q(clean)}" target="_blank" rel="noopener noreferrer">${inner}</a>` +
        (r === "ph" ? imgPh(clean, clean) : ""),
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

// Ponctuation/entity finale écartée d'une URL nue — les ')' ne sont
// retirées que si elles déséquilibrent les parenthèses de l'URL
// (https://x/a_(b) reste entier, https://x/a) perd son ')' orphelin).
const URL_TRAIL = /(?:&(?:quot|#39|lt|gt);|[.,;:!?\]])+$/;
function trimUrlEnd(u: string): string {
  let s = u;
  for (;;) {
    const n = s.replace(URL_TRAIL, "");
    if (n === s) break;
    s = n;
  }
  while (s.endsWith(")")) {
    const open = (s.match(/\(/g) ?? []).length;
    const close = (s.match(/\)/g) ?? []).length;
    if (close <= open) break;
    s = s.slice(0, -1).replace(URL_TRAIL, "");
  }
  return s;
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

// ---------------------------------------------------------------------------
// Liens inline […](…) / ![…](…) — parseur manuel sur le texte échappé.
// Les regex historiques ([^)]* pour l'URL, [^\]]* pour le texte)
// cassaient sur les destinations à parenthèses (wikipedia…), les textes
// à crochets imbriqués, les destinations <…> avec espaces et les titres.
// Formes acceptées (proche CommonMark) :
//   [t](https://x/y_(z))          parenthèses équilibrées dans la dest
//   [t](<mon fichier.md>)         destination entre <…> (espaces admis)
//   [t](docs/a.md "titre")        titre optionnel "…" '…' (…)
//   [a [b] c](https://x)          crochets équilibrés dans le texte
// Destinataire :
//   http(s)         → <a target=_blank> / <img> selon la politique image
//   chemin (pas de schéma URI, hors #ancre) → .flink data-path — le
//                     viewer de fichiers les ouvre (résolution relative
//                     au dossier du .md) ; dans le chat ils restent
//                     stylés comme les chemins nus de linkify
//   #ancre, autre schéma (mailto:, javascript:…) → libellé seul, JAMAIS
//                     d'href : initExternalLinks n'ouvre que http(s) et
//                     la webview naviguerait sur le relatif.
// ---------------------------------------------------------------------------

const MD_WS = new Set([" ", "\t", "\n", "\r"]);
const MD_SCHEME = /^[a-zA-Z][a-zA-Z0-9+.-]*:/;
// `C:\…`/`C:/…` ressemble à un schéma mais est un chemin Windows.
const MD_WIN_PATH = /^[a-zA-Z]:[\\/]/;

interface MdLink {
  end: number; // index juste après le ')' final
  bang: boolean; // préfixe '!' → image
  text: string; // libellé (échappé, crochets internes conservés)
  dest: string; // destination (échappée, <…> et titre retirés)
  title: string | null;
}

/// Parse « […](…) » ou « ![…](…) » à partir de s[i] ('[' ou '!').
/// null = pas un lien valide → le texte reste tel quel (jamais de
/// réparation à l'aveugle : une parenthèse orpheline casse le match).
function mdLinkAt(s: string, i: number): MdLink | null {
  const bang = s[i] === "!";
  const b = bang ? i + 1 : i;
  if (s[b] !== "[") return null;
  // Texte du lien : crochets équilibrés (un '[' interne doit être fermé).
  let j = b + 1;
  let depth = 1;
  while (j < s.length && depth) {
    if (s[j] === "[") depth++;
    else if (s[j] === "]") depth--;
    j++;
  }
  if (depth || s[j] !== "(") return null;
  const text = s.slice(b + 1, j - 1);
  // Destination : <…> (&lt;…&gt; après échappement, espaces admis) ou
  // suite sans blanc ni ')' non appariée.
  let k = j + 1;
  while (k < s.length && MD_WS.has(s[k])) k++;
  let dest = "";
  if (s.startsWith("&lt;", k)) {
    const gt = s.indexOf("&gt;", k + 4);
    if (gt < 0) return null;
    dest = s.slice(k + 4, gt);
    k = gt + 4;
  } else {
    const d0 = k;
    let p = 0;
    while (k < s.length) {
      const c = s[k];
      if (c === "(") p++;
      else if (c === ")") {
        if (!p) break;
        p--;
      } else if (MD_WS.has(c)) break;
      k++;
    }
    dest = s.slice(d0, k);
    if (!dest) return null;
  }
  // Espaces puis ')' — ou titre optionnel ("…" '…' (…) sans imbrication)
  // puis ')'.
  while (k < s.length && MD_WS.has(s[k])) k++;
  let title: string | null = null;
  if (s[k] !== ")") {
    const open = s[k];
    let close = -1;
    if (open === '"' || open === "'") close = s.indexOf(open, k + 1);
    else if (open === "(") {
      const e = s.indexOf(")", k + 1);
      if (e >= 0 && !s.slice(k + 1, e).includes("(")) close = e;
    }
    if (close < 0) return null;
    title = s.slice(k + 1, close);
    k = close + 1;
    while (k < s.length && MD_WS.has(s[k])) k++;
    if (s[k] !== ")") return null;
  }
  if (!dest) return null;
  return { end: k + 1, bang, text, dest, title };
}

/// HTML d'un lien/image parsé — null = « libellé seul » (ancre ou
/// schéma non-http) : le texte est recopié brut pour garder le gras.
function mdLinkHtml(l: MdLink): string | null {
  const { bang, text, dest, title } = l;
  if (/^https?:\/\//i.test(dest)) {
    if (!bang) {
      const ttl = title ? ` title="${q(title)}"` : "";
      return `<a href="${q(dest)}" target="_blank" rel="noopener noreferrer"${ttl}>${text}</a>`;
    }
    const r = imgRender(dest);
    return r === "img"
      ? `<a href="${q(dest)}" target="_blank" rel="noopener noreferrer">` +
          `<img class="md-img" src="${q(dest)}" alt="${q(text)}" loading="lazy"></a>`
      : `<a href="${q(dest)}" target="_blank" rel="noopener noreferrer">${text || dest}</a>` +
          (r === "ph" ? imgPh(dest, text) : "");
  }
  // Ancre ou schéma non-http (mailto:, javascript:, data:…) : jamais
  // d'href — la navigation relative/media: casserait la webview.
  if (dest.startsWith("#") || (MD_SCHEME.test(dest) && !MD_WIN_PATH.test(dest)))
    return null;
  // Chemin → .flink : cliquable dans le viewer (relatif au dossier du
  // fichier), simplement stylé dans le chat — même sort que les chemins
  // nus produits par linkify.
  const label = bang ? text || dest : text;
  return `<span class="flink" data-path="${q(dest)}">${label}</span>`;
}

/// Remplace chaque […](…) / ![…](…) valide par son HTML via `stash`
/// (les marqueurs \x00B imbriqués — code inline dans le libellé — sont
/// résolus au fixpoint final, comme les anciens placeholders).
function mdLinks(work: string, stash: (html: string) => string): string {
  let out = "";
  let i = 0;
  while (i < work.length) {
    const c = work[i];
    if (c === "[" || (c === "!" && work[i + 1] === "[")) {
      const l = mdLinkAt(work, i);
      if (l) {
        const html = mdLinkHtml(l);
        out += html === null ? l.text : stash(html);
        i = l.end;
        continue;
      }
    }
    out += c;
    i++;
  }
  return out;
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

  // 3) images ![…](…) et liens […](…) — parseur manuel (le texte est
  //    déjà échappé) : parenthèses équilibrées dans la destination,
  //    forme <…> et titre optionnel, crochets imbriqués équilibrés.
  //    http(s) → <a>/<img> ; chemin (relatif, /abs, ~/) → .flink
  //    (ouvrable dans le viewer) ; ancre #… ou autre schéma → libellé
  //    seul — jamais d'href non-http (initExternalLinks les ignorerait
  //    et la webview naviguerait sur le relatif).
  work = mdLinks(work, stash);

  // 4) inline : gras / italique / barré
  work = work
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\b__([^_]+)__\b/g, "<strong>$1</strong>")
    .replace(/(^|[^\w*])\*([^*\n]+)\*(?![*\w])/g, "$1<em>$2</em>")
    .replace(/(?<![\w_])_([^_\n]+)_(?![\w_])/g, "<em>$1</em>")
    .replace(/~~([^~]+)~~/g, "<del>$1</del>");

  // 5) chemins de fichiers cliquables (hors blocs extraits)
  work = linkify(work);

  // 6) structure de blocs : headers, hr, citations, tables, listes, paragraphes
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

/** Markdown → texte prononçable (TTS #138) : blocs de code, markup et
 *  URLs exclus — la voix lit la prose, ni la syntaxe ni le code. */
export function speechText(src: string): string {
  return src
    .replace(/```[\s\S]*?(?:```|$)/g, " ")
    .replace(/`([^`\n]+)`/g, "$1")
    .replace(/!\[([^\]]*)\]\((?:[^()]|\([^()]*\))*\)/g, "$1")
    .replace(/\[([^\]]+)\]\((?:[^()]|\([^()]*\))*\)/g, "$1")
    .replace(/<[^>]+>/g, " ")
    .replace(/https?:\/\/\S+/g, " ")
    .replace(/^#{1,6}\s+/gm, "")
    .replace(/^\s*(?:\*{3,}|-{3,}|_{3,})$/gm, " ")
    .replace(/^\s*>\s?/gm, "")
    .replace(/^\s*(?:[-*+]|\d+[.)])\s+/gm, "")
    .replace(/^\s*\|?[\s:|-]+\|[\s:|-]*$/gm, " ")
    .replace(/\|/g, " ")
    .replace(/\*\*([^*]+)\*\*/g, "$1")
    .replace(/\b__([^_]+)__\b/g, "$1")
    .replace(/\*([^*\n]+)\*/g, "$1")
    .replace(/(^|[^\w])_([^_\n]+)_(?=[^\w]|$)/g, "$1$2")
    .replace(/~~([^~]+)~~/g, "$1")
    .replace(/[ \t]+/g, " ")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
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

/** Durée courte pour les tooltips (âge d'un waiter, #189) :
 *  "34 s", "12 min", "1 h 05", "2 j". < 1 s → "0 s" (l'appelant
 *  affiche « à l'instant »). */
export const fmtDur = (secs: number): string => {
  const s = Math.max(0, Math.round(secs));
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.floor(s / 60)} min`;
  if (s < 86400) {
    const h = Math.floor(s / 3600);
    const m = Math.round((s % 3600) / 60);
    if (m === 0) return `${h} h`;
    if (m === 60) return `${h + 1} h`;
    return `${h} h ${String(m).padStart(2, "0")}`;
  }
  return `${Math.floor(s / 86400)} j`;
};
