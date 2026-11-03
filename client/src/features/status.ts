// ---------------------------------------------------------------------------
// Indicateurs globaux : icône fenêtre agrégée, gemmes d'état, barre d'état
// (mémoire/CPU du client, rafraîchie périodiquement).
// ---------------------------------------------------------------------------

import { invoke, isWeb } from "../core/bridge";
import { gemSVG } from "../crystal";
import { $ } from "../core/dom";
import { t } from "../i18n";

/** Gemme d'état : dorée working, violette compacting, rouge question/failed,
 *  verte idle, grise dead, **opale wait** — attente transitoire (backoff
 *  rate_limit à reprise auto, session endormie réveillable) distincte des
 *  échecs terminaux et de la demande d'input utilisateur. */
const DOT_ROLE: Record<string, string> = {
  working: "human", running: "human", starting: "human",
  waiting_input: "inference", failed: "inference", error: "inference",
  compacting: "agent",
  idle: "neutral",
  interrupted: "slate", dead: "slate", asleep: "slate",
  wait: "wait",
};

interface LastError {
  kind?: string;
}

/** Clé effective de l'indicateur : `error` + `kind=rate_limit` (backoff,
 *  reprise auto — pas une panne) et `asleep` (endormie, réveillable — pas
 *  morte) basculent sur la teinte `wait` ; `waiting_input` reste rouge
 *  (attente UTILISATEUR, action requise — sémantique différente). */
function dotKey(state: string, lastError?: LastError | null): string {
  if (state === "error" && lastError?.kind === "rate_limit") return "wait";
  if (state === "asleep") return "wait";
  return state;
}

export function dotIcon(state: string, lastError?: LastError | null): string {
  return gemSVG(DOT_ROLE[dotKey(state, lastError)] ?? "slate");
}

/** Classe `st-*` whitelistée pour l'attribut class : un état arbitraire
 *  (jamais prévu côté démon, mais la confiance n'est pas un filtre)
 *  retombe sur "dead" plutôt que d'être interpolé brut. */
export function dotCls(state: string, lastError?: LastError | null): string {
  const k = dotKey(state, lastError);
  return Object.hasOwn(DOT_ROLE, k) ? k : "dead";
}

/** Icône fenêtre : rouge si une question attend, dorée si un agent tourne,
 *  violette si une session compacte, grise sans agent vivant, gemme sauge
 *  sinon (no-op sur Android). */
let lastIconState = "";
export function setAppIcon(state: string) {
  if (state === lastIconState) return;
  lastIconState = state;
  invoke("set_app_icon", { state }).catch(() => {});
}

/** Bloc `host` du daemon_status de la conn active — absent sur un vieux
 *  démon → la section reste masquée silencieusement. */
export interface HostStats {
  cpu?: number;
  mem_used_mb?: number;
  mem_total_mb?: number;
  load1?: number;
  rss_mb?: number;
  daemon_cpu?: number;
}

/** Résout le bloc `host` à afficher (injecté par main.ts — la sonde
 *  daemon_status du watchdog l'alimente déjà, zéro polling ajouté). */
let hostStats: (() => HostStats | undefined) | undefined;

/** "3.1/8 GB" dès que le total dépasse 1024 Mo, sinon "128/512 MB" —
 *  1 décimale max, ".0" élagué (même famille de bug que #122). */
function fmtMemPair(usedMb: number, totalMb: number): string {
  const gb = totalMb >= 1024;
  const f = (v: number) => (gb ? Math.round(v / 102.4) / 10 : Math.round(v));
  return `${f(usedMb)}/${f(totalMb)} ${gb ? "GB" : "MB"}`;
}

/** Segment « srv » de la barre : cpu + mem de l'hôte démon (+ load unix
 *  et RSS/CPU du process démon — les libellés i18n distinguent la
 *  portée : « host » = machine entière, « daemon » = process seul). */
function hostPart(): string {
  const h = hostStats?.();
  if (!h || typeof h.cpu !== "number" ||
      typeof h.mem_used_mb !== "number" || typeof h.mem_total_mb !== "number") {
    return "";
  }
  // toFixed : ne pas faire confiance à la précision démon (même piège
  // d'élargissement f32→f64 que #122 côté local).
  let s = t("sb_srv", {
    cpu: h.cpu.toFixed(1),
    mem: fmtMemPair(h.mem_used_mb, h.mem_total_mb),
  });
  if (typeof h.load1 === "number") s += ` · ${t("sb_srv_load", { v: h.load1 })}`;
  if (typeof h.rss_mb === "number") {
    // daemon_cpu absent sur les vieux démons → repli « RSS seul ».
    s += ` · ${typeof h.daemon_cpu === "number"
      ? t("sb_srv_dmn_cpu", { cpu: h.daemon_cpu.toFixed(1), v: h.rss_mb })
      : t("sb_srv_dmn", { v: h.rss_mb })}`;
  }
  return ` · ${s}`;
}

// Barre d'état : mémoire + CPU du client, toutes les 3 s
async function updateStatusbar() {
  try {
    const s = await invoke<{ rss_mb: number; cpu: number; rx_bps: number; tx_bps: number }>("sys_stats");
    const fmt = (b: number) => b >= 1_048_576 ? `${(b / 1_048_576).toFixed(1)} MB/s` : b >= 1024 ? `${(b / 1024).toFixed(0)} kB/s` : `${Math.round(b)} B/s`;
    // Web : rss/cpu = métriques du process Tauri (0 en navigateur) — on
    // n'affiche que le trafic de la WebSocket vers le démon.
    const el = $("sb-stats");
    const host = hostPart();
    el.textContent = (isWeb
      ? `↓${fmt(s.rx_bps)} ↑${fmt(s.tx_bps)}`
      : `mem ${Math.round(s.rss_mb)} MB · cpu ${s.cpu.toFixed(1)}% · ↓${fmt(s.rx_bps)} ↑${fmt(s.tx_bps)}`)
      + host;
    // Tooltip : l'espace est trop compté pour expliciter chaque
    // métrique inline — la portée (client / hôte entier / process
    // démon) est détaillée au survol.
    el.title = t("sb_stats_tip") + (host ? `\n${t("sb_srv_tip")}` : "");
  } catch { /* silencieux */ }
}

export function initStatus(host?: () => HostStats | undefined) {
  hostStats = host;
  setInterval(updateStatusbar, 3000);
  updateStatusbar();
}
