// ---------------------------------------------------------------------------
// Sondes de télémétrie perf (#297) — points d'injection côté core :
// rpc/wsclient/bridge rapportent leurs mesures ici sans dépendre de la
// feature features/telemetry.ts (les imports vont features → core,
// jamais l'inverse). Null = télémétrie OFF ou pas encore initialisée →
// les points chauds ne paient qu'un test de nullité.
// ---------------------------------------------------------------------------

export const perfProbes: {
  /** Latence d'un appel RPC `call` abouti (succès ou erreur transport). */
  rpc: ((method: string, ms: number) => void) | null;
  /** Message WS reçu (mode web) : octets de la trame + durée JSON.parse. */
  wsMsg: ((bytes: number, parseMs: number) => void) | null;
  /** Livraison d'un event Tauri au listener (mode desktop/Android). */
  push: (() => void) | null;
} = { rpc: null, wsMsg: null, push: null };
