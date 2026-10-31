// ---------------------------------------------------------------------------
// Alertes sonores : un son court quand une session change d'état —
//   fin de tour   working/compacting/running/waiting_input → idle   "done"
//   question      → waiting_input                                 "question"
//   erreur        → dead/interrupted                              "error"
// Branché sur le diff d'états partagé de state-diff.ts (liste fusionnée de
// Sidebar.refresh() — toutes les connexions, session ouverte ou non) ;
// chaque transition y est dispatchée vers alert().
// Réglages localStorage ac-* ; le timbre joué est un preset choisi par
// événement, synthétisé en WebAudio à la volée (sound-presets.ts).
// ---------------------------------------------------------------------------

import { logUi } from "../core/dom";
import { PRESETS, playPreset, type SoundKind } from "./sound-presets";

export type { SoundKind };

/** Rafale évitée : un même type ne peut pas sonner deux fois en 2 s. */
const COOLDOWN_MS = 2_000;
/** Son mis en file (autoplay suspendu) : au-delà, la notification est
 *  périmée — jouer un « done » vieux de plusieurs minutes au déblocage
 *  n'a plus de sens, on le laisse tomber. */
const STALE_MS = 15_000;

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
/** Preset choisi pour un événement — id inconnu ou absent → premier de la
 *  liste (« classic », équivalent des wav historiques). */
export const sndPresetId = (k: SoundKind) => {
  const id = localStorage.getItem(`ac-snd-preset-${k}`) ?? "";
  return PRESETS[k].some((p) => p.id === id) ? id : PRESETS[k][0].id;
};
const sndPreset = (k: SoundKind) =>
  PRESETS[k].find((p) => p.id === sndPresetId(k)) ?? PRESETS[k][0];

class SoundAlerts {
  private lastPlay = new Map<SoundKind, number>();

  /** Transition signalée par state-diff — gardes réglages puis lecture. */
  alert(kind: SoundKind) {
    this.play(kind);
  }

  /** Bouton « tester » des paramètres — volontairement sans garde (toggle,
   *  focus, cooldown) : le geste utilisateur débloque aussi l'autoplay. */
  test(kind: SoundKind) {
    void this.emit(kind);
  }

  /** Vrai tant que la politique autoplay garde l'AudioContext suspendu
   *  (aucun geste utilisateur reçu depuis le boot) — ou si le contexte
   *  n'a pas pu être créé du tout. Affiché dans les réglages pour
   *  expliquer un éventuel silence. */
  blocked() {
    return this.ctxFailed || (this.ctx !== null && this.ctx.state === "suspended");
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
  private ctxFailed = false;
  private warnedBlocked = false;

  private audioCtx(): AudioContext | null {
    if (!this.ctx) {
      if (this.ctxFailed) return null;
      try {
        this.ctx = new AudioContext();
      } catch (e) {
        this.ctxFailed = true;
        logUi("error", `audio: AudioContext indisponible: ${e}`);
        return null;
      }
      // Déverrouillage autoplay : le contexte démarre "suspended" tant que
      // la page n'a pas reçu de geste — le premier suffit (capture, once).
      const unlock = () => void this.ctx?.resume().catch(() => {});
      for (const ev of ["pointerdown", "keydown", "touchend", "click"] as const) {
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

  private async emit(kind: SoundKind) {
    const ctx = this.audioCtx();
    if (!ctx) return;
    // Hors geste (event réseau) resume() peut rester pending jusqu'au
    // geste suivant : le son part alors tout de même dès le déblocage.
    const t0 = Date.now();
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
    // Débloqué trop tard : la notification est périmée, on l'écarte
    // plutôt que de jouer en rafale des sons obsolètes.
    if (Date.now() - t0 > STALE_MS) return;
    // Gain maître par lecture (volume courant) → synthèse du preset, puis
    // compresseur léger en headroom : les presets denses (accords, voix
    // doublées) peuvent dépasser la crête unitaire là où les wav étaient
    // normalisés. Le graphe est déconnecté une fois la queue passée.
    const master = ctx.createGain();
    master.gain.value = sndVolume();
    const comp = ctx.createDynamicsCompressor();
    comp.threshold.value = -18;
    comp.knee.value = 20;
    comp.ratio.value = 6;
    comp.attack.value = 0.002;
    comp.release.value = 0.15;
    master.connect(comp).connect(ctx.destination);
    const ttl = playPreset(ctx, master, sndPreset(kind));
    setTimeout(() => {
      master.disconnect();
      comp.disconnect();
    }, ttl * 1000 + 100);
  }
}

export const sounds = new SoundAlerts();
