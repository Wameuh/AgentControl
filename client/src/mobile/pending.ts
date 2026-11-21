// ---------------------------------------------------------------------------
// Demandes d'agent en attente (S3) : ids via session_info.pending, contenu
// via les derniers events du journal — partagé entre le dock de permission
// et d'éventuelles vues « en attente ». Aucun RPC nouveau.
// ---------------------------------------------------------------------------

import { call } from "../core/rpc";
import type { AcEvent, SessionMeta } from "../core/types";
import { tick } from "./haptics";

export interface PendingReq {
  seq: number;
  request_id: string;
  type: "permission_request" | "question_request";
  title: string;
  kind?: string;
  detail?: string;
  options: { option_id: string; name: string; kind: string }[];
}

/** Demandes en attente d'une session, même non ouverte : ids via
 *  session_info.pending, contenu via les 200 derniers events. */
export async function findPending(s: SessionMeta): Promise<PendingReq[]> {
  const info = await call<{ pending?: string[] }>("session_info", {
    session: s.id,
    conn: s.conn,
  }).catch(() => null);
  const ids = info?.pending;
  if (!ids?.length) return [];
  const res = await call<{ events?: AcEvent[] }>("events", {
    session: s.id,
    conn: s.conn,
    limit: 200,
  }).catch(() => null);
  const out: PendingReq[] = [];
  for (const ev of res?.events ?? []) {
    if (ev.type !== "permission_request" && ev.type !== "question_request") continue;
    const rid = ev.request_id as string;
    if (!rid || !ids.includes(rid)) continue;
    out.push({
      seq: ev.seq as number,
      request_id: rid,
      type: ev.type,
      title: (ev.title ?? ev.message) as string,
      kind: ev.kind as string | undefined,
      detail: ev.detail as string | undefined,
      options: (ev.options as PendingReq["options"]) ?? [],
    });
  }
  return out;
}

/** Ordre d'affichage des options d'une permission, partagé par la carte
 *  « À traiter » de l'accueil et le dock : reject_* à gauche, puis
 *  allow_always, et allow_once en dernier — l'action principale, sous le
 *  pouce. L'ordre annoncé par l'agent ne compte pas : « toujours
 *  autoriser » n'est jamais l'action mise en avant. */
export function sortPermOptions<T extends { kind: string }>(opts: T[]): T[] {
  const rank = (k: string) =>
    k.startsWith("reject") ? 0 : k === "allow_always" ? 1 : 2;
  return [...opts].sort((a, b) => rank(a.kind) - rank(b.kind));
}

/** Répond à une demande en attente, session ouverte ou non — `conn` est
 *  obligatoire : sans lui call() viserait la connexion courante. Signature
 *  légère (id + conn) réutilisable par les actions de notification (P4).
 *  Entonnoir unique des réponses « hors session » (accueil inbox, actions
 *  de notification Y3) : le tick haptique « resolve » (Y4) vit ici pour
 *  couvrir tous les appelants sans double vibration. */
export function respondPending(
  s: { id: string; conn?: string },
  requestId: string,
  reply: { option_id: string } | { answer: unknown },
): Promise<unknown> {
  return call("respond", {
    session: s.id, conn: s.conn, request_id: requestId, ...reply,
  }).then((r) => {
    tick("resolve");
    return r;
  });
}
