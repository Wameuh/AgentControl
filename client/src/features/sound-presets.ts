// ---------------------------------------------------------------------------
// Bibliothèque de presets sonores — synthèse WebAudio à la volée.
// Un preset = recette paramétrique : un timbre (forme d'onde, partiels,
// enveloppe, filtre, bruit d'attaque), un motif de notes et un écho
// optionnel, joués par le mini-synthé de ce module. Zéro asset binaire —
// le preset « classic » de chaque événement reproduit les wav historiques
// (ex tools/gen_sounds.py : sinus + harmoniques, attaque 8 ms, décroissance
// exponentielle).
// ---------------------------------------------------------------------------

import type { Key } from "../i18n";

export type SoundKind = "done" | "question" | "error";

/** Une note du motif : fréquence, placement, durée audible et nuances. */
export interface NoteSpec {
  f: number; // Hz
  t: number; // départ (s) dans le motif
  d: number; // durée (s) — l'extinction forcée démarre à t+d
  g?: number; // gain relatif (défaut 1)
  tau?: number; // décroissance propre (s) — override du timbre
}

/** Timbre : tout ce qui fait la couleur d'une note, indépendamment du motif. */
export interface Timbre {
  wave: OscillatorType;
  partials: number[]; // poids de chaque partiel
  ratios?: number[]; // multiplicateurs de fréquence (défaut : harmoniques 1..n)
  attack: number; // s — rampe linéaire 0 → gain de la note
  tau: number; // s — décroissance exponentielle vers `sustain`
  sustain: number; // niveau résiduel 0..1 (0 = percussif, >0 = tenu)
  release: number; // s — constante de la coupure en fin de note
  lp?: number; // lowpass Hz devant l'enveloppe
  detune?: number; // ±cents — voix doublées désaccordées (chorus)
  noise?: number; // gain du bruit filtré en attaque (percussions)
  noiseHp?: number; // highpass du bruit (défaut 2 kHz)
  noiseLp?: number; // lowpass du bruit — prime sur noiseHp si présent
}

export interface Preset {
  id: string; // identifiant stocké en localStorage
  label: Key; // clé i18n du nom affiché
  notes: NoteSpec[];
  timbre: Timbre;
  delay?: { time: number; fb: number; mix: number }; // écho simple
}

// --- Timbres partagés -------------------------------------------------------
// « classic » n'est pas ici : il reproduit les wav historiques dont les
// harmoniques diffèrent par événement — chaque preset classic porte le sien.

const TB = {
  // Cloche : partiels inharmoniques (bourdon, prime, tierce, nominal),
  // frappe sèche et longue queue.
  bell: {
    wave: "sine",
    ratios: [0.5, 1, 2.76, 5.4],
    partials: [0.25, 1, 0.35, 0.08],
    attack: 0.004,
    tau: 0.34,
    sustain: 0,
    release: 0.12,
  },
  // Marimba : attaque bruitée très courte, partiel aigu boisé, chute rapide.
  marimba: {
    wave: "sine",
    partials: [1, 0.5, 0.05, 0.22],
    attack: 0.003,
    tau: 0.055,
    sustain: 0,
    release: 0.03,
    noise: 0.22,
    noiseHp: 2600,
  },
  // Corde pincée : triangle riche, décroissance moyenne — pour les arpèges.
  pluck: {
    wave: "triangle",
    partials: [1, 0.4, 0.15],
    attack: 0.004,
    tau: 0.1,
    sustain: 0,
    release: 0.04,
  },
  // Accord : sinus voix doublées légèrement désaccordées, entrée douce.
  chord: {
    wave: "sine",
    partials: [1, 0.35, 0.12],
    attack: 0.03,
    tau: 0.17,
    sustain: 0,
    release: 0.1,
    detune: 4,
  },
  // Pop : blip bref avec un soupçon de bruit — discret.
  pop: {
    wave: "sine",
    partials: [1, 0.2],
    attack: 0.004,
    tau: 0.045,
    sustain: 0,
    release: 0.02,
    noise: 0.1,
    noiseHp: 1800,
  },
  // Impact grave : sinus bas + bruit feutré passe-bas.
  thud: {
    wave: "sine",
    partials: [1, 0.35],
    attack: 0.004,
    tau: 0.06,
    sustain: 0,
    release: 0.05,
    noise: 0.3,
    noiseLp: 300,
  },
  // Nappe : dents de scie filtrées doublées désaccordées, montée lente,
  // palier tenu puis longue extinction.
  pad: {
    wave: "sawtooth",
    partials: [1, 0.3],
    attack: 0.22,
    tau: 0.4,
    sustain: 0.4,
    release: 0.32,
    lp: 1400,
    detune: 9,
  },
  // 8-bit : carré brut, attaque immédiate, pas de queue.
  retro: {
    wave: "square",
    partials: [1, 0.15],
    attack: 0.002,
    tau: 0.05,
    sustain: 0,
    release: 0.015,
    lp: 4200,
  },
} satisfies Record<string, Timbre>;

const CLASSIC = {
  wave: "sine",
  attack: 0.008,
  tau: 0.11,
  sustain: 0,
  release: 0.03,
} satisfies Omit<Timbre, "partials">;

// --- Presets ----------------------------------------------------------------
// Motifs par événement : done = ascendant/positif, question = intervalle
// interrogatif, error = grave/descendant. Le premier de chaque liste est le
// défaut (reproduction des wav historiques — migration douce).

export const PRESETS: Record<SoundKind, Preset[]> = {
  done: [
    {
      id: "classic",
      label: "sndp_classic",
      timbre: { ...CLASSIC, partials: [1, 0.3, 0.1] },
      notes: [
        { f: 659.26, t: 0, d: 0.45, g: 0.55 }, // E5
        { f: 987.77, t: 0.16, d: 0.55, g: 0.5 }, // B5
      ],
    },
    {
      id: "bell",
      label: "sndp_bell",
      timbre: TB.bell,
      delay: { time: 0.19, fb: 0.28, mix: 0.16 },
      notes: [
        { f: 659.26, t: 0, d: 0.6, g: 0.5 },
        { f: 987.77, t: 0.16, d: 0.9, g: 0.45 },
      ],
    },
    {
      id: "marimba",
      label: "sndp_marimba",
      timbre: TB.marimba,
      notes: [
        { f: 659.26, t: 0, d: 0.22, g: 0.6 },
        { f: 987.77, t: 0.11, d: 0.35, g: 0.55 },
      ],
    },
    {
      id: "arp",
      label: "sndp_arp",
      timbre: TB.pluck,
      notes: [
        { f: 659.26, t: 0, d: 0.3, g: 0.5 }, // E5
        { f: 783.99, t: 0.07, d: 0.3, g: 0.5 }, // G5
        { f: 987.77, t: 0.14, d: 0.3, g: 0.5 }, // B5
        { f: 1318.51, t: 0.21, d: 0.55, g: 0.5 }, // E6
      ],
    },
    {
      id: "chord",
      label: "sndp_chord",
      timbre: TB.chord,
      notes: [
        { f: 659.26, t: 0, d: 0.75, g: 0.45 }, // E5
        { f: 830.61, t: 0.03, d: 0.75, g: 0.4 }, // G#5
        { f: 987.77, t: 0.06, d: 0.8, g: 0.4 }, // B5
      ],
    },
    {
      id: "pop",
      label: "sndp_pop",
      timbre: TB.pop,
      notes: [{ f: 987.77, t: 0, d: 0.14, g: 0.7 }],
    },
    {
      id: "pad",
      label: "sndp_pad",
      timbre: TB.pad,
      notes: [
        { f: 659.26, t: 0, d: 1.1, g: 0.4 },
        { f: 830.61, t: 0.04, d: 1.1, g: 0.35 },
        { f: 1318.51, t: 0.08, d: 1.1, g: 0.3 },
      ],
    },
    {
      id: "retro",
      label: "sndp_retro",
      timbre: TB.retro,
      notes: [
        { f: 659.26, t: 0, d: 0.09, g: 0.5 },
        { f: 783.99, t: 0.08, d: 0.09, g: 0.5 },
        { f: 987.77, t: 0.16, d: 0.09, g: 0.5 },
        { f: 1318.51, t: 0.24, d: 0.3, g: 0.55 },
      ],
    },
  ],
  question: [
    {
      id: "classic",
      label: "sndp_classic",
      timbre: { ...CLASSIC, partials: [1, 0.2, 0.05] },
      notes: [
        { f: 587.33, t: 0, d: 0.13, g: 0.5, tau: 0.09 }, // D5
        { f: 880, t: 0.15, d: 0.3, g: 0.5, tau: 0.13 }, // A5
      ],
    },
    {
      id: "bell",
      label: "sndp_bell",
      timbre: TB.bell,
      delay: { time: 0.19, fb: 0.28, mix: 0.16 },
      notes: [
        { f: 587.33, t: 0, d: 0.45, g: 0.5 },
        { f: 880, t: 0.15, d: 0.7, g: 0.45 },
      ],
    },
    {
      id: "marimba",
      label: "sndp_marimba",
      timbre: TB.marimba,
      notes: [
        { f: 587.33, t: 0, d: 0.16, g: 0.6 },
        { f: 880, t: 0.12, d: 0.3, g: 0.55 },
      ],
    },
    {
      id: "arp",
      label: "sndp_arp",
      timbre: TB.pluck,
      notes: [
        { f: 587.33, t: 0, d: 0.25, g: 0.5 }, // D5
        { f: 698.46, t: 0.08, d: 0.25, g: 0.5 }, // F5
        { f: 880, t: 0.16, d: 0.45, g: 0.5 }, // A5
      ],
    },
    {
      id: "chord",
      label: "sndp_chord",
      timbre: TB.chord,
      notes: [
        { f: 587.33, t: 0, d: 0.7, g: 0.45 }, // D5
        { f: 698.46, t: 0.03, d: 0.7, g: 0.4 }, // F5 — Dm, suspensif
        { f: 880, t: 0.06, d: 0.75, g: 0.4 }, // A5
      ],
    },
    {
      id: "pop",
      label: "sndp_pop",
      timbre: TB.pop,
      notes: [
        { f: 587.33, t: 0, d: 0.1, g: 0.6 },
        { f: 880, t: 0.11, d: 0.12, g: 0.6 },
      ],
    },
    {
      id: "pad",
      label: "sndp_pad",
      timbre: TB.pad,
      notes: [
        { f: 587.33, t: 0, d: 1, g: 0.4 },
        { f: 880, t: 0.05, d: 1.05, g: 0.35 },
      ],
    },
    {
      id: "retro",
      label: "sndp_retro",
      timbre: TB.retro,
      notes: [
        { f: 587.33, t: 0, d: 0.08, g: 0.5 },
        { f: 698.46, t: 0.08, d: 0.08, g: 0.5 },
        { f: 880, t: 0.16, d: 0.2, g: 0.5 },
      ],
    },
  ],
  error: [
    {
      id: "classic",
      label: "sndp_classic",
      timbre: { ...CLASSIC, partials: [1, 0.15, 0.04] },
      notes: [
        { f: 220, t: 0, d: 0.22, g: 0.6, tau: 0.16 }, // A3
        { f: 164.81, t: 0.24, d: 0.38, g: 0.6, tau: 0.18 }, // E3
      ],
    },
    {
      id: "bell",
      label: "sndp_bell",
      timbre: TB.bell,
      delay: { time: 0.24, fb: 0.3, mix: 0.2 },
      notes: [
        { f: 220, t: 0, d: 0.8, g: 0.5 }, // glas grave
        { f: 164.81, t: 0.3, d: 1, g: 0.45 },
      ],
    },
    {
      id: "marimba",
      label: "sndp_marimba",
      timbre: TB.marimba,
      notes: [
        { f: 220, t: 0, d: 0.2, g: 0.65 },
        { f: 164.81, t: 0.14, d: 0.3, g: 0.6 },
      ],
    },
    {
      id: "arp",
      label: "sndp_arp",
      timbre: TB.pluck,
      notes: [
        { f: 329.63, t: 0, d: 0.22, g: 0.5 }, // E4
        { f: 261.63, t: 0.08, d: 0.22, g: 0.5 }, // C4
        { f: 220, t: 0.16, d: 0.22, g: 0.5 }, // A3
        { f: 164.81, t: 0.24, d: 0.4, g: 0.55 }, // E3
      ],
    },
    {
      id: "chord",
      label: "sndp_chord",
      timbre: TB.chord,
      notes: [
        { f: 220, t: 0, d: 0.9, g: 0.45 }, // A3
        { f: 261.63, t: 0.04, d: 0.9, g: 0.4 }, // C4 — Am, sombre
        { f: 329.63, t: 0.08, d: 0.9, g: 0.4 }, // E4
      ],
    },
    {
      id: "thud",
      label: "sndp_thud",
      timbre: TB.thud,
      notes: [{ f: 130.81, t: 0, d: 0.3, g: 0.85 }], // C3
    },
    {
      id: "pad",
      label: "sndp_pad",
      timbre: TB.pad,
      notes: [
        { f: 110, t: 0, d: 1.3, g: 0.35 }, // A2
        { f: 164.81, t: 0.06, d: 1.3, g: 0.35 }, // E3
        { f: 220, t: 0.12, d: 1.3, g: 0.3 }, // A3
      ],
    },
    {
      id: "retro",
      label: "sndp_retro",
      timbre: TB.retro,
      notes: [
        { f: 220, t: 0, d: 0.09, g: 0.5 },
        { f: 196, t: 0.09, d: 0.09, g: 0.5 }, // G3
        { f: 164.81, t: 0.18, d: 0.09, g: 0.5 },
        { f: 130.81, t: 0.27, d: 0.35, g: 0.55 }, // C3
      ],
    },
  ],
};

// --- Mini-synthé --------------------------------------------------------------

/** Bruit blanc déterministe (LCG) partagé par contexte — attaques
 *  percussives. Reproductible : pas de Math.random(). */
const noiseBufs = new WeakMap<AudioContext, AudioBuffer>();
function noiseBuf(ctx: AudioContext): AudioBuffer {
  let b = noiseBufs.get(ctx);
  if (!b) {
    b = ctx.createBuffer(1, Math.ceil(ctx.sampleRate * 0.25), ctx.sampleRate);
    const d = b.getChannelData(0);
    let s = 0x2f6e2b1;
    for (let i = 0; i < d.length; i++) {
      s = (s * 1103515245 + 12345) & 0x7fffffff;
      d[i] = s / 0x3fffffff - 1;
    }
    noiseBufs.set(ctx, b);
  }
  return b;
}

function playNote(
  ctx: AudioContext,
  dry: AudioNode,
  wet: AudioNode | null,
  tb: Timbre,
  n: NoteSpec,
  start: number,
) {
  // Enveloppe : rampe linéaire → décroissance exp vers sustain → coupure
  // exp en fin de note (release). Plus de déterminisme que des rampes à
  // valeurs fixées : la décroissance reste lisse quel que soit le motif.
  const env = ctx.createGain();
  const p = env.gain;
  const peak = n.g ?? 1;
  const atkEnd = start + tb.attack;
  p.setValueAtTime(0, start);
  p.linearRampToValueAtTime(peak, atkEnd);
  p.setTargetAtTime(peak * tb.sustain, atkEnd, n.tau ?? tb.tau);
  const noteEnd = start + n.d;
  p.setTargetAtTime(0, noteEnd, tb.release);
  // Tête de chaîne : lowpass optionnel devant l'enveloppe.
  let head: AudioNode = env;
  if (tb.lp) {
    const f = ctx.createBiquadFilter();
    f.type = "lowpass";
    f.frequency.value = tb.lp;
    f.connect(env);
    head = f;
  }
  env.connect(dry);
  if (wet) env.connect(wet);
  const stop = noteEnd + tb.release * 10;
  const ratios = tb.ratios ?? tb.partials.map((_, i) => i + 1);
  const voices = tb.detune ? 2 : 1;
  for (const [i, w] of tb.partials.entries()) {
    if (!w) continue;
    const og = ctx.createGain();
    og.gain.value = w / voices;
    og.connect(head);
    for (let v = 0; v < voices; v++) {
      const o = ctx.createOscillator();
      o.type = tb.wave;
      o.frequency.value = n.f * ratios[i];
      if (tb.detune) o.detune.value = tb.detune * (v === 0 ? -1 : 1);
      o.connect(og);
      o.start(start);
      o.stop(stop);
    }
  }
  if (tb.noise) {
    const nb = ctx.createBufferSource();
    nb.buffer = noiseBuf(ctx);
    const nf = ctx.createBiquadFilter();
    if (tb.noiseLp) {
      nf.type = "lowpass";
      nf.frequency.value = tb.noiseLp;
    } else {
      nf.type = "highpass";
      nf.frequency.value = tb.noiseHp ?? 2000;
    }
    const ng = ctx.createGain();
    ng.gain.setValueAtTime(tb.noise, start);
    ng.gain.setTargetAtTime(0, start + 0.003, 0.008);
    nb.connect(nf);
    nf.connect(ng);
    ng.connect(head);
    nb.start(start);
    nb.stop(start + 0.08);
  }
}

/** Joue un preset sur `dest` (gain maître déjà appliqué par l'appelant).
 *  Retourne la durée de vie du graphe en secondes — l'appelant peut
 *  déconnecter `dest` passé ce délai sans tronquer la queue d'extinction
 *  ni l'écho. */
export function playPreset(
  ctx: AudioContext,
  dest: AudioNode,
  preset: Preset,
): number {
  const t0 = ctx.currentTime + 0.03;
  let wet: AudioNode | null = null;
  if (preset.delay) {
    const send = ctx.createGain();
    const dly = ctx.createDelay(1.5);
    dly.delayTime.value = preset.delay.time;
    const fb = ctx.createGain();
    fb.gain.value = preset.delay.fb;
    const mix = ctx.createGain();
    mix.gain.value = preset.delay.mix;
    send.connect(dly);
    dly.connect(fb);
    fb.connect(dly);
    dly.connect(mix);
    mix.connect(dest);
    wet = send;
  }
  let end = 0;
  for (const n of preset.notes) {
    playNote(ctx, dest, wet, preset.timbre, n, t0 + n.t);
    end = Math.max(end, n.t + n.d);
  }
  const tail = Math.max(
    preset.timbre.release * 8,
    preset.delay ? preset.delay.time * 9 : 0,
  );
  return t0 + end + tail - ctx.currentTime;
}
