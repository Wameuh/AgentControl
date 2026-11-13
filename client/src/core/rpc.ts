// ---------------------------------------------------------------------------
// RPC vers le démon via le bridge Tauri (`call` command Rust).
// Dépendances injectées au boot par initRpc() — pas d'import des features.
//
// `call` = chemin strict (actions utilisateur) : conn marquée hors ligne →
// échec immédiat. `callBg`/`connOp` = loads de fond : la connexion se
// répare d'elle-même (probe/reconnect du ConnectionManager) — un appel
// tombé dans la fenêtre de coupure attend le retour plutôt que d'échouer
// en bruit, puis est rejoué une fois. Le rapport d'erreur reste à
// l'appelant : `bgErr` tait les transitoires (le chip connexion porte
// déjà l'état) et ne toast que les échecs applicatifs.
// ---------------------------------------------------------------------------

import { invoke } from "./bridge";
import { logUi, showErr } from "./dom";
import { t } from "../i18n";
import type { ConnLive } from "./types";

let currentConn: () => string | undefined = () => undefined;
let connState: (k: string) => ConnLive | undefined = () => "ok";
let connGone: (k: string) => boolean = () => false;

export function initRpc(d: {
  currentConn: () => string | undefined;
  connState: (k: string) => ConnLive | undefined;
  connGone?: (k: string) => boolean;
}) {
  currentConn = d.currentConn;
  connState = d.connState;
  connGone = d.connGone ?? (() => false);
}

/** Erreur « conn perdue » du fast-fail/gate : flaggée pour distinguer le
 *  transitoire d'un échec applicatif sans matcher le libellé i18n. */
function connLostErr(): Error {
  const e = new Error(t("conn_offline"));
  (e as { connLost?: boolean }).connLost = true;
  return e;
}

/** Échec de transport transitoire ? Flag fast-fail, libellés connus du
 *  bridge (« connexion fermée » = reader Rust mort ; conn_offline côté
 *  transport web), ou conn marquée down/reconnecting au moment du catch
 *  (conn-dead déjà consommé : toute erreur de la fenêtre est imputable au
 *  transport — un vrai refus démon rejettera pareil au rejeu). */
export function isConnLost(e: unknown, ck = ""): boolean {
  if ((e as { connLost?: boolean } | null)?.connLost) return true;
  if (ck && (connState(ck) ?? "ok") !== "ok") return true;
  return /connexion ferm[ée]e|connection lost|connexion perdue/i.test(String(e));
}

// Attente bornée du retour « conn prête » : le backoff du
// ConnectionManager plafonne à ~75 s entre essais + la durée d'un
// essai (tunnel SSH lent) → marge à 120 s. Une conn déconnectée à la
// demande (gone) ne reviendra pas : échec immédiat, pas d'attente.
const CONN_WAIT_MS = 120_000;
const CONN_POLL_MS = 300;

async function connReady(ck: string, waitMs = CONN_WAIT_MS): Promise<void> {
  const t0 = Date.now();
  while ((connState(ck) ?? "ok") !== "ok") {
    if (connGone(ck) || Date.now() - t0 > waitMs) throw connLostErr();
    await new Promise((r) => setTimeout(r, CONN_POLL_MS));
  }
}

/** Gate « conn prête » + un rejeu pour une op RPC de fond (y compris les
 *  invoke non-`call` comme subscribe). ck "" (conn non résolue côté
 *  client — appels fusionnés multi-conns) = pas de gate possible →
 *  essai unique. */
export async function connOp<T>(ck: string, op: () => Promise<T>): Promise<T> {
  if (ck) await connReady(ck);
  try {
    return await op();
  } catch (e) {
    if (!ck || !isConnLost(e, ck)) throw e;
  }
  await connReady(ck); // transitoire : rejoué une fois la conn de retour
  return op();
}

/** Appels lifecycle journalisés dans client.log : la création de
 *  session était muette — ni conn cible, ni issue de l'appel (un clic
 *  « créer » sans retour était intraçable). Départ → issue (ok /
 *  échec + durée) ; les autres méthodes (events, read_file, sondes)
 *  restent hors log pour ne pas noyer le flux. */
const TRACE_METHODS = new Set([
  "new_session",
  "attach_session",
  "resume_session",
  "fork_session",
  "delete_session",
]);

/** Résumé des params utiles au diagnostic (agent/cwd/session/titre). */
function traceArgs(p: Record<string, unknown>): string {
  return ["agent", "cwd", "session", "native_id", "title"]
    .map((k) => (p[k] != null && p[k] !== "" ? `${k}=${p[k]}` : ""))
    .filter(Boolean)
    .join(" ");
}

export async function call<T = unknown>(
  method: string,
  params: Record<string, unknown> = {},
): Promise<T> {
  const p = { ...params };
  // conn absent (undefined) → injecter celui de la session affichée ; un
  // appelant peut passer conn:"" explicitement (envois en arrière-plan).
  if (p.session && p.conn === undefined && currentConn()) p.conn = currentConn();
  const ck = (p.conn as string) || "";
  const traced = TRACE_METHODS.has(method);
  const connLabel = ck || currentConn() || "défaut";
  const t0 = traced ? Date.now() : 0;
  if (traced) {
    const args = traceArgs(p);
    logUi("info", `rpc ${method} → conn ${connLabel}${args ? ` ${args}` : ""}`);
  }
  try {
    // Conn marquée down → échec rapide au lieu d'attendre le timeout
    // RPC — tracé comme les autres issues (un fast-fail muet était
    // indistinguable d'un clic sans effet dans client.log).
    if (ck && (connState(ck) ?? "ok") !== "ok") throw connLostErr();
    const r = await invoke<T>("call", { method, params: p });
    if (traced) {
      const sid =
        r && typeof r === "object" && "session" in r
          ? ` session=${(r as { session?: unknown }).session}`
          : "";
      logUi("info", `rpc ${method} ok — ${Date.now() - t0} ms${sid}`);
    }
    return r;
  } catch (e) {
    if (traced)
      logUi("error", `rpc ${method} échec après ${Date.now() - t0} ms (conn ${connLabel}): ${e}`);
    throw e;
  }
}

/** `call` pour les loads de fond : la conn est attendue au lieu de
 *  fast-fail, et un échec transitoire est rejoué une fois post-retour.
 *  conn:"" → gate sur la conn affichée (le démon résoudra sa défaut). */
export function callBg<T = unknown>(
  method: string,
  params: Record<string, unknown> = {},
): Promise<T> {
  const p = { ...params };
  if (p.session && p.conn === undefined && currentConn()) p.conn = currentConn();
  const ck = (p.conn as string) || currentConn() || "";
  return connOp(ck, () => call<T>(method, p));
}

/** Rapport d'erreur des loads de fond : transitoire → log discret (le
 *  chip connexion porte déjà l'état — pas de toast de bruit transport) ;
 *  échec applicatif → toast comme ailleurs. */
export function bgErr(prefix: string, e: unknown, ck = "") {
  if (isConnLost(e, ck)) {
    logUi("warn", `${prefix}: ${e} — transitoire conn`);
    return;
  }
  showErr(`${prefix}: ${e}`);
}
