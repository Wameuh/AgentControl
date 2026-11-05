// ---------------------------------------------------------------------------
// Coûts des modèles : sondés côté démon par profil agent (catalogue
// provider, ex. `devin models`), mis en cache ici par (conn, agent).
// Provider sans source → map vide → les selects restent inchangés.
// ---------------------------------------------------------------------------

import { callBg } from "../core/rpc";
import { app } from "../core/state";
import type { ModelCostInfo } from "../core/types";

export type CostMap = Record<string, ModelCostInfo>;

const TTL = 10 * 60_000;
const FAIL_TTL = 60_000;
const cache = new Map<string, { at: number; ttl: number; data: CostMap | null }>();
const inflight = new Map<string, { waiters: Set<() => void> }>();

let capable: (conn: string) => boolean = () => false;
export function initModelCosts(d: { capable: (conn: string) => boolean }) {
  capable = d.capable;
}

/** Clé de cache/capability : conn absente = démon par défaut. */
function ckey(conn: string, agent: string) {
  return `${conn || "default"} ${agent}`;
}

function fresh(k: string): CostMap | null | undefined {
  const e = cache.get(k);
  return e && Date.now() - e.at < e.ttl ? e.data : undefined;
}

/** Coût connu d'un id de modèle exact (variantes effort incluses —
 *  `joinModel(base, eff)` côté modelbar). conn/agent omis = session
 *  courante. */
export function costFor(
  model: string,
  conn?: string,
  agent?: string,
): ModelCostInfo | undefined {
  const c = conn ?? app.current?.conn ?? "";
  const a = agent ?? app.current?.agent ?? "";
  if (!a) return undefined;
  return fresh(ckey(c, a))?.[model];
}

/** Suffixe « · $1/$5 » / « · Free » quand le coût est connu — "" sinon. */
export function costSuffix(model: string, conn?: string, agent?: string): string {
  const c = costFor(model, conn, agent);
  return c ? ` · ${c.label}` : "";
}

/** Charge les coûts du profil `agent` sur `conn` si absents/périmés.
 *  `onReady` est appelé à chaque arrivée de données non vides (permet
 *  un re-render du select après le premier affichage sans coût). */
export function ensureCosts(conn: string, agent: string, onReady?: () => void) {
  if (!agent || !capable(conn || "default")) return;
  const k = ckey(conn, agent);
  const cur = inflight.get(k);
  if (cur) {
    if (onReady) cur.waiters.add(onReady);
    return;
  }
  if (fresh(k) !== undefined) return;
  const st = { waiters: new Set<() => void>() };
  if (onReady) st.waiters.add(onReady);
  inflight.set(k, st);
  callBg<{ models: CostMap }>("model_costs", {
    agent,
    ...(conn ? { conn } : {}),
  })
    .then((r) => {
      cache.set(k, { at: Date.now(), ttl: TTL, data: r?.models ?? {} });
    })
    .catch(() => {
      // Échec transitoire : TTL court — retenté vite sans marteler
      // le provider à chaque rendu.
      cache.set(k, { at: Date.now(), ttl: FAIL_TTL, data: null });
    })
    .finally(() => {
      inflight.delete(k);
      const data = cache.get(k)?.data;
      if (data && Object.keys(data).length) {
        for (const cb of st.waiters) cb();
      }
    });
}
