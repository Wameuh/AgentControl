// ---------------------------------------------------------------------------
// Catalogue complet des modèles d'un profil agent : sondé côté démon via
// `catalogue_cmd` de `[agents.*]` (ex. "devin models list"), mis en cache
// ici par (conn, agent). Les variantes catalogue-only (absentes des options
// ACP annoncées) sont proposées dans les selects avec un badge — leur
// application passe par le side-channel `model_override_cmd` du démon,
// jamais par session/set_config_option.
// ---------------------------------------------------------------------------

import { callBg } from "../core/rpc";
import { app } from "../core/state";

export interface CatalogEntry {
  value: string;
  label: string;
}
export type CatalogMap = Record<string, CatalogEntry>;

const TTL = 10 * 60_000;
const FAIL_TTL = 60_000;
const cache = new Map<string, { at: number; ttl: number; data: CatalogMap | null }>();
const inflight = new Map<string, { waiters: Set<() => void> }>();

let capable: (conn: string) => boolean = () => false;
export function initModelCatalog(d: { capable: (conn: string) => boolean }) {
  capable = d.capable;
}

function ckey(conn: string, agent: string) {
  return `${conn || "default"} ${agent}`;
}

function fresh(k: string): CatalogMap | null | undefined {
  const e = cache.get(k);
  return e && Date.now() - e.at < e.ttl ? e.data : undefined;
}

/** Catalogue connu d'un profil (toutes variantes, annoncées ou non).
 *  conn/agent omis = session courante. */
export function catalogFor(
  conn?: string,
  agent?: string,
): CatalogMap | undefined {
  const c = conn ?? app.current?.conn ?? "";
  const a = agent ?? app.current?.agent ?? "";
  if (!a) return undefined;
  return fresh(ckey(c, a)) ?? undefined;
}

/** Charge le catalogue du profil `agent` sur `conn` si absent/périmé.
 *  `onReady` appelé à chaque arrivée de données non vides (re-render des
 *  selects après le premier affichage sans catalogue). */
export function ensureCatalog(conn: string, agent: string, onReady?: () => void) {
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
  callBg<{ models: CatalogEntry[] }>("model_catalog", {
    agent,
    ...(conn ? { conn } : {}),
  })
    .then((r) => {
      const map: CatalogMap = {};
      for (const m of r?.models ?? []) map[m.value] = m;
      cache.set(k, { at: Date.now(), ttl: TTL, data: map });
    })
    .catch(() => {
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
