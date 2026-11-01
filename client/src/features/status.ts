// ---------------------------------------------------------------------------
// Indicateurs globaux : icône fenêtre agrégée, gemmes d'état, barre d'état
// (mémoire/CPU du client, rafraîchie périodiquement).
// ---------------------------------------------------------------------------

import { invoke, isWeb } from "../core/bridge";
import { gemSVG } from "../crystal";
import { $ } from "../core/dom";
import { t } from "../i18n";

/** Gemme d'état : dorée working, violette compacting, rouge question/failed,
 *  verte idle, grise dead. */
const DOT_ROLE: Record<string, string> = {
  working: "human", running: "human", starting: "human",
  waiting_input: "inference", failed: "inference",
  compacting: "agent",
  idle: "neutral",
  interrupted: "slate", dead: "slate", asleep: "slate",
};
export function dotIcon(state: string): string {
  return gemSVG(DOT_ROLE[state] ?? "slate");
}

/** Classe `st-*` whitelistée pour l'attribut class : un état arbitraire
 *  (jamais prévu côté démon, mais la confiance n'est pas un filtre)
 *  retombe sur "dead" plutôt que d'être interpolé brut. */
export function dotCls(state: string): string {
  return Object.hasOwn(DOT_ROLE, state) ? state : "dead";
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

/** Segment « srv » de la barre : cpu + mem de l'hôte démon (+ load unix). */
function hostPart(): string {
  const h = hostStats?.();
  if (!h || typeof h.cpu !== "number" ||
      typeof h.mem_used_mb !== "number" || typeof h.mem_total_mb !== "number") {
    return "";
  }
  let s = t("sb_srv", {
    cpu: h.cpu,
    mem: fmtMemPair(h.mem_used_mb, h.mem_total_mb),
  });
  if (typeof h.load1 === "number") s += ` · ${t("sb_srv_load", { v: h.load1 })}`;
  return ` · ${s}`;
}

// Barre d'état : mémoire + CPU du client, toutes les 3 s
async function updateStatusbar() {
  try {
    const s = await invoke<{ rss_mb: number; cpu: number; rx_bps: number; tx_bps: number }>("sys_stats");
    const fmt = (b: number) => b >= 1_048_576 ? `${(b / 1_048_576).toFixed(1)} MB/s` : b >= 1024 ? `${(b / 1024).toFixed(0)} kB/s` : `${Math.round(b)} B/s`;
    // Web : rss/cpu = métriques du process Tauri (0 en navigateur) — on
    // n'affiche que le trafic de la WebSocket vers le démon.
    $("sb-stats").textContent = (isWeb
      ? `↓${fmt(s.rx_bps)} ↑${fmt(s.tx_bps)}`
      : `mem ${Math.round(s.rss_mb)} MB · cpu ${s.cpu.toFixed(1)}% · ↓${fmt(s.rx_bps)} ↑${fmt(s.tx_bps)}`)
      + hostPart();
  } catch { /* silencieux */ }
}

export function initStatus(host?: () => HostStats | undefined) {
  hostStats = host;
  setInterval(updateStatusbar, 3000);
  updateStatusbar();
}
