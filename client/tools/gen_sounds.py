#!/usr/bin/env python3
"""Synthétise les sons d'alerte embarqués du client (public/sounds/*.wav).
Pure stdlib, déterministe (pas d'aléa) : relancer produit des fichiers
identiques. Chaque son = notes sinusoïdales enrichies d'harmoniques sous
une enveloppe attaque courte + décroissance exponentielle.

  done.wav      chime doux ascendant (E5 → B5)   — tour terminé (…→ idle)
  question.wav  deux-ton montant (D5 → A5)       — input requis (→ waiting_input)
  error.wav     tonalité grave descendante (A3 → E3) — erreur (→ dead/interrupted)

Usage : python3 client/tools/gen_sounds.py
"""
import math
import os
import struct
import wave

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(os.path.dirname(HERE), "public", "sounds")

SR = 44100          # Hz, mono 16 bits
ATTACK = 0.008      # montée linéaire anti-clic (s)
FADE_OUT = 0.030    # fondu final global (s)
PEAK = 0.85         # normalisation de crête

# Une note : (fréquence Hz, départ s, durée s, gain, tau décroissance s,
#             poids des harmoniques 1..n)
Note = tuple


def note(freq, start, dur, gain=0.5, tau=0.11, harm=(1.0, 0.30, 0.10)):
    return (freq, start, dur, gain, tau, harm)


def render(notes):
    """Mixe les notes dans un buffer float [-1..1], fondu final, puis
    normalise à PEAK."""
    total = int(max(st + dur for _, st, dur, *_ in notes) * SR) + 1
    buf = [0.0] * total
    for freq, st, dur, gain, tau, harm in notes:
        n0 = int(st * SR)
        n1 = min(total, int((st + dur) * SR))
        for i in range(n0, n1):
            t = (i - n0) / SR
            amp = min(1.0, t / ATTACK) * math.exp(-t / tau)
            s = 0.0
            for h, w in enumerate(harm, start=1):
                if w:
                    s += w * math.sin(2 * math.pi * freq * h * t)
            buf[i] += gain * amp * s
    nf = int(FADE_OUT * SR)
    for i in range(max(0, total - nf), total):
        buf[i] *= (total - i) / nf
    peak = max(1e-9, max(abs(x) for x in buf))
    return [x / peak * PEAK for x in buf]


def write_wav(path, buf):
    frames = b"".join(
        struct.pack("<h", int(max(-1.0, min(1.0, x)) * 32767)) for x in buf
    )
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(frames)


SOUNDS = {
    # Chime doux : quinte ascendante, harmoniques clochette.
    "done": [
        note(659.26, 0.00, 0.45, gain=0.55),               # E5
        note(987.77, 0.16, 0.55, gain=0.50),               # B5
    ],
    # Deux-ton : appel bref puis montée interrogative, plus sec.
    "question": [
        note(587.33, 0.00, 0.13, gain=0.50, tau=0.09,
             harm=(1.0, 0.20, 0.05)),                      # D5
        note(880.00, 0.15, 0.30, gain=0.50, tau=0.13,
             harm=(1.0, 0.20, 0.05)),                      # A5
    ],
    # Tonalité grave : quinte descendante, fondamental dominant, lente
    # extinction — sérieux sans être agressif.
    "error": [
        note(220.00, 0.00, 0.22, gain=0.60, tau=0.16,
             harm=(1.0, 0.15, 0.04)),                      # A3
        note(164.81, 0.24, 0.38, gain=0.60, tau=0.18,
             harm=(1.0, 0.15, 0.04)),                      # E3
    ],
}


def main():
    os.makedirs(OUT, exist_ok=True)
    for name, notes in SOUNDS.items():
        path = os.path.join(OUT, name + ".wav")
        write_wav(path, render(notes))
        dur = max(st + d for _, st, d, *_ in notes)
        print(f"{name}.wav  ({dur:.2f} s)")


if __name__ == "__main__":
    main()
