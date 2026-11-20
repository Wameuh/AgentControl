// ---------------------------------------------------------------------------
// Capacités du moteur de rendu (webview) — détection au boot, rapportée
// dans les logs et la boîte « À propos » pour le support à distance.
// Servent aussi aux chemins de repli : field-sizing absent → autosize
// par miroir (composer), content-visibility absent → contain:layout.
// ---------------------------------------------------------------------------

export interface EngineCaps {
  /** field-sizing:content — le navigateur dimensionne #input seul. */
  fieldSizing: boolean;
  /** content-visibility:auto — skip layout/paint des events hors écran. */
  contentVisibility: boolean;
  /** contain:layout — containment de repli (Safari ≥15.4 / WebKitGTK ≥2.36). */
  contain: boolean;
}

export const engineCaps: EngineCaps = {
  fieldSizing:
    typeof CSS !== "undefined" && CSS.supports("field-sizing", "content"),
  contentVisibility:
    typeof CSS !== "undefined" && CSS.supports("content-visibility", "auto"),
  contain: typeof CSS !== "undefined" && CSS.supports("contain", "layout"),
};

/** Échappatoires de test A/B pour reproduire le webview de l'utilisateur
 *  sur un navigateur moderne : `localStorage["ac-nofs"]="1"` force le
 *  chemin miroir du composer, `["ac-nocv"]="1"` neutralise
 *  content-visibility (le repli contain:layout prend le relais). */
export const debugNoFs = () => localStorage.getItem("ac-nofs") === "1";
export const debugNoCv = () => localStorage.getItem("ac-nocv") === "1";

/** Libellé moteur lisible pour le support : UA nu trop variable, on
 *  dérive la famille + version exploitable (WebKitGTK n'a pas de marque
 *  UA propre — « AppleWebKit » seul → on le dit honnêtement). */
export function engineLabel(): string {
  const ua = navigator.userAgent;
  const chrome = /(?:Chrome|Chromium|Edg|OPR)\/(\d+)/.exec(ua);
  if (chrome && /Android/.test(ua)) return `Android WebView (Chromium ${chrome[1]})`;
  if (/Firefox\//.test(ua)) return `Firefox ${/Firefox\/(\d+)/.exec(ua)?.[1] ?? ""}`.trim();
  if (chrome) return `Chromium ${chrome[1]}`;
  const safari = /Version\/(\d+(?:\.\d+)?)[^\d]*Safari\//.exec(ua);
  if (safari) return `WebKit/Safari ${safari[1]}`;
  if (/AppleWebKit\//.test(ua)) return "WebKit (version inconnue — webview Tauri probable)";
  return "inconnu";
}

/** Ligne de diagnostic complète : moteur + capabilities + flags debug. */
export function engineReport(): string {
  const c = engineCaps;
  const flags = [debugNoFs() && "nofs", debugNoCv() && "nocv"].filter(Boolean);
  return (
    `${engineLabel()} · field-sizing:${c.fieldSizing ? "+" : "-"} ` +
    `content-visibility:${c.contentVisibility ? "+" : "-"} ` +
    `contain:${c.contain ? "+" : "-"}` +
    (flags.length ? ` · debug:${flags.join(",")}` : "")
  );
}
