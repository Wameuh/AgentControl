#!/usr/bin/env python3
"""Génère l'icône AgentControl : le même cristal que les diagrammes du CV
(boundary irrégulier + réseau intérieur + Delaunay), palette "agent"
(améthyste). Produit :
  - public/gem.svg            (favicon / usage web)
  - src-tauri/icons/icon.png  (512)
  - src-tauri/icons/32x32.png
  - src-tauri/icons/128x128.png
  - src-tauri/icons/icon.ico  (16..256 multi-tailles)
  - src-tauri/icons/nsis-*.bmp (habillage du wizard d'installation Windows)
Usage : python3 client/tools/gem_icon.py
"""
import math
import os
from PIL import Image, ImageDraw, ImageFilter, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
CLIENT = os.path.dirname(HERE)
M32 = 0xFFFFFFFF

# Palette "neutral" (sauge verte) — identité de l'app
PALETTE = ["#4a573e", "#727555", "#8f8665", "#38472f",
           "#5c6949", "#626747", "#a19673", "#455139"]
BASE = "#303b29"


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


def render_png(boundary, faces, size, palette=PALETTE, base=BASE):
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    layer = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    t, _ = transform(boundary, size)
    d.polygon([t(p) for p in boundary], fill=base)
    for pts, tone in faces:
        d.polygon([t(p) for p in pts], fill=palette[tone])
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).polygon([t(p) for p in boundary], fill=255)
    img.paste(layer, (0, 0), mask)
    return img


def render_svg(boundary, faces, size=256, palette=PALETTE, base=BASE):
    t, (x0, y0, w, h) = transform(boundary, size)
    x0 -= size * .10 / .8
    y0 -= size * .10 / .8
    loc = lambda pts: " ".join(f"{p[0]:.1f},{p[1]:.1f}" for p in pts)
    polys = "".join(
        f'<polygon points="{loc([t(p) for p in pts])}" fill="{palette[tone]}"/>'
        for pts, tone in faces)
    vb = f"0 0 {size} {size}"
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb}">'
        f'<defs><clipPath id="c"><polygon points="{loc([t(p) for p in boundary])}"/></clipPath></defs>'
        f'<g clip-path="url(#c)"><polygon points="{loc([t(p) for p in boundary])}" fill="{base}"/>'
        f"{polys}</g></svg>")


# Variantes d'état — mêmes facettes (même seed), autre famille de teintes.
# Utilisées par set_app_icon : gold = agent actif, violet = compaction,
# red = question en attente, grey = aucun agent connecté.
# "violet" reprend la palette "agent" de src/crystal.ts (améthyste).
VARIANTS = {
    "gold": (["#8a5510", "#b57619", "#d99a2b", "#6b3f0c",
              "#9c6414", "#c9852a", "#eec25e", "#7d4e10"], "#4e2f08"),
    "violet": (["#74608f", "#a087bb", "#b69acc", "#4b395f",
                "#8e72a8", "#816795", "#ceb1e3", "#624a7d"], "#49355d"),
    "red": (["#7a352b", "#9c4a3a", "#b95f49", "#5c281f",
             "#8a4132", "#a85544", "#cf7f63", "#6b3026"], "#45201a"),
    "grey": (["#4d514c", "#6f7368", "#8c8f81", "#3a3e38",
              "#5c6057", "#676b5f", "#a6a893", "#44483f"], "#2e322e"),
}

BG = (18, 22, 20, 255)  # #121614 — ic_launcher_background / tuiles


def fit_paste(canvas, img, ratio):
    """Gemme recadrée, centrée, max dim = ratio * taille du canvas."""
    s = canvas.size[0]
    gem = img.crop(img.getbbox())
    k = s * ratio / max(gem.size)
    g2 = gem.resize((max(1, int(gem.width * k)), max(1, int(gem.height * k))),
                    Image.LANCZOS)
    canvas.paste(g2, ((s - g2.width) // 2, (s - g2.height) // 2), g2)


def write_android(icons, big):
    """Adaptive icon : le masque launcher ne montre que le ~66% central —
    la gemme entière doit tenir dans la zone de sécurité."""
    dens = {"mdpi": (108, 48), "hdpi": (162, 72), "xhdpi": (216, 96),
            "xxhdpi": (324, 144), "xxxhdpi": (432, 192)}
    for name, (fg, lg) in dens.items():
        d = os.path.join(icons, "android", "mipmap-" + name)
        os.makedirs(d, exist_ok=True)
        layer = Image.new("RGBA", (fg, fg), (0, 0, 0, 0))
        fit_paste(layer, big, 0.60)
        layer.save(os.path.join(d, "ic_launcher_foreground.png"))
        sq = Image.new("RGBA", (lg, lg), BG)
        fit_paste(sq, big, 0.72)
        sq.save(os.path.join(d, "ic_launcher.png"))
        rd = Image.new("RGBA", (lg, lg), (0, 0, 0, 0))
        ImageDraw.Draw(rd).ellipse((0, 0, lg - 1, lg - 1), fill=BG)
        fit_paste(rd, big, 0.62)
        rd.save(os.path.join(d, "ic_launcher_round.png"))


def write_ios(icons, big):
    """iOS : icônes opaques obligatoires — RGB, pas de canal alpha
    (l'App Store rejette tout canal alpha, même opaque)."""
    pts = [(20, (1, 2, 3)), (29, (1, 2, 3)), (40, (1, 2, 3)),
           (60, (2, 3)), (76, (1, 2)), (83.5, (2,))]
    d = os.path.join(icons, "ios")
    os.makedirs(d, exist_ok=True)
    for pt, scales in pts:
        for sc in scales:
            n = int(round(pt * sc))
            tile = Image.new("RGB", (n, n), BG)
            fit_paste(tile, big, 0.80)
            p = f"AppIcon-{pt:g}x{pt:g}@{sc}x.png"
            tile.save(os.path.join(d, p))
            # Slots iPad dupliqués par tauri icon : AppIcon-20x20@2x-1.png
            alt = os.path.join(d, p.replace(".png", "-1.png"))
            if os.path.exists(alt):
                tile.save(alt)
    tile = Image.new("RGB", (1024, 1024), BG)
    fit_paste(tile, big, 0.80)
    tile.save(os.path.join(d, "AppIcon-512@2x.png"))


def write_squares(icons, big):
    for s in (30, 44, 71, 89, 107, 142, 150, 284, 310):
        tile = Image.new("RGBA", (s, s), (0, 0, 0, 0))
        fit_paste(tile, big, 0.88)
        tile.save(os.path.join(icons, f"Square{s}x{s}Logo.png"))
    tile = Image.new("RGBA", (50, 50), (0, 0, 0, 0))
    fit_paste(tile, big, 0.88)
    tile.save(os.path.join(icons, "StoreLogo.png"))


# ---- Wizard NSIS ----------------------------------------------------------
# Le wizard NSIS classique ne permet pas un vrai thème sombre (fonds de
# pages, textes et boutons restent ceux du système) — on habille les deux
# zones bitmap aux couleurs de l'app : MUI_WELCOMEFINISHPAGE_BITMAP
# (panneau latéral des pages Welcome/Finish) et MUI_HEADERIMAGE_BITMAP
# (bandeau des pages internes). Palette :root de base.css (thème sombre) ;
# wordmark dans la fonte de l'app. Le vrai thème dark exigerait nsis
# moderne/extra — hors scope.
NSIS_BG_TOP = (22, 28, 24)     # --panel-solid #161c18
NSIS_BG_BOT = (13, 18, 14)     # --bg #121614 assombri vers le bas
NSIS_INK = "#ece6d8"           # --ink
NSIS_ACCENT = "#cdb68c"        # --sand / --accent
NSIS_FONT = os.path.join(CLIENT, "public", "fonts",
                        "InstrumentSans-Medium.ttf")


def _nsis_gradient(w, h):
    img = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(img)
    for y in range(h):
        k = y / (h - 1)
        d.line([(0, y), (w, y)],
               fill=tuple(round(a + (b - a) * k)
                          for a, b in zip(NSIS_BG_TOP, NSIS_BG_BOT)))
    return img


def _rgb(hexs):
    return tuple(int(hexs[i:i + 2], 16) for i in (1, 3, 5))


def _nsis_glow(img, box):
    """Halo --accent très léger derrière la gemme (radial, flou gaussien)."""
    glow = Image.new("RGBA", img.size, (0, 0, 0, 0))
    ImageDraw.Draw(glow).ellipse(box, fill=_rgb(NSIS_ACCENT) + (22,))
    glow = glow.filter(ImageFilter.GaussianBlur(30))
    return Image.alpha_composite(img.convert("RGBA"), glow).convert("RGB")


def _nsis_paste_gem(img, gem, width, cx, cy):
    k = width / max(gem.size)
    g = gem.resize((round(gem.width * k), round(gem.height * k)),
                   Image.LANCZOS)
    img.paste(g, (round(cx - g.width / 2), round(cy - g.height / 2)), g)


def write_nsis(icons, big):
    gem = big.crop(big.getbbox())
    font = lambda s: ImageFont.truetype(NSIS_FONT, s)

    # Panneau latéral 164×314 des pages Welcome/Finish : gemme surmontée
    # d'un halo doux, wordmark --ink, trait --accent.
    img = _nsis_gradient(164, 314)
    img = _nsis_glow(img, (24, 20, 140, 164))
    _nsis_paste_gem(img, gem, 96, 82, 92)
    d = ImageDraw.Draw(img)
    d.text((82, 170), "AgentControl", font=font(17), fill=NSIS_INK,
           anchor="mm")
    d.line((62, 190, 102, 190), fill=NSIS_ACCENT, width=1)
    img.save(os.path.join(icons, "nsis-sidebar.bmp"))

    # Bandeau 150×57 des pages internes : gemme à gauche + wordmark.
    hdr = _nsis_gradient(150, 57)
    _nsis_paste_gem(hdr, gem, 36, 30, 29)
    d = ImageDraw.Draw(hdr)
    d.text((54, 29), "AgentControl", font=font(14), fill=NSIS_INK,
           anchor="lm")
    hdr.save(os.path.join(icons, "nsis-header.bmp"))


def main():
    boundary, faces = geometry("bauxite")
    icons = os.path.join(CLIENT, "src-tauri", "icons")
    pub = os.path.join(CLIENT, "public")
    os.makedirs(icons, exist_ok=True)
    os.makedirs(pub, exist_ok=True)

    big = render_png(boundary, faces, 1024)
    rgba = big.resize((512, 512), Image.LANCZOS)
    rgba.save(os.path.join(icons, "icon.png"))
    # RGBA brut pour Image::new_owned (set_icon au runtime, --no-bundle)
    with open(os.path.join(icons, "icon.rgba"), "wb") as f:
        f.write(rgba.tobytes())
    big.resize((32, 32), Image.LANCZOS).save(os.path.join(icons, "32x32.png"))
    big.resize((64, 64), Image.LANCZOS).save(os.path.join(icons, "64x64.png"))
    big.resize((128, 128), Image.LANCZOS).save(os.path.join(icons, "128x128.png"))
    big.resize((256, 256), Image.LANCZOS).save(
        os.path.join(icons, "128x128@2x.png"))
    big.save(os.path.join(icons, "icon.ico"),
             sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    with open(os.path.join(pub, "gem.svg"), "w") as f:
        f.write(render_svg(boundary, faces))

    for name, (pal, base) in VARIANTS.items():
        v = render_png(boundary, faces, 512, pal, base)
        with open(os.path.join(icons, f"icon-{name}.rgba"), "wb") as f:
            f.write(v.tobytes())

    write_android(icons, big)
    write_ios(icons, big)
    write_squares(icons, big)
    write_nsis(icons, big)
    print("icons écrites dans", icons, "et", pub)


if __name__ == "__main__":
    main()
