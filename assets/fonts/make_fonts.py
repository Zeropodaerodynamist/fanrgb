"""Subset Inter static TTFs for FanRGB and bake tabular figures into digit-only fonts.

Usage: python -I make_fonts.py <src_ttf_dir> <out_dir>
"""
import sys
from pathlib import Path

from fontTools import subset
from fontTools.ttLib import TTFont

SRC = Path(sys.argv[1])
OUT = Path(sys.argv[2])
OUT.mkdir(parents=True, exist_ok=True)

# Latin, Latin-1, Latin Ext-A, general punctuation, arrows, math, technical, shapes, dingbats.
UNICODES = (
    list(range(0x20, 0x7F))
    + list(range(0xA0, 0x180))
    + list(range(0x2000, 0x2070))
    + [0x20AC, 0x2116, 0x2122, 0x2126, 0x2190, 0x2191, 0x2192, 0x2193, 0x2194, 0x21B6, 0x21B7, 0x21BA, 0x21BB]
    + list(range(0x2200, 0x2300))
    + list(range(0x2300, 0x2400))
    + list(range(0x25A0, 0x2600))
    + list(range(0x2713, 0x2719))
    + list(range(0x0370, 0x0400))  # Greek (τ, μ, Δ …)
)

TAB = {
    "0": "zero.tf", "1": "one.tf", "2": "two.tf", "3": "three.tf", "4": "four.tf",
    "5": "five.tf", "6": "six.tf", "7": "seven.tf", "8": "eight.tf", "9": "nine.tf",
    ".": "period.tf", ",": "comma.tf", ":": "colon.tf", "-": "hyphen.tf", "+": "plus.tf",
    "−": "minus.tf",
}


def opts():
    o = subset.Options()
    o.layout_features = []  # ab_glyph applies no OpenType features
    o.hinting = False
    o.name_IDs = ["*"]
    o.notdef_outline = True
    o.drop_tables += ["GSUB", "GPOS", "GDEF", "DSIG"]
    return o


for weight in ["Regular", "Medium", "SemiBold"]:
    src = SRC / f"Inter-{weight}.ttf"
    # 1) Text font.
    f = TTFont(src)
    s = subset.Subsetter(opts())
    s.populate(unicodes=UNICODES)
    s.subset(f)
    f.save(OUT / f"Inter-{weight}.ttf")
    # 2) Tabular digits: point the cmap at the .tf glyphs, keep only those code points.
    f = TTFont(src)
    order = set(f.getGlyphOrder())
    for table in f["cmap"].tables:
        if table.isUnicode():
            for ch, g in TAB.items():
                if ord(ch) in table.cmap and g in order:
                    table.cmap[ord(ch)] = g
    s = subset.Subsetter(opts())
    s.populate(unicodes=[ord(c) for c in TAB])
    s.subset(f)
    f.save(OUT / f"InterTab-{weight}.ttf")

for p in sorted(OUT.glob("*.ttf")):
    t = TTFont(p)
    cm = t.getBestCmap()
    print(p.name, p.stat().st_size, "glyphs", len(t.getGlyphOrder()), "'1'->", cm.get(ord("1")),
          "adv1", t["hmtx"][cm[ord("1")]][0], "adv0", t["hmtx"][cm[ord("0")]][0])
