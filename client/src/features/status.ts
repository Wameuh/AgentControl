// ---------------------------------------------------------------------------
// Indicateurs globaux : icône fenêtre agrégée, gemmes d'état, barre d'état
// (mémoire/CPU du client, rafraîchie périodiquement).
// ---------------------------------------------------------------------------

import { invoke, isWeb } from "../core/bridge";
import { gemSVG } from "../crystal";
import { $ } from "../core/dom";

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

// Barre d'état : mémoire + CPU du client, toutes les 3 s
async function updateStatusbar() {
  try {
    const s = await invoke<{ rss_mb: number; cpu: number; rx_bps: number; tx_bps: number }>("sys_stats");
    const fmt = (b: number) => b >= 1_048_576 ? `${(b / 1_048_576).toFixed(1)} MB/s` : b >= 1024 ? `${(b / 1024).toFixed(0)} kB/s` : `${Math.round(b)} B/s`;
    // Web : rss/cpu = métriques du process Tauri (0 en navigateur) — on
    // n'affiche que le trafic de la WebSocket vers le démon.
    $("sb-stats").textContent = isWeb
      ? `↓${fmt(s.rx_bps)} ↑${fmt(s.tx_bps)}`
      : `mem ${Math.round(s.rss_mb)} MB · cpu ${s.cpu.toFixed(1)}% · ↓${fmt(s.rx_bps)} ↑${fmt(s.tx_bps)}`;
  } catch { /* silencieux */ }
}

export function initStatus() {
  setInterval(updateStatusbar, 3000);
  updateStatusbar();
}
