// Test unitaire — mdRender : liens markdown de agent_text (#352).
// Régression : les regex historiques exigeaient https?:// et une URL
// sans ')' → les liens à parenthèses (wikipedia…), les textes à
// crochets imbriqués, les destinations <…> avec espaces, les titres et
// les liens relatifs/fichiers restaient du markdown brut à l'écran.
// Règle de rendu : http(s) → <a target=_blank> ; chemin → .flink
// data-path (ouvrable dans le viewer) ; ancre #… / autre schéma →
// libellé seul, JAMAIS d'href (la webview naviguerait sur le relatif).
// Usage : node tests/md-links-test.mjs   — Node ≥ 22.18 requis
// (type stripping : format.ts + i18n importés sans transpilation ;
// les globals DOM/localStorage sont stubbés — voir errcode-test.mjs).

// Stubs AVANT l'import dynamique : i18n lit navigator au top-level,
// imgMode/imgRender lisent localStorage/location au rendu.
globalThis.navigator ??= { userAgent: "node" };
globalThis.localStorage ??= {
  _m: new Map(),
  getItem(k) { return this._m.has(k) ? this._m.get(k) : null; },
  setItem(k, v) { this._m.set(k, String(v)); },
};
globalThis.location ??= { origin: "http://localhost" };

const { mdRender, linkify } = await import("../src/core/format.ts");

let fails = 0;
const ok = (c, n) => {
  console.log((c ? "  PASS " : "  FAIL ") + n);
  if (!c) fails++;
};
const has = (html, frag, n) => ok(html.includes(frag), `${n} — attendu «${frag}»`);
const notHas = (html, frag, n) => ok(!html.includes(frag), `${n} — absent «${frag}»`);

// --- liens http(s) simples (comportement historique préservé) ---
let h = mdRender("Voir [la doc](https://example.com/docs).");
has(h, '<a href="https://example.com/docs" target="_blank"', "lien https simple");
has(h, ">la doc</a>", "libellé du lien");
notHas(h, "[la doc]", "plus de markdown brut");

// --- URL à parenthèses équilibrées (wikipedia…) ---
h = mdRender("[Article](https://fr.wikipedia.org/wiki/Rust_(langage))");
has(h, 'href="https://fr.wikipedia.org/wiki/Rust_(langage)"', "parenthèses dans l'URL");
notHas(h, ")</a>)", "pas de ')' parasite après le lien");

// --- parenthèse orpheline : la ')' de clôture reste hors du href ---
h = mdRender("(voir [x](https://a.b/c))");
has(h, 'href="https://a.b/c"', "parenthèse orpheline exclue du href");
has(h, "</a>)", "la ')' de phrase survit après le lien");

// --- texte du lien à crochets imbriqués ---
h = mdRender("[a [b] c](https://x.y)");
has(h, 'href="https://x.y"', "crochets imbriqués dans le libellé");
has(h, ">a [b] c</a>", "libellé complet conservé");

// --- destination <…> avec espaces + titre optionnel ---
h = mdRender('[mon fichier](<docs/mon fichier.md> "le titre")');
has(h, 'class="flink" data-path="docs/mon fichier.md"', "dest <…> espaces → flink");
has(h, ">mon fichier</span>", "libellé du lien fichier");
h = mdRender('[doc](https://x.y "astuce")');
has(h, 'title="astuce"', "titre optionnel → attribut title");

// --- liens relatifs / fichiers → .flink (ouvrables dans le viewer) ---
for (const [src, path] of [
  ["[src](src/core/format.ts)", "src/core/format.ts"],
  ["[local](./client.ts)", "./client.ts"],
  ["[parent](../AGENTS.md)", "../AGENTS.md"],
  ["[abs](/home/dev/x.rs)", "/home/dev/x.rs"],
  ["[home](~/notes.md)", "~/notes.md"],
]) {
  h = mdRender(src);
  has(h, `class="flink" data-path="${path}"`, `lien fichier ${src}`);
  notHas(h, "<a href", `pas d'<a>' pour ${src}`);
}

// --- ancres et schémas non-http : libellé seul, jamais d'href ---
h = mdRender("[section](#install)");
has(h, ">section<", "ancre → libellé rendu");
notHas(h, "<a ", "ancre → aucun <a>");
notHas(h, 'href="#install"', "ancre → pas d'href relatif");
h = mdRender("[écrire](mailto:a@b.c)");
has(h, ">écrire<", "mailto → libellé");
notHas(h, "<a ", "mailto → aucun <a> (webview safe)");
h = mdRender("[x](javascript:alert(1))");
notHas(h, "<a ", "javascript: → aucun <a>");
notHas(h, "alert(1)</a>", "javascript: → pas de dest dans le HTML");

// --- images : http → politique image, chemin → flink, autre → alt ---
h = mdRender("![logo](https://example.com/logo.png)");
has(h, 'class="md-img-ph"', "image distante → placeholder clic (défaut)");
h = mdRender("![shot](shots/ui.png)");
has(h, 'class="flink" data-path="shots/ui.png"', "image relative → flink");

// --- URL nue avec parenthèses (linkify) ---
h = mdRender("Voir https://ex.com/a_(b) pour la suite.");
has(h, 'href="https://ex.com/a_(b)"', "URL nue à parenthèses");
h = mdRender("Voir https://ex.com/a) pour la suite.");
has(h, 'href="https://ex.com/a"', "')' orpheline retirée de l'URL nue");
has(h, "/a</a>)", "')' orpheline rendue en texte après le lien");

// --- non-lien : markdown incomplet reste tel quel ---
h = mdRender("[pas un lien](https://x.y"); // ')' manquante
has(h, "[pas un lien](", "lien incomplet → crochet reste brut");
notHas(h, ">pas un lien</a>", "lien incomplet → pas de <a> englobant");
h = mdRender("[vide]()");
has(h, "[vide]()", "destination vide → brut");
h = mdRender("un [simple] crochet");
has(h, "[simple]", "crochet sans () → brut");

// --- régression : souligné / échappement HTML dans le libellé ---
h = mdRender("[<b>gras</b>](https://x.y)");
has(h, "&lt;b&gt;gras&lt;/b&gt;</a>", "HTML dans le libellé échappé");
h = mdRender("[lien **fort**](https://x.y)");
has(h, "**fort**</a>", "markdown interne non re-formaté (comportement historique)");

// --- linkify : ponctuation finale toujours écartée ---
h = linkify("https://a.b/c.");
has(h, 'href="https://a.b/c"', "linkify : '.' final retiré");
has(h, "/c</a>.", "linkify : '.' rendu après le lien");

console.log(fails ? `\n${fails} FAIL` : "\nALL PASS");
process.exit(fails ? 1 : 0);
