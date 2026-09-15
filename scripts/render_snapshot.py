"""Render a TestBackend JSON export. Optional QA helper; requires Pillow."""
import argparse
import json
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont

parser = argparse.ArgumentParser()
parser.add_argument("input", type=Path)
parser.add_argument("output", type=Path)
parser.add_argument("--font", default="C:/Windows/Fonts/0xProtoNerdFontMono-Regular.ttf")
args = parser.parse_args()
data = json.loads(args.input.read_text(encoding="utf-8"))
cw, ch = 11, 23
font = ImageFont.truetype(args.font, 18)
canvas = Image.new("RGB", (data["width"] * cw, data["height"] * ch), "#0d1526")
draw = ImageDraw.Draw(canvas)
for x, y, text, fg, bg, bold in data["cells"]:
    left, top = x * cw, y * ch
    draw.rectangle((left, top, left + cw - 1, top + ch - 1), fill=bg)
for x, y, text, fg, bg, bold in data["cells"]:
    left, top = x * cw, y * ch
    if text == "▀":
        draw.rectangle((left, top, left + cw - 1, top + ch // 2), fill=fg)
    elif text == "▄":
        draw.rectangle((left, top + ch // 2, left + cw - 1, top + ch - 1), fill=fg)
    elif text == "█":
        draw.rectangle((left, top, left + cw - 1, top + ch - 1), fill=fg)
    elif text == "":
        draw.polygon([(left, top), (left + cw - 1, top + ch // 2), (left, top + ch - 1)], fill=fg)
    elif text.strip():
        draw.text((left, top + 1), text, font=font, fill=fg, stroke_width=0)
args.output.parent.mkdir(parents=True, exist_ok=True)
canvas.save(args.output)
print(args.output.resolve())
