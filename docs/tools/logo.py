#!/usr/bin/env python3
"""Generate the Mantra mark and banner.

    python3 docs/tools/logo.py      # writes docs/img/logo.{svg,png} and banner.svg

The mark is a mandala: six circles around a seventh, the oldest way of drawing "many
around one" — which is what Mantra is (one terminal, a team of agents). Six saffron
dots mark the agents, the centre carries the app's own planner glyph (a four-pointed
star), and the three colours are the TUI's own (`mantra_src/src/ui/theme.rs`), so the
logo and the product match.

Geometry is defined once and rendered twice: to SVG (crisp at any size) and, by
supersampled Pillow drawing, to PNG (for anywhere SVG is awkward).
"""

from __future__ import annotations

import math
import os

from PIL import Image, ImageDraw, ImageFont

MONO_BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"
SANS = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"

SAFFRON = (242, 165, 65)
VIOLET = (150, 130, 250)
TEAL = (60, 207, 180)
MUTED = (138, 145, 160)   # readable on GitHub's light *and* dark themes

SIZE = 256
C = SIZE / 2
R = 52          # petal radius — also the distance from centre to each petal centre
RING = 116      # outer ring
STAR = 33       # half-height of the centre star
STAR_W = 0.19   # how sharp its points are (fraction of the half-height)


def hexc(rgb) -> str:
    return "#%02x%02x%02x" % rgb


def petals():
    """The six petal centres, starting at the top and going clockwise."""
    return [(C + R * math.sin(math.radians(a)), C - R * math.cos(math.radians(a)))
            for a in range(0, 360, 60)]


def star_points(cx: float, cy: float, h: float, w: float):
    """A four-pointed sparkle (the app's ✦), as a concave polygon."""
    return [(cx, cy - h), (cx + w, cy - w), (cx + h, cy), (cx + w, cy + w),
            (cx, cy + h), (cx - w, cy + w), (cx - h, cy), (cx - w, cy - w)]


# ── SVG ─────────────────────────────────────────────────────────────────────────
def svg_mark(size: int = SIZE) -> str:
    p = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {SIZE} {SIZE}" '
         f'width="{size}" height="{size}" role="img" aria-label="Mantra">']
    p.append(f'<circle cx="{C}" cy="{C}" r="{RING}" fill="none" stroke="{hexc(TEAL)}" '
             f'stroke-width="2" stroke-opacity=".28" stroke-dasharray="3 9" stroke-linecap="round"/>')
    for i, (x, y) in enumerate(petals()):
        col = hexc(VIOLET if i % 2 == 0 else TEAL)
        p.append(f'<circle cx="{x:.2f}" cy="{y:.2f}" r="{R}" fill="none" stroke="{col}" '
                 f'stroke-width="3" stroke-opacity=".72"/>')
    p.append(f'<circle cx="{C}" cy="{C}" r="{R}" fill="none" stroke="{hexc(SAFFRON)}" stroke-width="3.5"/>')
    for x, y in petals():
        p.append(f'<circle cx="{x:.2f}" cy="{y:.2f}" r="4.5" fill="{hexc(SAFFRON)}" fill-opacity=".92"/>')
    pts = " ".join(f"{x:.2f},{y:.2f}" for x, y in star_points(C, C, STAR, STAR * STAR_W))
    p.append(f'<polygon points="{pts}" fill="{hexc(SAFFRON)}"/>')
    p.append("</svg>")
    return "\n".join(p)


def svg_banner() -> str:
    w, h = 760, 200
    mark = 132
    mx, my = 40, (h - mark) / 2
    scale = mark / SIZE
    inner = "\n".join(svg_mark().split("\n")[1:-1])
    tx = mx + mark + 34
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" role="img" aria-label="Mantra">
<g transform="translate({mx},{my}) scale({scale:.4f})">
{inner}
</g>
<text x="{tx}" y="{h / 2 - 6}" font-family="ui-monospace,SFMono-Regular,Menlo,Consolas,monospace" font-size="58" font-weight="700" letter-spacing="10" fill="{hexc(SAFFRON)}">MANTRA</text>
<text x="{tx + 3}" y="{h / 2 + 34}" font-family="ui-sans-serif,system-ui,Segoe UI,Helvetica,Arial,sans-serif" font-size="21" fill="{hexc(MUTED)}">one terminal for coding agents — alone, or as a team</text>
</svg>"""


# ── PNG (supersampled so the curves stay clean) ─────────────────────────────────
def png_mark(path: str, out_size: int = 512, ss: int = 4, quiet: bool = False):
    s = out_size * ss
    k = s / SIZE
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    def box(cx, cy, r):
        return [(cx - r) * k, (cy - r) * k, (cx + r) * k, (cy + r) * k]

    # outer dashed ring
    for a in range(0, 360, 9):
        d.arc(box(C, C, RING), a, a + 4, fill=TEAL + (72,), width=int(2 * k))
    for i, (x, y) in enumerate(petals()):
        col = (VIOLET if i % 2 == 0 else TEAL) + (184,)
        d.ellipse(box(x, y, R), outline=col, width=int(3 * k))
    d.ellipse(box(C, C, R), outline=SAFFRON + (255,), width=int(3.5 * k))
    for x, y in petals():
        d.ellipse(box(x, y, 4.5), fill=SAFFRON + (235,))
    d.polygon([(x * k, y * k) for x, y in star_points(C, C, STAR, STAR * STAR_W)], fill=SAFFRON + (255,))

    img = img.resize((out_size, out_size), Image.LANCZOS)
    img.save(path)
    if not quiet:
        print(f"  → {path}  ({out_size}×{out_size})")


def png_banner(path: str, ss: int = 3):
    """The README header: mark + wordmark + one line of what it is. Transparent, so it
    sits on GitHub's light and dark themes alike."""
    w, h, mark = 1180, 264, 200
    img = Image.new("RGBA", (w * ss, h * ss), (0, 0, 0, 0))
    m = Image.new("RGBA", (mark * ss, mark * ss), (0, 0, 0, 0))
    tmp = "/tmp/_mantra_mark.png"
    png_mark(tmp, out_size=mark * ss, ss=2, quiet=True)
    m = Image.open(tmp).convert("RGBA")
    img.paste(m, (40 * ss, (h - mark) // 2 * ss), m)

    d = ImageDraw.Draw(img)
    word = ImageFont.truetype(MONO_BOLD, 86 * ss)
    tag = ImageFont.truetype(SANS, 31 * ss)
    x, y = (40 + mark + 48) * ss, (h / 2 - 72) * ss
    for chland in "MANTRA":  # letter-spaced by hand: no tracking control in Pillow
        d.text((x, y), chland, font=word, fill=SAFFRON + (255,))
        x += d.textlength(chland, font=word) + 15 * ss
    d.text(((40 + mark + 52) * ss, (h / 2 + 28) * ss),
           "one terminal for coding agents — alone, or as a team",
           font=tag, fill=MUTED + (255,))

    img.resize((w, h), Image.LANCZOS).save(path)
    print(f"  → {path}  ({w}×{h})")


if __name__ == "__main__":
    out = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "img")
    os.makedirs(out, exist_ok=True)
    open(os.path.join(out, "logo.svg"), "w").write(svg_mark())
    print(f"  → {out}/logo.svg")
    open(os.path.join(out, "banner.svg"), "w").write(svg_banner())
    print(f"  → {out}/banner.svg")
    png_mark(os.path.join(out, "logo.png"))
    png_banner(os.path.join(out, "banner.png"))
