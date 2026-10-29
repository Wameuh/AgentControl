// ---------------------------------------------------------------------------
// Alertes sonores : un son court quand une session change d'état —
//   fin de tour   working/compacting/running/waiting_input → idle   "done"
//   question      → waiting_input                                 "question"
//   erreur        → dead/interrupted                              "error"
// Alimenté par le diff d'états de Sidebar.refresh() : list_sessions couvre
// toutes les connexions, alors que le canal event ne suit que la session
// ouverte. Le dernier état vu est mémorisé par session — un état déjà
// connu ne rejoue jamais (première observation = seed silencieux, y
// compris au démarrage).
// Réglages localStorage ac-* + sons synthétisés par tools/gen_sounds.py.
// ---------------------------------------------------------------------------

import { logUi } from "../core/dom";
import type { SessionMeta } from "../core/types";

export type SoundKind = "done" | "question" | "error";

const FILES: Record<SoundKind, string> = {
  done: "/sounds/done.wav",
  question: "/sounds/question.wav",
  error: "/sounds/error.wav",
};

/** États comptant comme « l'agent travaille » pour la transition → idle. */
const BUSY = new Set(["working", "compacting", "running", "waiting_input"]);
/** États terminaux anormaux — glisser de l'un à l'autre ne re-sonne pas. */
const ERR = new Set(["dead", "interrupted"]);

/** Rafale évitée : un même type ne peut pas sonner deux fois en 2 s. */
const COOLDOWN_MS = 2_000;

// --- Réglages (localStorage) ------------------------------------------------

export const sndEnabled = () => localStorage.getItem("ac-snd") !== "0";
export const sndTypeOn = (k: SoundKind) =>
  localStorage.getItem(`ac-snd-${k}`) !== "0";
/** 0..1 — défaut 60 %. */
export const sndVolume = () => {
  const v = parseInt(localStorage.getItem("ac-snd-vol") ?? "", 10);
  return (Number.isFinite(v) ? Math.min(100, Math.max(0, v)) : 60) / 100;
};
export const sndOnlyUnfocused = () =>
  localStorage.getItem("ac-snd-unfocused") === "1";

/** Transition d'intérêt, null sinon. */
function classify(prev: string, next: string): SoundKind | null {
  if (ERR.has(next)) return ERR.has(prev) ? null : "error";
  if (next === "waiting_input") return "question";
  if (next === "idle" && BUSY.has(prev)) return "done";
  return null;
}

class SoundAlerts {
  /** Dernier état vu par "conn:id" — la comparaison produit les sons. */
  private last = new Map<string, string>();
  private lastPlay = new Map<SoundKind, number>();

  /** Diff d'états sur la liste fusionnée du refresh sidebar. Les sessions
   *  offline gardent leur dernier état connu (stale — ni mise à jour ni
   *  son : une transition réelle pendant la coupure sonnera au retour) ;
   *  celles qui disparaissent de la liste sont oubliées. */
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
      if (kind) this.play(kind);
    }
    for (const k of [...this.last.keys()]) {
      if (!seen.has(k)) this.last.delete(k);
    }
  }

  /** Bouton « tester » des paramètres — volontairement sans garde (toggle,
   *  focus, cooldown) : le geste utilisateur débloque aussi l'autoplay. */
  test(kind: SoundKind) {
    void this.emit(kind);
  }

  /** Vrai tant que la politique autoplay garde l'AudioContext suspendu
   *  (aucun geste utilisateur reçu depuis le boot) — affiché dans les
   *  réglages pour expliquer un éventuel silence. */
  blocked() {
    return this.ctx !== null && this.ctx.state === "suspended";
  }

  private play(kind: SoundKind) {
    if (!sndEnabled() || !sndTypeOn(kind)) return;
    if (sndOnlyUnfocused() && document.hasFocus()) return;
    const now = Date.now();
    if (now - (this.lastPlay.get(kind) ?? -COOLDOWN_MS) < COOLDOWN_MS) return;
    this.lastPlay.set(kind, now);
    void this.emit(kind);
  }

  // --- Pipeline WebAudio ---------------------------------------------------
  // `new Audio(path).play()` hors geste utilisateur est rejeté par la
  // politique autoplay des webviews (Tauri/Android/web) → silence total.
  // Un AudioContext débloqué par resume() au premier geste (pointerdown/
  // keydown) permet ensuite les lectures programmées à tout moment.

  private ctx: AudioContext | null = null;
  private buffers = new Map<SoundKind, Promise<AudioBuffer | null>>();
  private warnedBlocked = false;

  private audioCtx(): AudioContext | null {
    if (!this.ctx) {
      try {
        this.ctx = new AudioContext();
      } catch (e) {
        logUi("error", `audio: AudioContext indisponible: ${e}`);
        return null;
      }
      // Déverrouillage autoplay : le contexte démarre "suspended" tant que
      // la page n'a pas reçu de geste — le premier suffit (capture, once).
      const unlock = () => void this.ctx?.resume();
      for (const ev of ["pointerdown", "keydown"] as const) {
        document.addEventListener(ev, unlock, { once: true, capture: true });
      }
      // Le hint « audio bloqué » des réglages suit l'état réel du contexte.
      this.ctx.addEventListener("statechange", () => {
        const el = document.getElementById("s-snd-blocked");
        if (el) el.hidden = !this.blocked();
      });
    }
    return this.ctx;
  }

  /** fetch + decode d'un son, mis en cache (null si l'asset est absent). */
  private buffer(kind: SoundKind): Promise<AudioBuffer | null> {
    let p = this.buffers.get(kind);
    if (!p) {
      const ctx = this.audioCtx();
      p = ctx
        ? fetch(FILES[kind])
            .then((r) => {
              if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
              return r.arrayBuffer();
            })
            .then((ab) => ctx.decodeAudioData(ab))
            .catch((e) => {
              logUi("error", `audio: ${FILES[kind]} illisible: ${e}`);
              return null;
            })
        : Promise.resolve(null);
      this.buffers.set(kind, p);
    }
    return p;
  }

  private async emit(kind: SoundKind) {
    const ctx = this.audioCtx();
    if (!ctx) return;
    // Hors geste (event réseau) resume() peut rester pending jusqu'au
    // geste suivant : le son part alors tout de même dès le déblocage.
    if (ctx.state === "suspended") {
      try {
        await ctx.resume();
      } catch {
        /* encore verrouillé — le resume du premier geste suivra */
      }
    }
    if (ctx.state !== "running") {
      if (!this.warnedBlocked) {
        this.warnedBlocked = true;
        logUi("warn", "audio: bloqué par la politique autoplay — un clic débloque");
      }
      return;
    }
    const buf = await this.buffer(kind);
    if (!buf) return;
    const src = ctx.createBufferSource();
    const gain = ctx.createGain();
    gain.gain.value = sndVolume();
    src.buffer = buf;
    src.connect(gain).connect(ctx.destination);
    src.start();
  }
}

export const sounds = new SoundAlerts();
