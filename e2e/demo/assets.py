#!/usr/bin/env python3
"""Draws the demo desktop's look: a grainy sunset wallpaper and a flat, rounded xfwm4 theme.

    python3 e2e/demo/assets.py <out dir> <width> <height>

writes <out dir>/wallpaper.png and the theme <out dir>/themes/tilt/xfwm4/. Needs Pillow and numpy.
"""
import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw

out = Path(sys.argv[1])
W, H = int(sys.argv[2]), int(sys.argv[3])


def hex_rgb(h):
    return np.array([int(h[i:i + 2], 16) for i in (1, 3, 5)], dtype=np.float32)


# Wallpaper: lavender sky, pink and orange in the middle, amber at the bottom, with two soft
# glows and a little grain so the gradient never bands.
stops = [(0.0, '#cdb4ff'), (0.28, '#e7a8e6'), (0.52, '#ff9a8a'), (0.74, '#ff8a4c'), (1.0, '#c9782a')]
y = np.linspace(0, 1, H, dtype=np.float32)[:, None]
x = np.linspace(0, 1, W, dtype=np.float32)[None, :]
# The bands lean a little, rising to the right.
t = np.clip(y + (x - 0.5) * -0.12, 0, 1)
img = np.zeros((H, W, 3), dtype=np.float32)
for (p0, c0), (p1, c1) in zip(stops, stops[1:]):
    m = (t >= p0) & (t <= p1)
    f = ((t - p0) / (p1 - p0))[..., None]
    f = f * f * (3 - 2 * f)
    img = np.where(m[..., None], hex_rgb(c0) * (1 - f) + hex_rgb(c1) * f, img)


def glow(cx, cy, r, color, strength):
    global img
    d = ((x - cx) ** 2 * (W / H) ** 2 + (y - cy) ** 2) / r ** 2
    a = (np.exp(-d) * strength)[..., None]
    img = img * (1 - a) + hex_rgb(color) * a


glow(0.15, 0.1, 0.45, '#b9a2ff', 0.55)
glow(0.85, 0.78, 0.5, '#ff7a3d', 0.35)
rng = np.random.default_rng(7)
img += rng.normal(0, 2.2, (H, W, 1)).astype(np.float32)
out.mkdir(parents=True, exist_ok=True)
Image.fromarray(np.clip(img, 0, 255).astype(np.uint8)).save(out / 'wallpaper.png')

# xfwm4 theme: the frame is the terminal's own colour, so a window reads as one dark card with
# rounded corners, a quiet title and round buttons on the right. xfwm4's compositor adds the shadow.
S = 2  # supersampling for smooth edges
BG = (22, 19, 34, 255)
BG_INACTIVE = (30, 27, 44, 255)
TITLE_H, R, SIDE, BOTTOM = 38, 12, 1, 12
theme = out / 'themes' / 'tilt' / 'xfwm4'
theme.mkdir(parents=True, exist_ok=True)


def piece(name, w, h, draw=None, fill=BG):
    for state, colour in (('active', fill), ('inactive', BG_INACTIVE if fill == BG else fill)):
        im = Image.new('RGBA', (w * S, h * S), colour)
        if draw:
            draw(im, colour)
        im.resize((w, h), Image.LANCZOS).save(theme / f'{name}-{state}.png')


def corner(left, top):
    def draw(im, colour):
        im.paste((0, 0, 0, 0), (0, 0, *im.size))
        d = ImageDraw.Draw(im)
        w, h = im.size
        r = R * S
        cx = r if left else w - r
        cy = r if top else h - r
        d.rectangle((0, 0, w, h), fill=colour)
        # Cut the outer corner away, then put back a quarter circle.
        box = (0 if left else w - r, 0 if top else h - r, r if left else w, r if top else h)
        d.rectangle(box, fill=(0, 0, 0, 0))
        d.ellipse((cx - r, cy - r, cx + r, cy + r), fill=colour)
    return draw


piece('top-left', R, TITLE_H, corner(True, True))
piece('top-right', R, TITLE_H, corner(False, True))
piece('bottom-left', R, BOTTOM, corner(True, False))
piece('bottom-right', R, BOTTOM, corner(False, False))
for i in range(1, 6):
    piece(f'title-{i}', 8, TITLE_H)
piece('left', SIDE, 8)
piece('right', SIDE, 8)
piece('bottom', 8, BOTTOM)

# Buttons: a soft circle with a thin glyph; prelight and pressed brighten it.
BW = 30


def button(name, glyph):
    for state, disc, ink in (('active', (255, 255, 255, 22), (214, 208, 232, 255)),
                             ('inactive', (255, 255, 255, 12), (140, 134, 160, 255)),
                             ('prelight', (255, 255, 255, 44), (255, 255, 255, 255)),
                             ('pressed', (255, 255, 255, 70), (255, 255, 255, 255))):
        im = Image.new('RGBA', (BW * S, TITLE_H * S), BG if state != 'inactive' else BG_INACTIVE)
        d = ImageDraw.Draw(im)
        cx, cy, r = BW * S / 2, TITLE_H * S / 2, 11 * S
        d.ellipse((cx - r, cy - r, cx + r, cy + r), fill=disc)
        glyph(d, cx, cy, ink)
        im.resize((BW, TITLE_H), Image.LANCZOS).save(theme / f'{name}-{state}.png')


def g_close(d, cx, cy, ink):
    k = 4 * S
    d.line((cx - k, cy - k, cx + k, cy + k), fill=ink, width=int(1.6 * S))
    d.line((cx - k, cy + k, cx + k, cy - k), fill=ink, width=int(1.6 * S))


def g_hide(d, cx, cy, ink):
    d.line((cx - 4.5 * S, cy, cx + 4.5 * S, cy), fill=ink, width=int(1.6 * S))


def g_max(d, cx, cy, ink):
    k = 4 * S
    d.rounded_rectangle((cx - k, cy - k, cx + k, cy + k), radius=1.5 * S, outline=ink, width=int(1.6 * S))


button('close', g_close)
button('hide', g_hide)
button('maximize', g_max)
button('maximize-toggled', g_max)

(theme / 'themerc').write_text('\n'.join([
    'active_text_color=#e9e5f7',
    'inactive_text_color=#8c86a0',
    'button_offset=8',
    'button_spacing=2',
    'full_width_title=true',
    'title_alignment=center',
    'title_shadow_active=false',
    'title_shadow_inactive=false',
    'title_vertical_offset_active=0',
    'title_vertical_offset_inactive=0',
    'show_app_icon=false',
    'maximized_offset=0',
    '',
]))
