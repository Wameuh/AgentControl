// ---------------------------------------------------------------------------
// RPC vers le démon via le bridge Tauri (`call` command Rust).
// Dépendances injectées au boot par initRpc() — pas d'import des features.
// ---------------------------------------------------------------------------

import { invoke } from "./bridge";
import { t } from "../i18n";
import type { ConnLive } from "./types";

let currentConn: () => string | undefined = () => undefined;
let connState: (k: string) => ConnLive | undefined = () => "ok";

export function initRpc(d: {
  currentConn: () => string | undefined;
  connState: (k: string) => ConnLive | undefined;
}) {
  currentConn = d.currentConn;
  connState = d.connState;
}

export async function call<T = unknown>(
  method: string,
  params: Record<string, unknown> = {},
): Promise<T> {
  const p = { ...params };
  // conn absent (undefined) → injecter celui de la session affichée ; un
  // appelant peut passer conn:"" explicitement (envois en arrière-plan).
  if (p.session && p.conn === undefined && currentConn()) p.conn = currentConn();
  // Conn marquée down → échec rapide au lieu d'attendre le timeout RPC.
  const ck = (p.conn as string) || "";
  if (ck && (connState(ck) ?? "ok") !== "ok") throw new Error(t("conn_offline"));
  return invoke<T>("call", { method, params: p });
}
