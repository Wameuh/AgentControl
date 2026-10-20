#!/usr/bin/env python3
"""Génère l'icône AgentControl : le même cristal que les diagrammes du CV
(boundary irrégulier + réseau intérieur + Delaunay), palette "agent"
(améthyste). Produit :
  - public/gem.svg            (favicon / usage web)
  - src-tauri/icons/icon.png  (512)
  - src-tauri/icons/32x32.png
  - src-tauri/icons/128x128.png
  - src-tauri/icons/icon.ico  (16..256 multi-tailles)
Usage : python3 client/tools/gem_icon.py
"""
import math
import os
from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
CLIENT = os.path.dirname(HERE)
M32 = 0xFFFFFFFF

PALETTE = ["#74608f", "#a087bb", "#b69acc", "#4b395f",
           "#8e72a8", "#816795", "#ceb1e3", "#624a7d"]
BASE = "#49355d"


def imul(a, b):
    return (a * b) & M32


def rng_for(key):
    seed = 2166136261
    for ch in key:
        seed = imul(seed ^ ord(ch), 16777619)

    def rnd():
        nonlocal seed
        seed = (seed + 0x6D2B79F5) & M32
        v = imul(seed ^ (seed >> 15), 1 | seed)
        v = ((v + imul(v ^ (v >> 7), 61 | v)) ^ v) & M32
        return ((v ^ (v >> 14)) & M32) / 4294967296.0

    return rnd


def geometry(key):
    random = rng_for(key)
    count = 9 + int(random() * 4)
    rotation = random() * math.pi * 2
    rx, ry = 32 + random() * 15, 27 + random() * 13
    lean = (random() - 0.5) * 16
    boundary = []
    for i in range(count):
        angle = (i + (random() - 0.5) * 0.3) / count * math.pi * 2 + rotation
        radius = 0.72 + random() * 0.38
        boundary.append(((math.cos(angle) * rx + math.sin(angle) * lean) * radius,
                         math.sin(angle) * ry * radius - 8))
    points = list(boundary)
    for y in range(3):
        for x in range(3):
            points.append(((x - 1) * 17 + (random() - 0.5) * 9,
                           (y - 1) * 16 - 8 + (random() - 0.5) * 9))
    size = len(points)
    points += [(-1000, -1000), (1000, -1000), (0, 1000)]
    triangles = [[size, size + 1, size + 2]]

    def contains(tri, p):
        a, b, c = (points[i] for i in tri)
        div = 2 * (a[0] * (b[1] - c[1]) + b[0] * (c[1] - a[1]) + c[0] * (a[1] - b[1]))
        if abs(div) < 1e-8:
            return False
        aa, bb, cc = (v[0] * v[0] + v[1] * v[1] for v in (a, b, c))
        cx = (aa * (b[1] - c[1]) + bb * (c[1] - a[1]) + cc * (a[1] - b[1])) / div
        cy = (aa * (c[0] - b[0]) + bb * (a[0] - c[0]) + cc * (b[0] - a[0])) / div
        return (p[0] - cx) ** 2 + (p[1] - cy) ** 2 <= (a[0] - cx) ** 2 + (a[1] - cy) ** 2 + 1e-7

    for i in range(size):
        bad = {j for j, t in enumerate(triangles) if contains(t, points[i])}
        edges = {}
        for j in bad:
            for k in range(3):
                e = (triangles[j][k], triangles[j][(k + 1) % 3])
                name = tuple(sorted(e))
                if name in edges:
                    del edges[name]
                else:
                    edges[name] = e
        triangles = [t for j, t in enumerate(triangles) if j not in bad]
        for e in edges.values():
            triangles.append([e[0], e[1], i])
    faces = [([points[i] for i in t], int(random() * 8))
             for t in triangles if all(i < size for i in t)]
    return boundary, faces


def transform(boundary, size, pad_ratio=0.10):
    xs = [p[0] for p in boundary]
    ys = [p[1] for p in boundary]
    x0, y0 = min(xs), min(ys)
    w, h = max(xs) - x0, max(ys) - y0
    pad = size * pad_ratio
    sc = (size - 2 * pad) / max(w, h)

    def t(p):
        return ((p[0] - x0) * sc + pad + (size - 2 * pad - w * sc) / 2,
                (p[1] - y0) * sc + pad + (size - 2 * pad - h * sc) / 2)
    return t, (x0, y0, w, h)


def render_png(boundary, faces, size):
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    layer = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    t, _ = transform(boundary, size)
    d.polygon([t(p) for p in boundary], fill=BASE)
    for pts, tone in faces:
        d.polygon([t(p) for p in pts], fill=PALETTE[tone])
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).polygon([t(p) for p in boundary], fill=255)
    img.paste(layer, (0, 0), mask)
    return img


def render_svg(boundary, faces, size=256):
    t, (x0, y0, w, h) = transform(boundary, size)
    x0 -= size * .10 / .8
    y0 -= size * .10 / .8
    loc = lambda pts: " ".join(f"{p[0]:.1f},{p[1]:.1f}" for p in pts)
    polys = "".join(
        f'<polygon points="{loc([t(p) for p in pts])}" fill="{PALETTE[tone]}"/>'
        for pts, tone in faces)
    vb = f"0 0 {size} {size}"
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb}">'
        f'<defs><clipPath id="c"><polygon points="{loc([t(p) for p in boundary])}"/></clipPath></defs>'
        f'<g clip-path="url(#c)"><polygon points="{loc([t(p) for p in boundary])}" fill="{BASE}"/>'
        f"{polys}</g></svg>")


def main():
    boundary, faces = geometry("agentcontrol")
    icons = os.path.join(CLIENT, "src-tauri", "icons")
    pub = os.path.join(CLIENT, "public")
    os.makedirs(icons, exist_ok=True)
    os.makedirs(pub, exist_ok=True)

    big = render_png(boundary, faces, 1024)
    big.resize((512, 512), Image.LANCZOS).save(os.path.join(icons, "icon.png"))
    big.resize((32, 32), Image.LANCZOS).save(os.path.join(icons, "32x32.png"))
    big.resize((128, 128), Image.LANCZOS).save(os.path.join(icons, "128x128.png"))
    big.save(os.path.join(icons, "icon.ico"),
             sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    with open(os.path.join(pub, "gem.svg"), "w") as f:
        f.write(render_svg(boundary, faces))
    print("icons écrites dans", icons, "et", pub)


if __name__ == "__main__":
    main()
