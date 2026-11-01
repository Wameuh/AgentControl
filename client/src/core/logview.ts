// ---------------------------------------------------------------------------
// Visionneuse de client.log partagée (vue connexion + paramètres) :
// tail filtré par niveau minimum, coloré par sévérité.
// ---------------------------------------------------------------------------

import { invoke } from "./bridge";
import { esc } from "./dom";

/** Rang d'un niveau de log — le filtre du sélecteur est un niveau MINIMUM. */
const logRank = (l: string): number =>
  ({ debug: 0, info: 1, warn: 2, error: 3 } as Record<string, number>)[l] ?? 1;

/** Tail de client.log filtré par niveau `min` et coloré dans `view`
 *  (lignes `[iso] [LVL] fichier.rs:NN msg` ; les lignes anciennes sans tag
 *  de niveau sont traitées en info). */
export async function renderClientLog(view: HTMLElement, min: string) {
  const minRank = logRank(min);
  const txt = await invoke<string>("log_tail", { n: 400 }).catch(() => "");
  const rows: string[] = [];
  for (const line of txt.split("\n")) {
    const m = line.match(/^(\[[^\]]*\] \[(DEBUG|INFO|WARN|ERROR)\])(?: (\S+:\d+))?/);
    if (logRank(m?.[2]?.toLowerCase() ?? "info") < minRank) continue;
    const cls = m ? ` ll-${m[2].toLowerCase()}` : "";
    let body = esc(line);
    if (m) {
      body = `<span class="ll-meta">${esc(m[1])}</span>`;
      if (m[3]) body += ` <span class="ll-meta">${esc(m[3])}</span>`;
      body += esc(line.slice(m[0].length));
    }
    rows.push(`<span class="ll${cls}">${body}</span>`);
  }
  view.innerHTML = rows.join("\n");
  view.scrollTop = view.scrollHeight;
}

/** Branche un bloc `<details class="log-details">` (select `.log-level` +
 *  pre `.log-view`) : rendu au premier dépliage puis à chaque changement
 *  de niveau, tant qu'il reste ouvert. */
export function bindLogView(details: HTMLDetailsElement) {
  const sel = details.querySelector<HTMLSelectElement>("select.log-level");
  const view = details.querySelector<HTMLElement>("pre.log-view");
  if (!sel || !view) return;
  const render = () => void renderClientLog(view, sel.value);
  details.addEventListener("toggle", () => {
    if (details.open) render();
  });
  sel.onchange = render;
}
