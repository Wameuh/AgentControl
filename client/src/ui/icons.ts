// ---------------------------------------------------------------------------
// Jeu d'icônes maison : SVG inline sur grille 24×24, trait 1.8 arrondi,
// couleur currentColor (hérite du contexte, thème clair/sombre inclus).
// Remplace les glyphes Unicode dont le rendu dépendait des fonts système
// (fallback incohérent, bascule emoji selon la plateforme).
// ---------------------------------------------------------------------------

/// Contenu interne de chaque icône (sans le <svg> englobant).
const FOLDER =
  '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>';
const SHAPES: Record<string, string> = {
  // Chrome / actions
  menu: '<path d="M4 6.5h16M4 12h16M4 17.5h16"/>',
  settings:
    '<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2Z"/>' +
    '<circle cx="12" cy="12" r="3"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 11v5"/><path d="M12 7.5h.01"/>',
  more: '<circle cx="12" cy="5" r="1.5" fill="currentColor" stroke="none"/>' +
    '<circle cx="12" cy="12" r="1.5" fill="currentColor" stroke="none"/>' +
    '<circle cx="12" cy="19" r="1.5" fill="currentColor" stroke="none"/>',
  min: '<path d="M5.5 12h13"/>',
  max: '<rect x="6" y="6" width="12" height="12" rx="1.5"/>',
  close: '<path d="M6 6l12 12M18 6 6 18"/>',
  refresh:
    '<path d="M3 12a9 9 0 0 1 15-6.7L21 8"/><path d="M21 3v5h-5"/>' +
    '<path d="M21 12a9 9 0 0 1-15 6.7L3 16"/><path d="M3 21v-5h5"/>',
  play: '<path d="M8 5v14l11-7Z"/>',
  merge:
    '<circle cx="6" cy="6" r="2.7"/><circle cx="18" cy="18" r="2.7"/>' +
    '<path d="M6 8.7V15a3 3 0 0 0 3 3h6.3"/>',
  folder: FOLDER,
  "folder-plus": FOLDER + '<path d="M12 10.5v6"/><path d="M9 13.5h6"/>',
  "user-plus":
    '<path d="M16 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"/>' +
    '<circle cx="8.5" cy="7" r="4"/>' +
    '<path d="M20 8v6"/><path d="M17 11h6"/>',
  file:
    '<path d="M14.5 2.5H6a2 2 0 0 0-2 2v15a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-12Z"/>' +
    '<path d="M14.5 2.5v6h6"/>',
  terminal:
    '<rect x="3" y="4.5" width="18" height="15" rx="2"/>' +
    '<path d="m6.5 9 3.5 3-3.5 3"/><path d="M12.5 15.5H17"/>',
  panel: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M9.5 4v16"/>',
  down: '<path d="M12 4v12"/><path d="m6 12 6 6 6-6"/><path d="M5 21h14"/>',
  steer: '<path d="m15 10 5 5-5 5"/><path d="M4 4v7a4 4 0 0 0 4 4h12"/>',
  pin:
    '<path d="M12 17v4.5"/>' +
    '<path d="M9.5 10.5a2 2 0 0 1-1.1 1.8l-1.8.9A2 2 0 0 0 5.5 15v.5a1 1 0 0 0 1 1h11a1 1 0 0 0 1-1V15a2 2 0 0 0-1.1-1.8l-1.8-.9a2 2 0 0 1-1.1-1.8V6.5h1a2 2 0 1 0 0-4h-7a2 2 0 1 0 0 4h1Z"/>',
  // Navigation / disclosure
  back: '<path d="m14.5 5.5-6.5 6.5 6.5 6.5"/>',
  "chev-right": '<path d="m9.5 5.5 6.5 6.5-6.5 6.5"/>',
  "chev-down": '<path d="m5.5 9.5 6.5 6.5 6.5-6.5"/>',
  "chev-up": '<path d="m5.5 14.5 6.5-6.5 6.5 6.5"/>',
  "arrow-right": '<path d="M4.5 12h15"/><path d="m13.5 5.5 6.5 6.5-6.5 6.5"/>',
  external:
    '<path d="M14 4.5h5.5V10"/><path d="M19.5 4.5 11 13"/>' +
    '<path d="M19.5 13.5V19a1.5 1.5 0 0 1-1.5 1.5H6A1.5 1.5 0 0 1 4.5 19V6A1.5 1.5 0 0 1 6 4.5h4.5"/>',
  plus: '<path d="M12 5.5v13M5.5 12h13"/>',
  top: '<path d="M5 3.5h14"/><path d="m6 14.5 6-6 6 6"/><path d="M12 8.5V21"/>',
  "skip-down": '<path d="m5 5 7 7 7-7"/><path d="m5 12 7 7 7-7"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="m21 21-4.35-4.35"/>',
  "zoom-in":
    '<circle cx="11" cy="11" r="7"/><path d="m21 21-4.35-4.35"/>' +
    '<path d="M11 8.5v5M8.5 11h5"/>',
  "zoom-out": '<circle cx="11" cy="11" r="7"/><path d="m21 21-4.35-4.35"/><path d="M8.5 11h5"/>',
  // États / indicateurs
  loader: '<path d="M21 12a9 9 0 1 1-6.2-8.56"/>',
  dot: '<circle cx="12" cy="12" r="5" fill="currentColor" stroke="none"/>',
  sys: '<circle cx="12" cy="12" r="8.5"/><circle cx="12" cy="12" r="2.5" fill="currentColor" stroke="none"/>',
  check: '<path d="m4.5 12.5 5 5L19.5 6.5"/>',
  warn:
    '<path d="M10.7 4.4 2.3 18.7a2 2 0 0 0 1.7 3h16a2 2 0 0 0 1.7-3L13.3 4.4a1.5 1.5 0 0 0-2.6 0Z"/>' +
    '<path d="M12 9.5V14"/><path d="M12 17.5h.01"/>',
  up: '<path d="M12 19V5"/><path d="m5.5 11.5 6.5-6.5 6.5 6.5"/>',
  clock: '<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3.5 2"/>',
  sleep: '<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8Z"/>',
  // Contenu du fil
  edit: '<path d="M17 3a2.83 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z"/>',
  eye:
    '<path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12Z"/>' +
    '<circle cx="12" cy="12" r="3"/>',
  code: '<path d="m8 7-5 5 5 5"/><path d="m16 7 5 5-5 5"/>',
  thought:
    '<path d="M9.5 18h5"/><path d="M10.5 21.5h3"/>' +
    '<path d="M12 2.5a6.5 6.5 0 0 0-3.9 11.7c.8.6 1.4 1.7 1.4 2.8h5c0-1.1.6-2.2 1.4-2.8A6.5 6.5 0 0 0 12 2.5Z"/>',
  peer: '<path d="M7.9 20A9 9 0 1 0 4 16.1L2 22Z"/>',
  lock: '<rect x="4.5" y="11" width="15" height="9.5" rx="2"/><path d="M8 11V7.5a4 4 0 0 1 8 0V11"/>',
  question:
    '<circle cx="12" cy="12" r="9"/>' +
    '<path d="M9.4 9.2a2.6 2.6 0 0 1 5.1.7c0 1.7-2.5 2.1-2.5 3.6"/><path d="M12 17.2h.01"/>',
  tool: '<path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"/>',
  // Dictée vocale (composer)
  mic:
    '<rect x="9" y="2.5" width="6" height="11" rx="3"/>' +
    '<path d="M5.5 11.5a6.5 6.5 0 0 0 13 0"/><path d="M12 18v3.5"/>',
  stop: '<rect x="7" y="7" width="10" height="10" rx="1.5" fill="currentColor" stroke="none"/>',
  // Graphe hub-and-spoke (vue diagramme)
  map:
    '<circle cx="12" cy="12" r="2.6"/>' +
    '<circle cx="4.6" cy="5.6" r="2"/><circle cx="19.4" cy="5.6" r="2"/>' +
    '<circle cx="12" cy="19.4" r="2"/>' +
    '<path d="M10.3 10.2 6.2 7.2M13.7 10.2l4.1-3M12 14.7v2.6"/>',
  // Vue sidebar
  bot:
    '<rect x="4.5" y="9" width="15" height="10.5" rx="2.5"/>' +
    '<path d="M12 9V6"/><circle cx="12" cy="4.2" r="1.4"/>' +
    '<circle cx="9.4" cy="13.4" r="1" fill="currentColor" stroke="none"/>' +
    '<circle cx="14.6" cy="13.4" r="1" fill="currentColor" stroke="none"/>' +
    '<path d="M9.6 16.6h4.8"/>',
  backlog:
    '<path d="M8.5 6h12M8.5 12h12M8.5 18h12"/>' +
    '<path d="m3 5.9 1.2 1.2 2.3-2.4"/><path d="m3 11.9 1.2 1.2 2.3-2.4"/>' +
    '<path d="m3 17.9 1.2 1.2 2.3-2.4"/>',
  pr:
    '<circle cx="6.5" cy="6" r="2.7"/><circle cx="6.5" cy="18" r="2.7"/>' +
    '<path d="M6.5 8.7v6.6"/>' +
    '<circle cx="17.5" cy="8.5" r="2.7"/>' +
    '<path d="M14.8 8.5h-2.3a4 4 0 0 0-4 4v2.9"/>',
  repo:
    '<path d="M6.5 3A2.5 2.5 0 0 0 4 5.5v13A2.5 2.5 0 0 0 6.5 21H20V3Z"/>' +
    '<path d="M9 3v4.4l2-1.4 2 1.4V3"/>',
  // QR code (appairage)
  qr:
    '<rect x="4" y="4" width="6" height="6" rx="1"/>' +
    '<rect x="14" y="4" width="6" height="6" rx="1"/>' +
    '<rect x="4" y="14" width="6" height="6" rx="1"/>' +
    '<path d="M14 14h2.5v2.5H14z"/><path d="M20 14v1.5"/>' +
    '<path d="M14 20h1.5"/><path d="M18.5 18.5H20V20h-1.5z"/>',
};

export type IconName = keyof typeof SHAPES;

/** `<svg>` inline complet. `cls` ajoute des classes à `.ic ic-<name>`.
 *  Nom inconnu (ex. `data-icon` fautif) → warn + repli sur `sys`. */
export function icon(name: IconName, cls = ""): string {
  let safe = name;
  if (!(name in SHAPES)) {
    console.warn(`[icons] nom d'icône inconnu: ${name}`);
    safe = "sys";
  }
  return (
    `<svg class="ic ic-${safe}${cls ? ` ${cls}` : ""}" viewBox="0 0 24 24" fill="none"` +
    ` stroke="currentColor" stroke-width="1.8" stroke-linecap="round"` +
    ` stroke-linejoin="round" aria-hidden="true">${SHAPES[safe]}</svg>`
  );
}

/** Remplit les éléments `[data-icon]` du markup statique (boutons sans
 *  texte) — appelé une fois au boot. */
export function hydrateIcons(root: ParentNode = document) {
  root.querySelectorAll<HTMLElement>("[data-icon]").forEach((el) => {
    el.innerHTML = icon(el.dataset.icon as IconName);
  });
}

/** Bouton contenant uniquement une icône (pendant DOM de `qBtn` texte). */
export function qIco(name: IconName, title: string): HTMLButtonElement {
  const b = document.createElement("button");
  b.innerHTML = icon(name);
  b.title = title;
  b.setAttribute("aria-label", title);
  return b;
}
