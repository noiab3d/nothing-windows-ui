"""Draws the MSI installer's images: our own dot-matrix art, in the app's style. Needs Pillow.

WiX sizes: banner 493x58 (top of the inner pages, text on its left), dialog 493x312 (welcome and
finish pages, text on the right, starting 164 px in).
"""
import math
import os

from PIL import Image, ImageDraw

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "src-tauri", "installer")
INK, PAPER, GRID, RED, DIM = (0, 0, 0), (255, 255, 255), (226, 226, 222), (215, 25, 33), (60, 60, 60)
SS = 4  # supersampling, for round dots

BUD = ["...#####...", ".#########.", "###########", "####ooo####", "###ooooo###", "###ooooo###", "####ooo####",
       ".#########.", "..########.", ".....####..", ".....####..", ".....####..", ".....####..", ".....####..",
       "......##..."]
# 5x7 capitals for the dot-matrix wordmark
FONT = {"N": ["#...#", "##..#", "#.#.#", "#.#.#", "#..##", "#...#", "#...#"],
        "O": [".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
        "T": ["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "..#.."],
        "H": ["#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
        "G": [".###.", "#...#", "#....", "#.###", "#...#", "#...#", ".###."],
        "U": ["#...#", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
        "I": ["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "#####"],
        " ": ["..", "..", "..", "..", "..", "..", ".."]}


def dot(d, x, y, r, fill):
    d.ellipse([(x - r) * SS, (y - r) * SS, (x + r) * SS, (y + r) * SS], fill=fill)


def art(d, rows, ox, oy, cell, r, fill, hole, mirror=False):
    w = len(rows[0])
    for y, row in enumerate(rows):
        for x, ch in enumerate(row):
            if ch != ".":
                cx = (w - 1 - x if mirror else x) * cell + cell / 2
                dot(d, ox + cx, oy + y * cell + cell / 2, r, fill if ch == "#" else hole)


def word(d, text, ox, oy, cell, r, fill):
    x = ox
    for ch in text:
        glyph = FONT[ch]
        for gy, row in enumerate(glyph):
            for gx, c in enumerate(row):
                if c == "#":
                    dot(d, x + gx * cell, oy + gy * cell, r, fill)
        x += (len(glyph[0]) + 1) * cell
    return x


def ring(d, cx, cy, radius, count, r, fill):
    for i in range(count):
        a = 2 * math.pi * i / count
        dot(d, cx + radius * math.cos(a), cy + radius * math.sin(a), r, fill)


def canvas(w, h):
    img = Image.new("RGB", (w * SS, h * SS), PAPER)
    return img, ImageDraw.Draw(img)


def grid(d, x0, x1, h, step=10):
    for y in range(step // 2, h, step):
        for x in range(x0 + step // 2, x1, step):
            dot(d, x, y, 0.6, GRID)


def finish(img, w, h, name):
    os.makedirs(OUT, exist_ok=True)
    img.resize((w, h), Image.LANCZOS).convert("RGB").save(os.path.join(OUT, name), "BMP")


def dialog():
    w, h = 493, 312
    img, d = canvas(w, h)
    grid(d, 164, w, h)
    d.rectangle([0, 0, 163 * SS, h * SS], fill=INK)  # black panel, left of the text
    word(d, "NOTHING", 12, 20, 3.4, 1.35, PAPER)
    word(d, "UI", 12, 50, 3.4, 1.35, PAPER)
    art(d, BUD, 26, 92, 5.4, 2.1, PAPER, DIM)
    art(d, BUD, 79, 126, 5.4, 2.1, PAPER, DIM, mirror=True)
    dot(d, 146, 284, 5, RED)
    ring(d, 146, 284, 12, 12, 1.2, DIM)
    finish(img, w, h, "dialog.bmp")


def banner():
    w, h = 493, 58
    img, d = canvas(w, h)
    grid(d, 300, w, h, step=8)
    ring(d, 458, 29, 17, 14, 1.6, INK)
    dot(d, 458, 29, 5.5, RED)
    finish(img, w, h, "banner.bmp")


dialog()
banner()
print("installer art written to", OUT)
