// Pan / pincement / double tap sur le diagramme (N5). Le rendu repose
// le viewBox à chaque poll : on le relit via MutationObserver comme vue
// de référence, et le déplacement/zoom utilisateur est réappliqué
// par-dessus — le geste survit aux re-rendus.

import { $ } from "../core/dom";
import { isAndroid } from "../core/chrome";

interface Vb {
  x: number;
  y: number;
  w: number;
  h: number;
}

const parseVb = (s: string | null): Vb | null => {
  const n = (s ?? "")
    .trim()
    .split(/[\s,]+/)
    .map(Number);
  return n.length === 4 && n.every(Number.isFinite) && n[2] > 0 && n[3] > 0
    ? { x: n[0], y: n[1], w: n[2], h: n[3] }
    : null;
};
const fmt = (v: Vb) => `${v.x} ${v.y} ${v.w} ${v.h}`;
const clamp = (v: number, lo: number, hi: number) =>
  Math.min(hi, Math.max(lo, v));

let ref: Vb | null = null; // vue de référence (dernier rendu)
let cx = 0;
let cy = 0; // centre de la vue courante, en unités SVG
let zoom = 1; // relatif à ref — borné [0.5, 4]
let applied = ""; // viewBox posé par nous (garde-fou de l'observateur)

interface Pt {
  x0: number;
  y0: number;
  x: number;
  y: number;
}
const pts = new Map<number, Pt>();
interface Pinch {
  d0: number;
  z0: number;
  cx0: number;
  cy0: number;
  mx0: number;
  my0: number;
}
let pinch: Pinch | null = null;
let moved = 0; // plus grand déplacement du geste courant (px)
let lastTap = 0;

const view = (): Vb | null =>
  ref
    ? {
        x: cx - ref.w / zoom / 2,
        y: cy - ref.h / zoom / 2,
        w: ref.w / zoom,
        h: ref.h / zoom,
      }
    : null;

// px → unités SVG (preserveAspectRatio=meet : échelle uniforme = max).
const scale = (svg: SVGSVGElement): number => {
  const r = svg.getBoundingClientRect();
  const v = view();
  return v && r.width > 0 ? Math.max(v.w / r.width, v.h / r.height) : 1;
};

const mid = (a: Pt, b: Pt) => ({ x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 });
const dist = (a: Pt, b: Pt) => Math.hypot(a.x - b.x, a.y - b.y);

function apply(svg: SVGSVGElement) {
  const v = view();
  if (!v) return;
  const s = fmt(v);
  if (s === svg.getAttribute("viewBox")) {
    applied = s;
    return;
  }
  applied = s;
  svg.setAttribute("viewBox", s);
}

function armPinch() {
  const [a, b] = [...pts.values()];
  if (!a || !b || !ref) return;
  const m = mid(a, b);
  pinch = { d0: dist(a, b) || 1, z0: zoom, cx0: cx, cy0: cy, mx0: m.x, my0: m.y };
}

function movePinch(svg: SVGSVGElement) {
  if (!pinch || !ref) return;
  const [a, b] = [...pts.values()];
  if (!a || !b) return;
  const r = svg.getBoundingClientRect();
  const s0 = Math.max(
    ref.w / pinch.z0 / Math.max(r.width, 1),
    ref.h / pinch.z0 / Math.max(r.height, 1),
  );
  // Point SVG sous le milieu initial — il doit rester sous le milieu courant.
  const px = pinch.cx0 + (pinch.mx0 - (r.left + r.width / 2)) * s0;
  const py = pinch.cy0 + (pinch.my0 - (r.top + r.height / 2)) * s0;
  zoom = clamp(pinch.z0 * (dist(a, b) / pinch.d0), 0.5, 4);
  const s = scale(svg);
  const m = mid(a, b);
  cx = px - (m.x - (r.left + r.width / 2)) * s;
  cy = py - (m.y - (r.top + r.height / 2)) * s;
  apply(svg);
}

const eat = (e: Event) => {
  e.stopPropagation();
  e.preventDefault();
};

/** Avale le click concluant un geste (drag/pinch/double tap) pour ne pas
 *  déclencher activate() ; désarmé au prochain pointerdown réel. */
function swallowClick(svg: SVGSVGElement) {
  svg.addEventListener("click", eat, { capture: true, once: true });
}

export function initMapGestures() {
  if (!isAndroid()) return;
  const svg = document.getElementById("map-svg") as SVGSVGElement | null;
  if (!svg) return;

  ref = parseVb(svg.getAttribute("viewBox"));
  if (ref) {
    cx = ref.x + ref.w / 2;
    cy = ref.y + ref.h / 2;
    applied = fmt(ref);
  }

  // Chaque réécriture externe du viewBox (poll du module de rendu) est la
  // nouvelle vue de référence ; le delta utilisateur est recentré dessus.
  new MutationObserver(() => {
    const v = parseVb(svg.getAttribute("viewBox"));
    if (!v || fmt(v) === applied) return;
    if (ref) {
      cx += v.x + v.w / 2 - (ref.x + ref.w / 2);
      cy += v.y + v.h / 2 - (ref.y + ref.h / 2);
    } else {
      cx = v.x + v.w / 2;
      cy = v.y + v.h / 2;
    }
    ref = v;
    if (zoom !== 1 || fmt(view() as Vb) !== fmt(v)) apply(svg);
  }).observe(svg, { attributes: true, attributeFilter: ["viewBox"] });

  svg.addEventListener("pointerdown", (e) => {
    svg.removeEventListener("click", eat, { capture: true });
    svg.setPointerCapture(e.pointerId);
    pts.set(e.pointerId, { x0: e.clientX, y0: e.clientY, x: e.clientX, y: e.clientY });
    if (pts.size === 1) moved = 0;
    if (pts.size === 2) armPinch();
  });
  svg.addEventListener("pointermove", (e) => {
    const p = pts.get(e.pointerId);
    if (!p) return;
    const dx = e.clientX - p.x;
    const dy = e.clientY - p.y;
    p.x = e.clientX;
    p.y = e.clientY;
    moved = Math.max(moved, Math.hypot(p.x - p.x0, p.y - p.y0));
    if (pts.size === 1) {
      const s = scale(svg);
      cx -= dx * s;
      cy -= dy * s;
      apply(svg);
    } else if (pts.size === 2) movePinch(svg);
  });
  const up = (e: PointerEvent) => {
    if (!pts.delete(e.pointerId)) return;
    if (pts.size === 1) {
      pinch = null;
      const p = [...pts.values()][0];
      p.x0 = p.x;
      p.y0 = p.y; // ré-ancre le reliquat du pincement en simple pan
    } else if (pts.size === 0) {
      pinch = null;
      if (moved > 8) {
        swallowClick(svg);
      } else {
        const now = Date.now();
        if (now - lastTap < 350 && ref) {
          zoom = 1;
          cx = ref.x + ref.w / 2;
          cy = ref.y + ref.h / 2;
          apply(svg);
          swallowClick(svg);
        }
        lastTap = now;
      }
    }
  };
  svg.addEventListener("pointerup", up);
  svg.addEventListener("pointercancel", up);
}
