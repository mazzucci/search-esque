#!/usr/bin/env python3
"""Assembles the frames dev/record_demo.mjs saved into the README's GIF.

    python3 dev/make_gif.py <frames dir> media/playground.gif [width]

One palette for the whole animation (so frames differ only where the page changed, which keeps
the file small), each frame held as long as the recorder said. Needs Pillow.
"""
import json
import sys

from PIL import Image

src, out = sys.argv[1], sys.argv[2]
width = int(sys.argv[3]) if len(sys.argv) > 3 else None
plan = json.load(open(f"{src}/frames.json"))

frames = []
for f in plan:
    im = Image.open(f"{src}/{f['file']}").convert("RGB")
    if width and im.width != width:
        im = im.resize((width, round(im.height * width / im.width)), Image.LANCZOS)
    frames.append(im)

# a palette from a sample of every frame, applied to all of them
strip = Image.new("RGB", (frames[0].width, frames[0].height * min(len(frames), 12)))
step = max(1, len(frames) // 12)
for i, im in enumerate(frames[::step][:12]):
    strip.paste(im, (0, i * frames[0].height))
palette = strip.quantize(colors=255, method=Image.Quantize.MEDIANCUT)
quantized = [im.quantize(palette=palette, dither=Image.Dither.NONE) for im in frames]

quantized[0].save(
    out, save_all=True, append_images=quantized[1:], loop=0, optimize=True,
    duration=[max(20, f["hold"]) for f in plan], disposal=1,
)
import os
print(f"{out}: {len(frames)} frames, {frames[0].width}x{frames[0].height}, {os.path.getsize(out) / 1e6:.2f} MB")
