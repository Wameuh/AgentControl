// ---------------------------------------------------------------------------
// Préférences de dictée persistées (localStorage) — partagées entre la
// section « Reconnaissance vocale » des settings (selects) et le bouton
// mic du composer (appliquées au plugin avant chaque dictée, le Kotlin
// peut perdre son état volatile après un reload de la webview).
// ---------------------------------------------------------------------------

import { getLang } from "../i18n";

export type SttLang = "auto" | "en" | "fr" | "es" | "de";
export type SttEngine = "sherpa" | "system";

const LANG_KEY = "ac-stt-lang";
const ENGINE_KEY = "ac-stt-engine";
const LANGS: readonly string[] = ["auto", "en", "fr", "es", "de"];

/** Langue forcée par dictée. Défaut : la locale de l'app (dictée dans la
 *  langue de l'UI = cas dominant, et forcer lève toute une classe
 *  d'erreurs d'auto-détect sur les commandes FR techniques) — "auto"
 *  reste choisissable, il délègue à la détection du modèle/moteur. */
export const sttLang = (): SttLang => {
  const v = localStorage.getItem(LANG_KEY);
  if (v && LANGS.includes(v)) return v as SttLang;
  const app = getLang();
  return LANGS.includes(app) ? (app as SttLang) : "auto";
};

export const setSttLang = (l: SttLang) => localStorage.setItem(LANG_KEY, l);

/** Moteur : "sherpa" (modèle téléchargé, streaming natif) | "system"
 *  (SpeechRecognizer on-device Android, repli zéro-modèle). Défaut
 *  sherpa — le fallback système est opt-in (qualité/UX variables selon
 *  l'appareil, cf. stt-exploration.md §D). */
export const sttEngine = (): SttEngine =>
  localStorage.getItem(ENGINE_KEY) === "system" ? "system" : "sherpa";

export const setSttEngine = (e: SttEngine) =>
  localStorage.setItem(ENGINE_KEY, e);
