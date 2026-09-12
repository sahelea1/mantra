#!/usr/bin/env python3
"""Render real Mantra frames to PNG.

Mantra is a TUI, so its screenshots have to come from a real terminal: this drives the
binary inside tmux at a fixed size, waits for the frame you want, captures it *with*
its colours (`tmux capture-pane -e`), and paints the resulting character grid into a
PNG with a small window chrome.

Used by `docs/tools/shoot.py`; see that file for the scenes. Needs tmux, Pillow and
DejaVu fonts:

    pip install Pillow fonttools

Four glyphs Mantra uses (⏰ ⏱ ⏳ ⛔) have no usable outline in the installed fonts, so
they are drawn by hand in `_draw_special`; everything else comes from DejaVu Sans Mono,
falling back to DejaVu Sans (braille, technical symbols) and FreeSans.
"""

from __future__ import annotations

import os
import re
import subprocess
import time

from PIL import Image, ImageDraw, ImageFont

# ── palette (mirrors mantra_src/src/ui/theme.rs) ────────────────────────────────
BG = (13, 15, 20)
CHROME = (23, 26, 33)
CHROME_LINE = (38, 42, 52)
DEFAULT_FG = (226, 230, 236)
DOTS = [(242, 104, 138), (232, 197, 71), (110, 214, 138)]  # rose / amber / green

ANSI16 = [
    (32, 36, 44), (240, 90, 90), (110, 214, 138), (232, 197, 71),
    (110, 150, 255), (200, 130, 250), (60, 207, 180), (200, 205, 214),
    (80, 86, 100), (255, 130, 130), (150, 235, 170), (245, 215, 120),
    (150, 180, 255), (220, 170, 255), (120, 230, 210), (240, 244, 250),
]

MONO = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
MONO_BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"
SANS = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
SANS_BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"
FREE = "/usr/share/fonts/truetype/freefont/FreeSans.ttf"
FREE_BOLD = "/usr/share/fonts/truetype/freefont/FreeSansBold.ttf"

SPECIAL = {"⏰", "⏱", "⏳", "⛔"}  # no installed font draws these — see _draw_special

SGR = re.compile(r"\x1b\[([0-9;:]*)m")
CSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
OSC = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")


# ── driving a real terminal ─────────────────────────────────────────────────────
class Term:
    """A tmux session running one command at a fixed character size."""

    def __init__(self, cmd: str, cols: int = 120, rows: int = 34, env: dict | None = None, name: str = "mantrashot"):
        self.name, self.cols, self.rows = name, cols, rows
        self.kill()
        prefix = " ".join(f"{k}={_sh(v)}" for k, v in (env or {}).items())
        subprocess.run(
            ["tmux", "-f", "/dev/null", "new-session", "-d", "-s", name,
             "-x", str(cols), "-y", str(rows), f"env {prefix} {cmd}"],
            check=True,
        )
        # Truecolor, and no "escape-time is 500ms" warning in the captures.
        for opt in (["-s", "escape-time", "10"],
                    ["-g", "default-terminal", "tmux-256color"],
                    ["-as", "terminal-features", ",*:RGB"]):
            subprocess.run(["tmux", "set"] + opt, check=False)

    def kill(self):
        subprocess.run(["tmux", "kill-session", "-t", self.name], check=False,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def text(self) -> str:
        out = subprocess.run(["tmux", "capture-pane", "-p", "-t", self.name],
                             capture_output=True, text=True)
        return out.stdout

    def ansi(self) -> str:
        out = subprocess.run(["tmux", "capture-pane", "-p", "-e", "-t", self.name],
                             capture_output=True, text=True)
        return out.stdout

    def wait(self, needle: str, timeout: float = 90.0) -> bool:
        """Block until `needle` shows up on screen. Returns False on timeout."""
        end = time.time() + timeout
        while time.time() < end:
            if needle in self.text():
                return True
            time.sleep(0.25)
        print(f"  !! timeout waiting for {needle!r}")
        return False

    def send(self, *keys: str):
        for k in keys:
            subprocess.run(["tmux", "send-keys", "-t", self.name, k], check=True)
            time.sleep(0.12)

    def type(self, text: str):
        subprocess.run(["tmux", "send-keys", "-t", self.name, "-l", text], check=True)
        time.sleep(0.15)

    def sleep(self, s: float):
        time.sleep(s)


def _sh(v) -> str:
    return "'" + str(v).replace("'", "'\\''") + "'"


# ── ANSI → cell grid ────────────────────────────────────────────────────────────
class Cell:
    __slots__ = ("ch", "fg", "bold")

    def __init__(self, ch=" ", fg=DEFAULT_FG, bold=False):
        self.ch, self.fg, self.bold = ch, fg, bold


def parse(ansi: str, cols: int, rows: int) -> list[list[Cell]]:
    grid = [[Cell() for _ in range(cols)] for _ in range(rows)]
    for y, line in enumerate(ansi.split("\n")[:rows]):
        line = OSC.sub("", line)
        fg, bold, x = DEFAULT_FG, False, 0
        pos = 0
        while pos < len(line) and x < cols:
            m = SGR.match(line, pos)
            if m:
                fg, bold = _sgr(m.group(1), fg, bold)
                pos = m.end()
                continue
            m = CSI.match(line, pos)
            if m:  # any other control sequence: skip, it doesn't paint a cell
                pos = m.end()
                continue
            ch = line[pos]
            pos += 1
            if ch == "\x1b":
                continue
            grid[y][x] = Cell(ch, fg, bold)
            x += 1
    return grid


def _sgr(params: str, fg, bold):
    parts = [p for p in params.replace(":", ";").split(";")]
    i = 0
    while i < len(parts):
        p = parts[i] or "0"
        n = int(p) if p.isdigit() else 0
        if n == 0:
            fg, bold = DEFAULT_FG, False
        elif n == 1:
            bold = True
        elif n == 22:
            bold = False
        elif n == 39:
            fg = DEFAULT_FG
        elif 30 <= n <= 37:
            fg = ANSI16[n - 30]
        elif 90 <= n <= 97:
            fg = ANSI16[n - 90 + 8]
        elif n == 38 and i + 1 < len(parts):
            mode = int(parts[i + 1] or 0)
            if mode == 2 and i + 4 < len(parts):
                fg = tuple(int(parts[i + 2 + k] or 0) for k in range(3))
                i += 4
            elif mode == 5 and i + 2 < len(parts):
                fg = _xterm256(int(parts[i + 2] or 0))
                i += 2
        i += 1
    return fg, bold


def _xterm256(n: int):
    if n < 16:
        return ANSI16[n]
    if n < 232:
        n -= 16
        lv = [0, 95, 135, 175, 215, 255]
        return (lv[n // 36], lv[(n // 6) % 6], lv[n % 6])
    v = 8 + (n - 232) * 10
    return (v, v, v)


# ── painting ────────────────────────────────────────────────────────────────────
class Painter:
    def __init__(self, size: int = 26):
        self.size = size
        self.mono = ImageFont.truetype(MONO, size)
        self.mono_b = ImageFont.truetype(MONO_BOLD, size)
        self.sans = ImageFont.truetype(SANS, size)
        self.sans_b = ImageFont.truetype(SANS_BOLD, size)
        self.free = ImageFont.truetype(FREE, size)
        self.free_b = ImageFont.truetype(FREE_BOLD, size)
        self.cw = round(self.mono.getlength("M"))
        self.ch = round(size * 1.30)
        # Mantra's glyph set needs all three: DejaVu Sans Mono for text and most box drawing,
        # DejaVu Sans for braille and a few technical symbols, FreeSans for ⛔.
        self._mono_chars = _coverage(MONO)
        self._sans_chars = _coverage(SANS)
        self._free_chars = _coverage(FREE)
        self.missing: set[str] = set()

    def font_for(self, ch: str, bold: bool):
        cp = ord(ch)
        if cp < 128 or cp in self._mono_chars:
            return self.mono_b if bold else self.mono
        if cp in self._sans_chars:
            return self.sans_b if bold else self.sans
        if cp in self._free_chars:
            return self.free_b if bold else self.free
        self.missing.add(ch)
        return self.sans_b if bold else self.sans

    def render(self, grid, title: str = "mantra", pad: int = 22) -> Image.Image:
        rows, cols = len(grid), len(grid[0])
        bar = round(self.size * 1.55)
        w = cols * self.cw + pad * 2
        h = rows * self.ch + pad * 2 + bar
        img = Image.new("RGB", (w, h), BG)
        d = ImageDraw.Draw(img)

        # window chrome: title bar, traffic lights, hairline
        d.rectangle([0, 0, w, bar], fill=CHROME)
        d.line([(0, bar), (w, bar)], fill=CHROME_LINE, width=1)
        r = max(4, self.size // 5)
        cx, cy = pad, bar // 2
        for i, col in enumerate(DOTS):
            x = cx + i * (r * 3)
            d.ellipse([x - r, cy - r, x + r, cy + r], fill=col)
        tf = ImageFont.truetype(SANS, round(self.size * 0.72))
        tw = d.textlength(title, font=tf)
        d.text(((w - tw) / 2, cy - self.size * 0.40), title, font=tf, fill=(140, 146, 158))

        # the character grid
        y0 = bar + pad
        for y, row in enumerate(grid):
            for x, cell in enumerate(row):
                if cell.ch in (" ", " ", ""):
                    continue
                px, py = pad + x * self.cw, y0 + y * self.ch
                if cell.ch in SPECIAL:
                    _draw_special(d, cell.ch, px, py, self.cw, self.ch, cell.fg)
                    continue
                f = self.font_for(cell.ch, cell.bold)
                # centre non-mono fallbacks inside the cell so the grid stays aligned
                adv = d.textlength(cell.ch, font=f)
                off = (self.cw - adv) / 2 if abs(adv - self.cw) > 1 else 0
                d.text((px + off, py), cell.ch, font=f, fill=cell.fg)
        if self.missing:
            print(f"  !! no font covers: {' '.join(sorted(self.missing))} "
                  f"({', '.join(hex(ord(c)) for c in sorted(self.missing))})")
        return img


def _coverage(path: str) -> set[int]:
    from fontTools.ttLib import TTFont
    t = TTFont(path, fontNumber=0)
    out: set[int] = set()
    for tb in t["cmap"].tables:
        out |= set(tb.cmap.keys())
    return out


def _draw_special(d: ImageDraw.ImageDraw, ch: str, x: int, y: int, cw: int, chh: int, fg):
    """⏰ ⏱ ⏳ — in no installed font, so draw them at cell size."""
    m = max(1, cw // 12)
    box = [x + m, y + chh * 0.18, x + cw - m, y + chh * 0.18 + (cw - 2 * m)]
    if ch in ("⏰", "⏱"):  # alarm clock / stopwatch
        d.ellipse(box, outline=fg, width=max(1, cw // 9))
        cx, cy = (box[0] + box[2]) / 2, (box[1] + box[3]) / 2
        rr = (box[2] - box[0]) / 2
        d.line([cx, cy, cx, cy - rr * 0.55], fill=fg, width=max(1, cw // 10))
        d.line([cx, cy, cx + rr * 0.45, cy], fill=fg, width=max(1, cw // 10))
        if ch == "⏱":  # stopwatch crown
            d.line([cx - rr * 0.3, box[1] - m, cx + rr * 0.3, box[1] - m], fill=fg, width=max(1, cw // 8))
    elif ch == "⛔":  # halt marker: filled disc with a bar through it
        d.ellipse(box, fill=fg)
        cy = (box[1] + box[3]) / 2
        bar = max(2, (box[3] - box[1]) / 5)
        d.rectangle([box[0] + m, cy - bar / 2, box[2] - m, cy + bar / 2], fill=BG)
    else:  # ⏳ hourglass
        x0, x1 = box[0], box[2]
        y0, y1 = box[1], box[3]
        w = max(1, cw // 10)
        d.line([x0, y0, x1, y0], fill=fg, width=w)
        d.line([x0, y1, x1, y1], fill=fg, width=w)
        d.line([x0, y0, x1, y1], fill=fg, width=w)
        d.line([x1, y0, x0, y1], fill=fg, width=w)


def shoot(term: Term, out: str, title: str, size: int = 26, crop_rows: int | None = None):
    """Capture the current frame and write `out` as a PNG."""
    grid = parse(term.ansi(), term.cols, term.rows)
    if crop_rows:
        grid = grid[:crop_rows]
    img = Painter(size).render(grid, title=title)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    img.save(out, optimize=True)
    print(f"  → {out}  ({img.width}×{img.height})")
    return out
