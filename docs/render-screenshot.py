#!/usr/bin/env python3
"""Render the README screenshot from the app's own screen.

The `screenshot` test in src/tui.rs draws the app with ratatui's test backend
and writes every cell (symbol, color, dim, bold) to JSON; this turns that into
a PNG in a terminal window:

    SCREENSHOT_JSON=/tmp/screen.json cargo test screenshot -- --ignored
    python3 docs/render-screenshot.py /tmp/screen.json docs/tui.png

Needs Pillow. Text is set in Menlo; the two symbols it lacks (⏺ and ⎿) are
drawn as shapes, the way a terminal's fallback font shows them.
"""

import json
import sys

from PIL import Image, ImageDraw, ImageFont

SCALE = 2
FONT_SIZE = 13 * SCALE
FONT = "/System/Library/Fonts/Menlo.ttc"
BACKGROUND = (24, 24, 27)
FOREGROUND = (212, 212, 216)
NAMED = {
    "White": (250, 250, 250),
    "Green": (74, 222, 128),
    "Red": (248, 113, 113),
    "Yellow": (250, 204, 21),
    "Blue": (96, 165, 250),
    "Cyan": (34, 211, 238),
    "Magenta": (232, 121, 249),
    "DarkGray": (82, 82, 91),
}


def color(cell):
    fg = cell["fg"]
    if fg.startswith("#"):
        rgb = tuple(int(fg[i : i + 2], 16) for i in (1, 3, 5))
    else:
        rgb = NAMED.get(fg, FOREGROUND)
    if cell["dim"]:
        # Halfway to the background, as terminals draw faint text.
        rgb = tuple((c + b) // 2 for c, b in zip(rgb, BACKGROUND))
    return rgb


def main(source, target):
    rows = json.load(open(source))
    regular = ImageFont.truetype(FONT, FONT_SIZE, index=0)
    bold = ImageFont.truetype(FONT, FONT_SIZE, index=1)
    cell_w = regular.getlength("M")
    cell_h = int(FONT_SIZE * 1.3)
    pad, bar = 20 * SCALE, 28 * SCALE
    width = int(cell_w * len(rows[0])) + 2 * pad
    height = cell_h * len(rows) + 2 * pad + bar

    image = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    draw.rounded_rectangle((0, 0, width - 1, height - 1), radius=10 * SCALE, fill=BACKGROUND)
    # Window buttons.
    for i, rgb in enumerate([(255, 95, 86), (255, 189, 46), (39, 201, 63)]):
        x, y = pad + i * 20 * SCALE, bar // 2 + 4 * SCALE
        r = 6 * SCALE
        draw.ellipse((x - r, y - r, x + r, y + r), fill=rgb)

    top = pad + bar
    for y, row in enumerate(rows):
        for x, cell in enumerate(row):
            symbol = cell["s"]
            if symbol in (" ", ""):
                continue
            left, upper = pad + x * cell_w, top + y * cell_h
            rgb = color(cell)
            if symbol == "⏺":
                r = cell_w * 0.32
                cx, cy = left + cell_w / 2, upper + cell_h / 2
                draw.ellipse((cx - r, cy - r, cx + r, cy + r), fill=rgb)
            elif symbol == "⎿":
                cx, cy = left + cell_w * 0.35, upper + cell_h * 0.55
                w = max(1, SCALE)
                draw.line((cx, upper, cx, cy), fill=rgb, width=w)
                draw.line((cx, cy, left + cell_w, cy), fill=rgb, width=w)
            elif symbol in "─▀▄█":
                # Block and rule characters fill their cell edge to edge.
                if symbol == "─":
                    mid = upper + cell_h // 2
                    draw.line((left, mid, left + cell_w + 1, mid), fill=rgb, width=max(1, SCALE))
                else:
                    y0 = upper if symbol in "▀█" else upper + cell_h / 2
                    y1 = upper + cell_h if symbol in "▄█" else upper + cell_h / 2
                    draw.rectangle((left, y0, left + cell_w + 0.5, y1), fill=rgb)
            else:
                font = bold if cell["bold"] else regular
                draw.text((left, upper + (cell_h - FONT_SIZE) / 2), symbol, font=font, fill=rgb)

    image.save(target)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
