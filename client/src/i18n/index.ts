// i18n minimal : dictionnaires par langue (./en ./fr ./es ./de, défaut en),
// t() avec interpolation {var}, data-i18n / data-i18n-ph / data-i18n-title
// pour les chaînes statiques du HTML.

// Modifieur principal des raccourcis : Cmd sur macOS, Ctrl ailleurs —
// {mod} est un placeholder implicite, substitué par t() dans toutes les
// chaînes (ex. "{mod}+Enter = force"). Détection UA synchrone, cohérente
// avec isAndroid/isWindows de core/chrome.
import { en, type Key } from "./en";
import { fr } from "./fr";
import { es } from "./es";
import { de } from "./de";

const MOD = /Mac OS X|Macintosh/i.test(navigator.userAgent) ? "Cmd" : "Ctrl";

const STR = { en, fr, es, de } as const;

export type Lang = keyof typeof STR;
export type { Key };

/** Langues proposées dans #s-lang — dérivées de STR : ajouter une langue =
 *  un dictionnaire dans STR portant son propre nom (clé lang_name, que
 *  chaque bloc affiche dans sa propre langue). */
export const LANGS: { id: Lang; label: string }[] = (
  Object.keys(STR) as Lang[]
).map((l) => ({ id: l, label: STR[l].lang_name }));

let lang: Lang = "en";

export function initLang() {
  const saved = localStorage.getItem("ac-lang") as Lang | null;
  lang = saved !== null && LANGS.some((l) => l.id === saved) ? saved : "en";
}

export function getLang() {
  return lang;
}

export function setLang(l: Lang) {
  lang = l;
  localStorage.setItem("ac-lang", l);
  applyStatic();
}

export function t(k: Key, vars?: Record<string, string | number>): string {
  let s: string = STR[lang][k] ?? STR.en[k] ?? k;
  s = s.replaceAll("{mod}", MOD);
  if (vars) for (const [k2, v] of Object.entries(vars)) s = s.replace(`{${k2}}`, String(v));
  return s;
}

/** Traduit les éléments marqués data-i18n / data-i18n-ph / data-i18n-title. */
export function applyStatic() {
  document.documentElement.lang = lang;
  for (const el of document.querySelectorAll<HTMLElement>("[data-i18n]")) {
    el.textContent = t(el.dataset.i18n as Key);
  }
  for (const el of document.querySelectorAll<HTMLInputElement>("[data-i18n-ph]")) {
    el.placeholder = t(el.dataset.i18nPh as Key);
  }
  for (const el of document.querySelectorAll<HTMLElement>("[data-i18n-title]")) {
    el.title = t(el.dataset.i18nTitle as Key);
  }
}
