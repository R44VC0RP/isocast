#!/usr/bin/env python3
"""Draws packaging/linux/icons, the Linux app icon (hicolor PNGs at 16-512 px
and a scalable SVG), from the macOS icon's own design
(crates/isocast-app/bundle/Isocast.icon): its gradient fill, and the copper
dot with the bone and verdigris bars over it, scaled 1.25 as icon.json
places them. Linux has no Liquid Glass, so the layers are flat on a rounded
square, inset like macOS's (824 of 1024) so it sits with other apps' icons
in docks and menus. 16 and 24 px snap the bars to whole pixels (one-pixel
bars with a one-pixel gap) and use a one-pixel margin, so they stay legible.

    uv run --with pillow scripts/linux/make-icon.py

Run it again after changing the icon's design, and commit the icons.
"""
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "packaging/linux/icons/hicolor"
NAME = "dev.isocast.Isocast"
SIZES = [16, 24, 32, 48, 64, 128, 256, 512]
SS = 8  # supersampling

# icon.json: the fill's linear gradient (sRGB, top to bottom) and the layers.
TOP = (59, 66, 73)  # srgb 0.23137, 0.25882, 0.28627
BOTTOM = (34, 38, 42)  # srgb 0.13333, 0.14902, 0.16471
COPPER = (0xB4, 0x68, 0x3F)  # dot.svg
BONE = (0xEE, 0xEA, 0xE2)  # bars-inner.svg (and its dark fill)
VERDIGRIS = (0x3E, 0x8E, 0x7E)  # bars-outer.svg
SCALE = 1.25  # each layer's "position": {"scale": 1.25}
BODY = 824 / 1024  # macOS's icon grid: the shape inside its margin
RADIUS = 185 / 824  # its corner, of the shape's width
RIM = 36  # the edge's white, of 255


def unit(value):
    """A coordinate of the SVGs' 1024 box, scaled about the centre, as a
    fraction of the shape."""
    return (0.5 + (value / 1024 - 0.5) * SCALE)


def margin(size):
    """The transparent margin, in pixels: macOS's at 64 px and up, thinner
    below so small icons keep their detail."""
    return {16: 1, 24: 1, 32: 2, 48: 3}.get(size, round(size * (1 - BODY) / 2))


def geometry(size):
    """The shape's box, the dot and the two bars, in pixels."""
    m = margin(size)
    b = size - 2 * m
    px = lambda u: m + u * b  # noqa: E731
    dot = (px(unit(400)), px(unit(512)), 200 * SCALE / 1024 * b)
    bars = [(px(unit(300)), px(unit(top)), px(unit(820)), px(unit(bottom)))
            for top, bottom in ((426, 490), (534, 598))]
    if size <= 24:
        # Whole-pixel bars, one pixel apart, centred on the icon.
        thick = 1 if size == 16 else 2
        gap = 1 if size == 16 else 2
        top = (size - (2 * thick + gap)) // 2
        bars = [(round(x0), y, round(x1), y + thick)
                for (x0, _, x1, _), y in zip(bars, (top, top + thick + gap))]
    return m, b, dot, bars


def draw(size) -> Image.Image:
    n = size * SS
    m, b, (cx, cy, r), bars = geometry(size)
    s = lambda v: v * SS  # noqa: E731

    gradient = Image.new("RGB", (1, n))
    for y in range(n):
        t = min(max((y - s(m)) / max(s(b) - 1, 1), 0), 1)
        gradient.putpixel((0, y), tuple(round(a + (c - a) * t) for a, c in zip(TOP, BOTTOM)))
    image = gradient.resize((n, n)).convert("RGBA")

    dot = Image.new("L", (n, n), 0)
    ImageDraw.Draw(dot).ellipse((s(cx - r), s(cy - r), s(cx + r), s(cy + r)), fill=255)
    image.paste(Image.new("RGBA", (n, n), COPPER + (255,)), mask=dot)

    # Bone where the bars cross the dot, verdigris beyond it.
    mask = Image.new("L", (n, n), 0)
    d = ImageDraw.Draw(mask)
    for x0, y0, x1, y1 in bars:
        d.rounded_rectangle((s(x0), s(y0), s(x1) - 1, s(y1) - 1),
                            radius=s(y1 - y0) / 2, fill=255)
    image.paste(Image.new("RGBA", (n, n), BONE + (255,)), mask=ImageChops.multiply(mask, dot))
    image.paste(Image.new("RGBA", (n, n), VERDIGRIS + (255,)),
                mask=ImageChops.subtract(mask, dot))

    shape = Image.new("L", (n, n), 0)
    ImageDraw.Draw(shape).rounded_rectangle(
        (s(m), s(m), s(m + b) - 1, s(m + b) - 1), radius=s(b * RADIUS), fill=255)
    # A faint light rim, like macOS's glass edge, so the shape reads on dark
    # panels too (fainter below 48 px, where it is a whole pixel wide).
    w = max(SS, round(s(b / 128)))
    rim = Image.new("L", (n, n), 0)
    ImageDraw.Draw(rim).rounded_rectangle(
        (s(m) + w, s(m) + w, s(m + b) - 1 - w, s(m + b) - 1 - w),
        radius=s(b * RADIUS) - w, fill=255)
    rim = ImageChops.subtract(shape, rim).point(lambda v: v * RIM * min(size, 48) // (48 * 255))
    image.paste(Image.new("RGBA", (n, n), (255, 255, 255, 255)), mask=rim)
    image.putalpha(ImageChops.multiply(image.getchannel("A"), shape))
    return image.resize((size, size), Image.LANCZOS)


def svg():
    """The scalable icon, the same drawing at 1024 px."""
    m, b, (cx, cy, r), bars = geometry(1024)
    w = b / 128
    hex_ = lambda c: "#%02x%02x%02x" % c  # noqa: E731
    f = lambda v: f"{v:.2f}".rstrip("0").rstrip(".")  # noqa: E731
    inner = "".join(
        f'<rect x="{f(x0)}" y="{f(y0)}" width="{f(x1 - x0)}" height="{f(y1 - y0)}" '
        f'rx="{f((y1 - y0) / 2)}"/>' for x0, y0, x1, y1 in bars)
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="1024" height="1024">
  <defs>
    <linearGradient id="fill" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{hex_(TOP)}"/><stop offset="1" stop-color="{hex_(BOTTOM)}"/></linearGradient>
    <clipPath id="dot"><circle cx="{f(cx)}" cy="{f(cy)}" r="{f(r)}"/></clipPath>
  </defs>
  <rect x="{f(m)}" y="{f(m)}" width="{f(b)}" height="{f(b)}" rx="{f(b * RADIUS)}" fill="url(#fill)"/>
  <rect x="{f(m + w / 2)}" y="{f(m + w / 2)}" width="{f(b - w)}" height="{f(b - w)}" rx="{f(b * RADIUS - w / 2)}" fill="none" stroke="#fff" stroke-opacity="{f(RIM / 255)}" stroke-width="{f(w)}"/>
  <circle cx="{f(cx)}" cy="{f(cy)}" r="{f(r)}" fill="{hex_(COPPER)}"/>
  <g fill="{hex_(VERDIGRIS)}">{inner}</g>
  <g fill="{hex_(BONE)}" clip-path="url(#dot)">{inner}</g>
</svg>
"""


def main():
    for size in SIZES:
        path = OUT / f"{size}x{size}/apps/{NAME}.png"
        path.parent.mkdir(parents=True, exist_ok=True)
        draw(size).save(path, optimize=True)
    path = OUT / f"scalable/apps/{NAME}.svg"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(svg())
    print(f"{OUT.relative_to(ROOT)}: {', '.join(map(str, SIZES))} px and scalable")


if __name__ == "__main__":
    main()
