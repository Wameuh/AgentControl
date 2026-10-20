// Cristal façon "diagrammes du CV" : polygone de contour irrégulier +
// réseau de points intérieurs + triangulation de Delaunay. Chaque facette
// pioche dans une palette par rôle (mêmes couleurs que diagram-core.js).
// Le même algorithme est porté en Python (tools/gem_icon.py) pour l'icône.

const PALETTES: Record<string, string[]> = {
  agent: ["#74608f", "#a087bb", "#b69acc", "#4b395f", "#8e72a8", "#816795", "#ceb1e3", "#624a7d"],
  human: ["#8a754d", "#b19b68", "#cfb77e", "#625034", "#a78d5b", "#947c4f", "#e0c996", "#78603e"],
  neutral: ["#4a573e", "#727555", "#8f8665", "#38472f", "#5c6949", "#626747", "#a19673", "#455139"],
  slate: ["#74777b", "#96999d", "#babdc1", "#474b50", "#85898f", "#696e75", "#d1d3d5", "#5b6066"],
  inference: ["#a8504a", "#ce7770", "#e89486", "#6b302e", "#b9635c", "#9e4b49", "#f4b0a0", "#813937"],
};

const BASES: Record<string, string> = {
  agent: "#49355d",
  human: "#625034",
  neutral: "#303b29",
  slate: "#45494e",
  inference: "#5c2929",
};

type Pt = [number, number];
interface Geometry {
  boundary: Pt[];
  faces: { points: Pt[]; tone: number }[];
}

const cache = new Map<string, Geometry>();

function mulberry(key: string): () => number {
  let seed = 2166136261;
  for (const ch of key) seed = Math.imul(seed ^ ch.charCodeAt(0), 16777619);
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let v = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    v = (v + Math.imul(v ^ (v >>> 7), 61 | v)) ^ v;
    return ((v ^ (v >>> 14)) >>> 0) / 4294967296;
  };
}

function geometry(key: string): Geometry {
  const hit = cache.get(key);
  if (hit) return hit;
  const random = mulberry(key);
  const count = 9 + Math.floor(random() * 4);
  const rotation = random() * Math.PI * 2;
  const rx = 32 + random() * 15;
  const ry = 27 + random() * 13;
  const lean = (random() - 0.5) * 16;
  const boundary: Pt[] = [];
  for (let i = 0; i < count; i++) {
    const angle = ((i + (random() - 0.5) * 0.3) / count) * Math.PI * 2 + rotation;
    const radius = 0.72 + random() * 0.38;
    boundary.push([
      (Math.cos(angle) * rx + Math.sin(angle) * lean) * radius,
      Math.sin(angle) * ry * radius - 8,
    ]);
  }
  const points: Pt[] = [...boundary];
  // Réseau intérieur jitteré → beaucoup de jonctions de facettes.
  for (let y = 0; y < 3; y++) {
    for (let x = 0; x < 3; x++) {
      points.push([(x - 1) * 17 + (random() - 0.5) * 9, (y - 1) * 16 - 8 + (random() - 0.5) * 9]);
    }
  }
  const size = points.length;
  points.push([-1000, -1000], [1000, -1000], [0, 1000]);
  let triangles: number[][] = [[size, size + 1, size + 2]];
  const circleContains = (tri: number[], p: Pt): boolean => {
    const [a, b, c] = tri.map((i) => points[i]);
    const div = 2 * (a[0] * (b[1] - c[1]) + b[0] * (c[1] - a[1]) + c[0] * (a[1] - b[1]));
    if (Math.abs(div) < 1e-8) return false;
    const [aa, bb, cc] = [a, b, c].map((v) => v[0] * v[0] + v[1] * v[1]);
    const cx = (aa * (b[1] - c[1]) + bb * (c[1] - a[1]) + cc * (a[1] - b[1])) / div;
    const cy = (aa * (c[0] - b[0]) + bb * (a[0] - c[0]) + cc * (b[0] - a[0])) / div;
    return (p[0] - cx) ** 2 + (p[1] - cy) ** 2 <= (a[0] - cx) ** 2 + (a[1] - cy) ** 2 + 1e-7;
  };
  for (let i = 0; i < size; i++) {
    const bad = new Set(triangles.filter((t) => circleContains(t, points[i])));
    const edges = new Map<string, [number, number]>();
    for (const t of bad) {
      for (let j = 0; j < 3; j++) {
        const edge: [number, number] = [t[j], t[(j + 1) % 3]];
        const name = [...edge].sort((a, b) => a - b).join(":");
        if (edges.has(name)) edges.delete(name);
        else edges.set(name, edge);
      }
    }
    triangles = triangles.filter((t) => !bad.has(t));
    for (const e of edges.values()) triangles.push([...e, i]);
  }
  const geo: Geometry = {
    boundary,
    faces: triangles
      .filter((t) => t.every((i) => i < size))
      .map((t) => ({ points: t.map((i) => points[i]), tone: Math.floor(random() * 8) })),
  };
  cache.set(key, geo);
  return geo;
}

let uid = 0;

/** SVG complet d'une gemme. `role` ∈ agent|human|neutral|slate|inference. */
export function gemSVG(role: string): string {
  const g = geometry("agentcontrol");
  const pal = PALETTES[role] ?? PALETTES.neutral;
  const base = BASES[role] ?? BASES.slate;
  const xs = g.boundary.map((p) => p[0]);
  const ys = g.boundary.map((p) => p[1]);
  const x0 = Math.min(...xs) - 2;
  const y0 = Math.min(...ys) - 2;
  const w = Math.max(...xs) - x0 + 2;
  const h = Math.max(...ys) - y0 + 2;
  const loc = (pts: Pt[]) => pts.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join(" ");
  const id = `gm${uid++}`;
  const faces = g.faces
    .map((f) => `<polygon points="${loc(f.points)}" fill="${pal[f.tone]}"/>`)
    .join("");
  return (
    `<svg class="gem" viewBox="${x0.toFixed(1)} ${y0.toFixed(1)} ${w.toFixed(1)} ${h.toFixed(1)}"` +
    ` preserveAspectRatio="xMidYMid meet"><defs><clipPath id="${id}">` +
    `<polygon points="${loc(g.boundary)}"/></clipPath></defs>` +
    `<g clip-path="url(#${id})"><polygon points="${loc(g.boundary)}" fill="${base}"/>${faces}</g></svg>`
  );
}
