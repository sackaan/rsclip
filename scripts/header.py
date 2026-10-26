"""Render docs/header-{dark,light}.png: clapboard logo + title in the app font. Needs Pillow.

    python3 scripts/header.py
"""
import pathlib
from PIL import Image, ImageDraw, ImageFont
root = pathlib.Path(__file__).resolve().parent.parent
# Rendered at its final size with antialiasing, like the app's 40 pt title;
# the outlines break up when a small rasterization is scaled up.
FONT_PX, LOGO_H, GAP, PAD = 190, 220, 44, 8
font = ImageFont.truetype(f"{root}/src/assets/runescape_large.ttf", FONT_PX)
icon = Image.open(f"{root}/src/assets/icon.png").convert("RGBA")
icon = icon.crop(icon.getbbox())
icon = icon.resize((round(icon.width * LOGO_H / icon.height), LOGO_H), Image.LANCZOS)

def title(color):
    l, t, r, b = font.getbbox("RSClip")
    img = Image.new("RGBA", (r - l + 4, b - t + 4), (0, 0, 0, 0))
    ImageDraw.Draw(img).text((2 - l, 2 - t), "RSClip", font=font, fill=color)
    return img.crop(img.getbbox())

for name, color in (("dark", (255, 255, 255, 255)), ("light", (17, 17, 17, 255))):
    t = title(color)
    w = PAD + icon.width + GAP + t.width + PAD
    h = PAD * 2 + max(icon.height, t.height)
    out = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    out.alpha_composite(icon, (PAD, (h - icon.height) // 2))
    out.alpha_composite(t, (PAD + icon.width + GAP, (h - t.height) // 2 + 6))
    out.save(f"{root}/docs/header-{name}.png", optimize=True)
    print(name, out.size)
