// ---------------------------------------------------------------------------
// Télémétrie perf locale (#297) — agrégat de compteurs/mesures versé dans
// client.log via logUi("info", …) toutes les ~5 min : UNE ligne compacte
// key=value greppable. But : transformer « ça lag » en chiffres
// (diagnostics à l'aveugle : #290, #294).
//
// Surcoût : compteurs scalaires + réservoir borné (256) par métrique de
// latence — pas d'allocation par frappe, performance.now() seul sur le
// chemin chaud. Chaque source est feature-détectée : absent = champ omis,
// jamais d'exception (WebKitGTK n'a ni longtask ni performance.memory).
// Les points chauds du core (rpc/wsclient/bridge) passent par les sondes
// core/probe.ts — injectées ici au démarrage, nulles quand OFF.
//
// Vie privée : métriques uniquement — jamais de contenu ni de chemins.
// ---------------------------------------------------------------------------

import { logUi } from "../core/dom";
import { perfProbes } from "../core/probe";

const FLUSH_MS = 5 * 60_000;
/** Taille du réservoir par métrique de latence (percentiles approchés). */
const RES_CAP = 256;
/** Garde-fou : le nombre de méthodes RPC distinctes reste borné. */
const RPC_MAX_METHODS = 32;

/** Réservoir d'échantillons borné : comptage exact + percentiles
 *  approchés sur un échantillon aléatoire — mémoire constante quelle
 *  que soit la fenêtre. */
class Reservoir {
  private buf: number[] = [];
  private seen = 0;

  add(v: number) {
    this.seen++;
    if (this.buf.length < RES_CAP) {
      this.buf.push(v);
    } else {
      const j = (Math.random() * this.seen) | 0;
      if (j < RES_CAP) this.buf[j] = v;
    }
  }

  get n() {
    return this.seen;
  }

  /** Percentile sur l'échantillon trié (rang le plus proche). */
  pct(p: number): number {
    if (!this.buf.length) return 0;
    const s = [...this.buf].sort((a, b) => a - b);
    return s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))];
  }

  max(): number {
    let m = 0;
    for (const v of this.buf) if (v > m) m = v;
    return m;
  }
}

interface Agg {
  /** Handlers `input` de #input mesurés (≙ frappes). */
  keys: number;
  inH: Reservoir; // durée du handler input (ms)
  inPaint: Reservoir; // délai event → frame (ms)
  ltN: number; // long tasks
  ltTotal: number; // ms cumulés
  ltWorst: number; // pire ms
  flush: Reservoir; // batch render flushLive (ms)
  flushNodes: number; // nœuds .ev ajoutés sur la fenêtre
  fit: Reservoir; // fitInput() (ms)
  wsN: number; // messages WS reçus (mode web)
  wsBytes: number; // octets reçus
  parse: Reservoir; // durée JSON.parse (ms)
  pushN: number; // deliveries d'events Tauri (desktop/Android)
  rpc: Map<string, { n: number; res: Reservoir }>;
  jankN: number; // frames > 50 ms
  jankWorst: number; // pire intervalle de frame (ms)
}

const freshAgg = (): Agg => ({
  keys: 0,
  inH: new Reservoir(),
  inPaint: new Reservoir(),
  ltN: 0,
  ltTotal: 0,
  ltWorst: 0,
  flush: new Reservoir(),
  flushNodes: 0,
  fit: new Reservoir(),
  wsN: 0,
  wsBytes: 0,
  parse: new Reservoir(),
  pushN: 0,
  rpc: new Map(),
  jankN: 0,
  jankWorst: 0,
});

let agg = freshAgg();
let enabled = false;
let timer = 0;
let po: PerformanceObserver | null = null;
let jankRaf = 0;
let lastFrame = -1;

// ---- Sonde temps réel (chemin chaud : une fonction + un push) -------------

export function telemetryActive() {
  return enabled;
}

/** Durée du handler `input` de #input (mesurée côté Composer). */
export function probeInputHandler(ms: number) {
  if (!enabled) return;
  agg.keys++;
  agg.inH.add(ms);
}

/** Délai event → paint : `t0` = performance.now() au début du handler ;
 *  la sonde rAF s'exécute en fin de frame (enregistrée après le rAF de
 *  coalescence UI du composer) → delta ≈ jusqu'au paint. */
export function probeInputPaint(t0: number) {
  if (!enabled) return;
  requestAnimationFrame(() => {
    if (enabled) agg.inPaint.add(performance.now() - t0);
  });
}

/** Batch render des events live (ChatView.flushLive) : durée + nœuds. */
export function probeDomFlush(ms: number, nodes: number) {
  if (!enabled) return;
  agg.flush.add(ms);
  agg.flushNodes += nodes;
}

/** Durée de fitInput() (Composer) — suspect n°1 des lag de frappe. */
export function probeFitInput(ms: number) {
  if (!enabled) return;
  agg.fit.add(ms);
}

// ---- Sondes core (features → core via perfProbes) ------------------------

function wireCoreProbes() {
  perfProbes.rpc = (method, ms) => {
    if (!enabled) return;
    let e = agg.rpc.get(method);
    if (!e) {
      if (agg.rpc.size >= RPC_MAX_METHODS) return;
      e = { n: 0, res: new Reservoir() };
      agg.rpc.set(method, e);
    }
    e.n++;
    e.res.add(ms);
  };
  perfProbes.wsMsg = (bytes, parseMs) => {
    if (!enabled) return;
    agg.wsN++;
    agg.wsBytes += bytes;
    agg.parse.add(parseMs);
  };
  perfProbes.push = () => {
    if (enabled) agg.pushN++;
  };
}

// ---- Observateurs (démarrés seulement quand la télémétrie est ON) --------

function attachLongtask() {
  try {
    if (
      typeof PerformanceObserver !== "function" ||
      !PerformanceObserver.supportedEntryTypes?.includes("longtask")
    )
      return;
    po = new PerformanceObserver((list) => {
      for (const e of list.getEntries()) {
        agg.ltN++;
        agg.ltTotal += e.duration;
        if (e.duration > agg.ltWorst) agg.ltWorst = e.duration;
      }
    });
    po.observe({ entryTypes: ["longtask"] });
  } catch {
    // API présente mais observe() refusé (WebKitGTK ancien) — on laisse
    // le champ longtasks omis plutôt que de casser le boot.
    po = null;
  }
}

/** Jank frame : deltas entre rAF consécutifs — une boucle rAF par frame,
 *  un timestamp et un compteur, rien d'autre. */
function jankTick(t: number) {
  jankRaf = requestAnimationFrame(jankTick);
  if (lastFrame >= 0) {
    const d = t - lastFrame;
    if (d > 50) {
      agg.jankN++;
      if (d > agg.jankWorst) agg.jankWorst = d;
    }
  }
  lastFrame = t;
}

// ---- Formatage ------------------------------------------------------------

const ms = (v: number) => String(Math.round(v * 10) / 10);
const kb = (b: number) => (b / 1024).toFixed(0) + "k";
/** Compact : 999 → "999", 1234 → "1.2k", 2.1M → "2.1M". */
const cnt = (n: number) =>
  n >= 1_000_000
    ? (n / 1_000_000).toFixed(1) + "M"
    : n >= 1000
      ? (n / 1000).toFixed(1) + "k"
      : String(n);

function buildLine(tag: string): string {
  const a = agg;
  const p: string[] = [`telemetry ${tag}:`, `keys=${a.keys}`];
  if (a.inH.n)
    p.push(
      `input_h_n=${a.inH.n}`,
      `input_h_p50=${ms(a.inH.pct(50))}`,
      `input_h_p95=${ms(a.inH.pct(95))}`,
      `input_h_max=${ms(a.inH.max())}`,
    );
  if (a.inPaint.n)
    p.push(
      `input_paint_n=${a.inPaint.n}`,
      `input_paint_p50=${ms(a.inPaint.pct(50))}`,
      `input_paint_p95=${ms(a.inPaint.pct(95))}`,
      `input_paint_max=${ms(a.inPaint.max())}`,
    );
  if (a.ltN) p.push(`longtasks=${a.ltN}/${Math.round(a.ltTotal)}/${Math.round(a.ltWorst)}`);
  if (a.flush.n)
    p.push(
      `domflush=${a.flush.n}`,
      `domflush_p95=${ms(a.flush.pct(95))}`,
      `domflush_max=${ms(a.flush.max())}`,
      `domnodes=${a.flushNodes}`,
    );
  if (a.fit.n)
    p.push(`fit=${a.fit.n}`, `fit_p95=${ms(a.fit.pct(95))}`, `fit_max=${ms(a.fit.max())}`);
  if (a.wsN)
    p.push(`ws=${a.wsN}/${kb(a.wsBytes)}`, `parse_p95=${ms(a.parse.pct(95))}`);
  if (a.pushN) p.push(`push=${a.pushN}`);
  if (a.rpc.size) {
    const top = [...a.rpc.entries()]
      .sort((x, y) => y[1].n - x[1].n)
      .slice(0, 3)
      .map(([m, e]) => `${m}:${e.n}/${ms(e.res.pct(95))}ms`)
      .join(",");
    p.push(`rpc=${top}`);
  }
  const mem = (performance as { memory?: { usedJSHeapSize: number } }).memory;
  if (mem) p.push(`heap=${Math.round(mem.usedJSHeapSize / 1048576)}MB`);
  p.push(`nodes=${cnt(document.getElementsByTagName("*").length)}`);
  const ev = document.getElementById("events");
  if (ev) p.push(`ev_children=${cnt(ev.childElementCount)}`);
  p.push(`jank=${a.jankN}/${Math.round(a.jankWorst)}ms`);
  return p.join(" ");
}

function finish() {
  logUi("info", buildLine("5m"));
  agg = freshAgg();
}

// ---- Cycle de vie ----------------------------------------------------------

function start() {
  if (timer) return;
  wireCoreProbes();
  attachLongtask();
  lastFrame = -1;
  jankRaf = requestAnimationFrame(jankTick);
  timer = window.setInterval(finish, FLUSH_MS);
}

function stop() {
  if (!timer) return;
  window.clearInterval(timer);
  timer = 0;
  po?.disconnect();
  po = null;
  if (jankRaf) cancelAnimationFrame(jankRaf);
  jankRaf = 0;
  perfProbes.rpc = perfProbes.wsMsg = perfProbes.push = null;
  agg = freshAgg();
}

export function setTelemetryEnabled(on: boolean) {
  if (on === enabled) return;
  enabled = on;
  if (on) start();
  else stop();
}

// ---- Préférence (checkbox Paramètres → Diagnostics) -----------------------

export const telemetryPref = () => localStorage.getItem("ac-telemetry") !== "0";

export function setTelemetryPref(on: boolean) {
  localStorage.setItem("ac-telemetry", on ? "1" : "0");
  setTelemetryEnabled(on);
}

/** Boot : expose le dump debug et applique la préférence persistée.
 *  `window.__telemetry()` écrit l'agrégat courant dans client.log sans
 *  reset — pour corréler un lag « à chaud ». */
export function initTelemetry() {
  (window as { __telemetry?: () => string }).__telemetry = () => {
    const line = buildLine("dump");
    logUi("info", line);
    return line;
  };
  setTelemetryEnabled(telemetryPref());
}
