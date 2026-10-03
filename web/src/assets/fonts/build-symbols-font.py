#!/usr/bin/env python3
"""Rebuild web/src/assets/fonts/OpenRelayTerminalSymbols.woff2.

Why this exists
---------------
JetBrains Mono (the bundled terminal face) has no glyphs for the ranges tools
actually print: Misc Technical (U+23F5), Braille spinners (U+2819), dingbats
(U+2713) and geometric shapes (U+25CB). Without help those characters fall
through to whatever the OS has: Windows font-links Segoe UI Symbol and renders
them proportionally, while phones have nothing and draw tofu.

Nerd Fonts does not help - its builds add icon glyphs, not these blocks. The
font that does own them is Noto Sans Symbols 2, but it is proportional. So we
take a subset of it and make it monospaced: every glyph is scaled into the
JetBrains Mono cell (600/1000 em) and given that advance, giving one symbol per
cell on every platform.

Usage
-----
    pip install --user fonttools brotli
    curl -L -o NotoSansSymbols2-Regular.ttf \
      https://raw.githubusercontent.com/google/fonts/main/ofl/notosanssymbols2/NotoSansSymbols2-Regular.ttf
    python web/src/assets/fonts/build-symbols-font.py \
      NotoSansSymbols2-Regular.ttf web/src/assets/fonts/OpenRelayTerminalSymbols.woff2

The unicode-range declared in web/src/components/XTerm.css must be kept in step
with RANGES below (it is currently omitted - see the comment there).
Licence: OFL, see web/src/assets/fonts/NotoSansSymbols2-OFL.txt.
"""

import sys

from fontTools import subset

# JetBrains Mono's advance for 'M', in font units (unitsPerEm 1000).
CELL = 600

# The blocks this face claims. Anything outside falls through to the system
# stack declared in XTerm.tsx - notably U+23BF, which no bundled font carries.
RANGES = [
    (0x2000, 0x206F),  # General Punctuation
    (0x2190, 0x21FF),  # Arrows
    (0x2300, 0x23FF),  # Miscellaneous Technical
    (0x2500, 0x25FF),  # Box Drawing + Geometric Shapes
    (0x2700, 0x27BF),  # Dingbats
    (0x2800, 0x28FF),  # Braille Patterns
    (0x2B00, 0x2BFF),  # Misc Symbols and Arrows
]


def wanted():
    points = set()
    for start, end in RANGES:
        points.update(range(start, end + 1))
    return points


def subset_font(src):
    options = subset.Options()
    options.flavor = "woff2"
    options.desubroutinize = True
    options.layout_features = []
    options.name_IDs = [1, 2, 3, 4, 6]
    options.notdef_outline = True
    # recalc_bounds trips over plain-list coordinates in recent fontTools;
    # make_monospaced() recomputes the boxes it cares about itself.
    options.recalc_bounds = False

    font = subset.load_font(src, options)
    subsetter = subset.Subsetter(options=options)
    subsetter.populate(unicodes=wanted())
    subsetter.subset(font)
    return font


def make_monospaced(font):
    """Scale glyph outlines into one cell and give them all the cell advance."""
    glyf = font["glyf"]
    hmtx = font["hmtx"]
    scaled = 0
    composites = 0
    for name in font.getGlyphOrder():
        glyph = glyf[name]
        # 0 = empty, negative = composite (no outline coordinates to scale).
        if glyph.numberOfContours < 0:
            composites += 1
        elif glyph.numberOfContours > 0:
            raw = glyph.getCoordinates(glyf)
            # Recent fontTools returns (coordinates, endPts, flags); empty
            # glyphs come back as an empty tuple.
            if isinstance(raw, tuple) and len(raw) == 3:
                coords, end_pts, flags = raw
                if coords is not None and len(coords):
                    x_min, _, x_max, _ = coords.calcIntBounds()
                    width = x_max - x_min
                    if width > CELL:
                        factor = CELL / width
                        shift = (CELL - width * factor) / 2 - x_min * factor
                        # x scales into the cell and is centred; y is untouched.
                        # GlyphCoordinates.transform wants a 2x3 nested matrix.
                        coords.transform(((factor, 0), (0, 1), (shift, 0)))
                        coords.toInt()
                        glyph.coordinates = coords
                        glyph.endPtsOfContours = end_pts
                        glyph.flags = flags
                        new_min, _, new_max, _ = coords.calcIntBounds()
                        glyph.xMin, glyph.xMax = new_min, new_max
                        scaled += 1
        hmtx[name] = (CELL, hmtx[name][1])

    font["hhea"].advanceWidthMax = CELL
    return scaled, composites


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    src, dst = sys.argv[1], sys.argv[2]
    font = subset_font(src)
    scaled, composites = make_monospaced(font)
    subset.save_font(font, dst, subset.Options(flavor="woff2"))
    print(f"{dst}: {len(font.getGlyphOrder())} glyphs, {scaled} scaled to {CELL} units, "
          f"{composites} composite")


if __name__ == "__main__":
    main()