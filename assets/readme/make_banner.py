#!/usr/bin/env python3
"""Draws the README banner: the disc mark beside the wordmark, on transparency.

The mark is rasterised from the same geometry the app uses, ported from `src/mark.rs`, so
the banner and the running program cannot drift apart. It is not traced or redrawn.

No background, because a forge shows a README on whichever theme the reader picked and a
banner with a dark plate baked in looks like a sticker on a light page.

That rules out the app's near-white wordmark, which would vanish on white. Rescuing it with
a heavy halo was tried and looked like a smudge, so the wordmark takes the mark's own blue
instead: strong on paper, and the app's colour on a dark page. "INSTALLER" sits in a neutral
grey chosen to clear both backgrounds rather than one. The shadow that remains is tight and
faint, doing depth rather than legibility, which is the only job a shadow does well.
"""

import math
import os

from PIL import Image, ImageDraw, ImageFilter, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, 'banner.png')

FONTS = '/usr/share/fonts/noto/'
MEDIUM = FONTS + 'NotoSans-Medium.ttf'
REGULAR = FONTS + 'NotoSans-Regular.ttf'

# src/mark.rs, DISPLAY.
GEOM = dict(r_in=0.62, r_disc=0.42, cut_a=0.78, cut_b=0.88, cut_mid=0.16)
PAD = 0.93
SS = 4
FRAC_1_SQRT_2 = 1.0 / math.sqrt(2.0)

# theme.rs
ACCENT_HOVER = (0x3B, 0x82, 0xF6)
# Not theme.rs's TEXT and TEXT_DIM: those assume the app's dark panel behind them.
WORDMARK = ACCENT_HOVER
SUBTITLE = (0x7C, 0x86, 0x95)

S = 3                                   # supersample, then shrink
W, H = 1180 * S, 210 * S

SHADOW_BLUR = 4 * S
SHADOW_ALPHA = 0.45
SHADOW_DROP = 2 * S


def inside(x, y, g):
    r2 = x * x + y * y
    if r2 <= g['r_disc'] ** 2:
        return True
    if r2 > 1.0 or r2 < g['r_in'] ** 2:
        return False
    t = abs((x + y) * FRAC_1_SQRT_2)
    return not (t <= g['cut_mid'] or (g['cut_a'] <= t <= g['cut_b']))


def mark(side, colour):
    centre = side * 0.5
    radius = centre * PAD
    step = 1.0 / SS
    px = []
    for y in range(side):
        for x in range(side):
            hits = 0
            for sy in range(SS):
                cy = y + (sy + 0.5) * step
                for sx in range(SS):
                    cx = x + (sx + 0.5) * step
                    if inside((cx - centre) / radius, (cy - centre) / radius, GEOM):
                        hits += 1
            px.append((*colour, hits * 255 // (SS * SS)))
    im = Image.new('RGBA', (side, side))
    im.putdata(px)
    return im


def tracked(d, xy, text, font, fill, track):
    """Letter tracking, which PIL has no setting for and the app does by hand too."""
    x, y = xy
    for ch in text:
        d.text((x, y), ch, font=font, fill=fill)
        x += d.textbbox((0, 0), ch, font=font)[2] + track
    return x


def width_of(d, text, font, track):
    return sum(d.textbbox((0, 0), c, font=font)[2] + track for c in text) - track


def build():
    layer = Image.new('RGBA', (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)

    side = 116 * S
    gap = 40 * S
    cap_font = ImageFont.truetype(MEDIUM, 46 * S)
    sub_font = ImageFont.truetype(REGULAR, 21 * S)

    cap_w = width_of(d, 'ECHO VRCE', cap_font, 5 * S)
    total = side + gap + cap_w
    mx = (W - total) // 2
    my = (H - side) // 2

    disc = mark(side, ACCENT_HOVER)
    layer.alpha_composite(disc, (mx, my))

    tx = mx + side + gap
    tracked(d, (tx, 70 * S), 'ECHO VRCE', cap_font, WORDMARK, 5 * S)
    tracked(d, (tx + 3 * S, 129 * S), 'INSTALLER', sub_font, SUBTITLE, 8 * S)

    # The lockup's own silhouette, blurred, dimmed and dropped a couple of pixels.
    shadow = Image.new('RGBA', (W, H), (0, 0, 0, 0))
    mask = (layer.getchannel('A')
            .filter(ImageFilter.GaussianBlur(SHADOW_BLUR))
            .point(lambda a: int(a * SHADOW_ALPHA)))
    shadow.paste(Image.new('RGBA', (W, H), (0, 0, 0, 255)), (0, SHADOW_DROP), mask)

    out = Image.alpha_composite(shadow, layer)
    return out.resize((W // S, H // S), Image.LANCZOS)


if __name__ == '__main__':
    img = build()
    img.save(OUT, optimize=True)
    print('%s  %sx%s  %s  %.1f KB'
          % (OUT, img.size[0], img.size[1], img.mode, os.path.getsize(OUT) / 1024))
