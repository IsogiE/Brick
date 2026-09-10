"""Regenerate Brick's local digit templates from ART's PT Sans Narrow font.
Usage: python scripts/generate-replay-font.py /path/to/ART/Media/Fonts/PTSansNarrow.ttf
Requires Pillow; development tool only, never invoked by the app.
"""
import json
from pathlib import Path
import sys
from PIL import ImageFont

fonts = []
for size in range(6, 33):
    font = ImageFont.truetype(sys.argv[1], size)
    glyphs = []
    for digit in '0123456789':
        phases = []
        for phase in range(4):
            mask, offset = font.getmask2(digit, mode='L', anchor='ls', start=(phase / 4, 0))
            phases.append(dict(width=mask.size[0], height=mask.size[1], left=offset[0], top=offset[1], pixels=list(mask)))
        glyphs.append(dict(advance=font.getlength(digit), phases=phases))
    fonts.append(dict(glyphs=glyphs))
output = Path(__file__).resolve().parent.parent / 'src/assets/replay-digits.json'
output.write_text(json.dumps(fonts, separators=(',', ':')) + '\n')
