#!/usr/bin/env python3
"""Draws crates/isocast-ui/windows/isocast.ico, the Windows app icon, from the
macOS icon's own design (crates/isocast-app/bundle/Isocast.icon): its
gradient fill, and the copper dot with the bone and verdigris bars over it,
scaled 1.25 as icon.json places them. Windows has no Liquid Glass, so the
layers are flat on a rounded square.

    python3 scripts/windows/make-icon.py      (needs Pillow)

Run it again after changing the icon's design, and commit the .ico.
"""
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "crates/isocast-ui/windows/isocast.ico"
SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
SS = 4  # supersampling
N = 1024 * SS

# icon.json: the fill's linear gradient (sRGB, top to bottom) and the layers.
TOP = (59, 66, 73)  # srgb 0.23137, 0.25882, 0.28627
BOTTOM = (34, 38, 42)  # srgb 0.13333, 0.14902, 0.16471
COPPER = (0xB4, 0x68, 0x3F)  # dot.svg
BONE = (0xEE, 0xEA, 0xE2)  # bars-inner.svg (and its dark fill)
VERDIGRIS = (0x3E, 0x8E, 0x7E)  # bars-outer.svg
SCALE = 1.25  # each layer's "position": {"scale": 1.25}


def at(value):
    """A coordinate of the SVGs' 1024 box, scaled about the centre."""
    return (512 + (value - 512) * SCALE) * SS


def draw() -> Image.Image:
    gradient = Image.new("RGB", (1, N))
    for y in range(N):
        t = y / (N - 1)
        gradient.putpixel((0, y), tuple(round(a + (b - a) * t) for a, b in zip(TOP, BOTTOM)))
    image = gradient.resize((N, N)).convert("RGBA")

    # The dot: circle cx 400, cy 512, r 200.
    dot = Image.new("L", (N, N), 0)
    r = 200 * SCALE * SS
    ImageDraw.Draw(dot).ellipse((at(400) - r, at(512) - r, at(400) + r, at(512) + r), fill=255)
    image.paste(Image.new("RGBA", (N, N), COPPER + (255,)), mask=dot)

    # The bars: y 426-490 and 534-598, round ends at x 332 and 788 (r 32);
    # bone where they cross the dot, verdigris beyond it.
    bars = Image.new("L", (N, N), 0)
    d = ImageDraw.Draw(bars)
    for top, bottom in ((426, 490), (534, 598)):
        d.rounded_rectangle((at(300), at(top), at(820), at(bottom)), radius=32 * SCALE * SS, fill=255)
    image.paste(Image.new("RGBA", (N, N), BONE + (255,)), mask=ImageChops.multiply(bars, dot))
    image.paste(Image.new("RGBA", (N, N), VERDIGRIS + (255,)),
                mask=ImageChops.subtract(bars, dot))

    # Windows' shape: a rounded square, full bleed.
    shape = Image.new("L", (N, N), 0)
    ImageDraw.Draw(shape).rounded_rectangle((0, 0, N - 1, N - 1), radius=round(N * 0.18), fill=255)
    image.putalpha(shape)
    return image


def main():
    big = draw()
    frames = [big.resize((s, s), Image.LANCZOS) for s in SIZES]
    OUT.parent.mkdir(parents=True, exist_ok=True)
    frames[-1].save(OUT, format="ICO", sizes=[(s, s) for s in SIZES], append_images=frames[:-1])
    print(f"{OUT.relative_to(ROOT)}: {', '.join(map(str, SIZES))} px")


if __name__ == "__main__":
    main()
