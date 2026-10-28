// ---------------------------------------------------------------------------
// État transversal de l'app — la session affichée et la dernière liste
// connue. Muté par les features ; lu partout (import léger, sans deps).
// ---------------------------------------------------------------------------

import type { SessionMeta } from "./types";

export const app = {
  /** Session ouverte dans la vue conversation (null = aucune). */
  current: null as SessionMeta | null,
  /** Process agent vivant pour la session affichée. */
  alive: false,
  /** Dernière liste de sessions fusionnée (refresh de la sidebar). */
  lastSessions: [] as SessionMeta[],
  /** Sessions créées par ce client dont le spawn tourne en tâche de fond
   *  (id → timestamp de création) — suivies par la sidebar jusqu'au
   *  premier état résolu pour notifier un échec même si l'utilisateur a
   *  navigué ailleurs (aucun canal event hors session ouverte). */
  spawnWatch: new Map<string, number>(),
};
