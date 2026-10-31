// ---------------------------------------------------------------------------
// Détecteur de transitions d'état des sessions — diff sur la liste fusionnée
// de Sidebar.refresh() : list_sessions couvre toutes les connexions, alors
// que le canal event ne suit que la session ouverte. Source unique partagée
// par les canaux d'alerte (sounds.ts, notifications.ts) : une même transition
// produit ses effets depuis la même observation, jamais deux diffs divergents.
//   fin de tour   working/compacting/running/waiting_input → idle   "done"
//   question      → waiting_input                                 "question"
//   erreur        → dead/interrupted                              "error"
// ---------------------------------------------------------------------------

import { logUi } from "../core/dom";
import type { SessionMeta } from "../core/types";

/** Type d'événement d'attention — commun à tous les canaux d'alerte. */
export type AlertKind = "done" | "question" | "error";

/** États comptant comme « l'agent travaille » pour la transition → idle. */
const BUSY = new Set(["working", "compacting", "running", "waiting_input"]);
/** États terminaux anormaux — glisser de l'un à l'autre ne re-signale pas. */
const ERR = new Set(["dead", "interrupted"]);

/** Transition d'intérêt, null sinon. */
function classify(prev: string, next: string): AlertKind | null {
  if (ERR.has(next)) return ERR.has(prev) ? null : "error";
  if (next === "waiting_input") return "question";
  if (next === "idle" && BUSY.has(prev)) return "done";
  return null;
}

export type TransitionListener = (kind: AlertKind, s: SessionMeta) => void;

class StateDiff {
  /** Dernier état vu par "conn:id" — la comparaison produit les alertes. */
  private last = new Map<string, string>();
  private listeners: TransitionListener[] = [];

  /** Canaux abonnés — appelés dans l'ordre d'enregistrement, par session. */
  onTransition(cb: TransitionListener) {
    this.listeners.push(cb);
  }

  /** Diff d'états sur la liste fusionnée du refresh sidebar. Les sessions
   *  offline gardent leur dernier état connu (stale — ni mise à jour ni
   *  alerte : une transition réelle pendant la coupure signalera au retour) ;
   *  celles qui disparaissent de la liste sont oubliées. Le dernier état vu
   *  est mémorisé par session — un état déjà connu ne rejoue jamais
   *  (première observation = seed silencieux, y compris au démarrage). */
  noteStates(list: SessionMeta[]) {
    const seen = new Set<string>();
    for (const s of list) {
      const key = `${s.conn ?? "default"}:${s.id}`;
      seen.add(key);
      if (s.offline) continue;
      const prev = this.last.get(key);
      this.last.set(key, s.state);
      if (prev === undefined || prev === s.state) continue;
      const kind = classify(prev, s.state);
      if (kind) {
        // Un canal en panne ne doit pas couper les autres ni la purge des
        // sessions disparues (entrées « last » obsolètes → re-diff).
        for (const cb of this.listeners) {
          try {
            cb(kind, s);
          } catch (e) {
            logUi("error", `state-diff: canal d'alerte en échec: ${e}`);
          }
        }
      }
    }
    for (const k of [...this.last.keys()]) {
      if (!seen.has(k)) this.last.delete(k);
    }
  }
}

export const stateDiff = new StateDiff();
