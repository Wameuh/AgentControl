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
};
