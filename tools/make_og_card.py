"""Build Orrery's Open Graph card: 1200x630, drawn as a printed plate.

    python3 tools/make_og_card.py

Everything is drawn rather than screenshotted, so the type stays crisp and the
file stays small. Colours and fonts are the page's own.

Two things this had to be careful about:

  * Fraunces at opsz=144 -- what the page asks for -- has an 'e' crossbar so
    fine that the rasteriser drops it, and the name renders as "Orrcry". 100 is
    the largest optical size that survives here with the contrast intact.
  * The 'y' descender reaches far below the baseline at this size, so the
    tagline is placed from the title's measured bounding box rather than from
    a guessed line height.
"""
import os
import shutil
import subprocess

from PIL import Image, ImageDraw, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
OUT = os.path.join(ROOT, "web", "public", "og-v1.png")
CACHE = os.path.join(HERE, ".fonts")

# The page's three families, from Google's own repository. Cached so a rerun
# costs nothing, and gitignored -- 1.8 MB of TTF to rebuild one 18 KB file is
# not worth committing.
FONTS = {
    "Fraunces.ttf": "fraunces/Fraunces%5BSOFT,WONK,opsz,wght%5D.ttf",
    "Newsreader.ttf": "newsreader/Newsreader%5Bopsz,wght%5D.ttf",
    "Newsreader-Italic.ttf": "newsreader/Newsreader-Italic%5Bopsz,wght%5D.ttf",
    "DMMono.ttf": "dmmono/DMMono-Regular.ttf",
    "DMMono-Medium.ttf": "dmmono/DMMono-Medium.ttf",
}


def fetch_fonts():
    """Fetch with curl rather than urllib, for the same reason build_web.sh
    does: it uses the system trust store, and a Python built without a CA
    bundle -- which the python.org macOS builds are -- cannot verify the
    certificate at all."""
    if not shutil.which("curl"):
        raise SystemExit("curl is required to fetch the fonts")
    os.makedirs(CACHE, exist_ok=True)
    for name, path in FONTS.items():
        dest = os.path.join(CACHE, name)
        if os.path.exists(dest):
            continue
        url = "https://raw.githubusercontent.com/google/fonts/main/ofl/" + path
        print("  fetching", name)
        result = subprocess.run(
            ["curl", "-sSLf", "--max-time", "60", "-o", dest, url],
            capture_output=True, text=True)
        if result.returncode != 0:
            raise SystemExit(f"could not fetch {name}: {result.stderr.strip()}")


fetch_fonts()

W, H = 1200, 630
PAPER     = (244, 241, 234)
INK       = (22, 19, 15)
INK_SOFT  = (74, 68, 59)
INK_FAINT = (133, 124, 109)
RULE      = (214, 207, 191)
EDGE      = (183, 174, 156)     # connectors: RULE is too faint to survive JPEG re-encoding
SPOT      = (27, 63, 160)
SPOT_WASH = (223, 227, 242)
F = CACHE + "/"

def fraunces(size, wght=600, opsz=100):
    f = ImageFont.truetype(F + "Fraunces.ttf", size)
    f.set_variation_by_axes([opsz, wght, 0, 0])
    return f

def newsreader(size, wght=400, italic=False):
    f = ImageFont.truetype(F + ("Newsreader-Italic.ttf" if italic else "Newsreader.ttf"), size)
    f.set_variation_by_axes([wght, min(72, max(6, size))])
    return f

def mono(size, medium=False):
    return ImageFont.truetype(F + ("DMMono-Medium.ttf" if medium else "DMMono.ttf"), size)

img = Image.new("RGB", (W, H), PAPER)
d = ImageDraw.Draw(img)

def tracked(xy, text, font, fill, tracking=0.0, centre=False):
    """Letter-spaced text. PIL has no tracking, and the small-caps labels are
    unreadable without it."""
    x, y = xy
    widths = [d.textlength(ch, font=font) + tracking for ch in text]
    if centre:
        x -= (sum(widths) - tracking) / 2
    for ch, w in zip(text, widths):
        d.text((x, y), ch, font=font, fill=fill)
        x += w

M = 80

# --- masthead -------------------------------------------------------------
tracked((M, 52), "RUST · WEBASSEMBLY · SIMD128 · NO SERVER", mono(17), INK_FAINT, 2.6)
d.line([(M, 102), (W - M, 102)], fill=RULE, width=1)

title_font = fraunces(116, 600, 100)
title_y = 124
d.text((M - 5, title_y), "Orrery", font=title_font, fill=INK)
# Measured, not assumed: the descender of the 'y' runs well past the baseline.
_, _, _, title_bottom = d.textbbox((M - 5, title_y), "Orrery", font=title_font)

tag_font = newsreader(38, 400, italic=True)
d.text((M, title_bottom + 2), "A query engine in the browser", font=tag_font, fill=INK_SOFT)
_, _, _, tag_bottom = d.textbbox((M, title_bottom + 2), "A query engine in the browser", font=tag_font)

# The right of a masthead is dead space unless it carries something. A plate's
# descriptive note, right-ranged against the title -- prose, deliberately, so
# it does not become the row of figures every other card on the feed has.
note = newsreader(27, 400, italic=True)
for i, line in enumerate([
    "Every stage of the query is on",
    "the page \u2014 including each rewrite",
    "the optimizer makes.",
]):
    d.text((W - M, 150 + i * 38), line, font=note, fill=INK_SOFT, anchor="ra")

# --- the diagram ----------------------------------------------------------
ROW_H, GAP = 42, 22
R0 = tag_bottom + 46
rows = [R0 + i * (ROW_H + GAP) for i in range(4)]

def node(cx, row, w, label, moved=False):
    y0 = rows[row]
    d.rounded_rectangle([cx - w / 2, y0, cx + w / 2, y0 + ROW_H], radius=3,
                        fill=SPOT_WASH if moved else (251, 249, 245),
                        outline=SPOT if moved else INK_SOFT,
                        width=2 if moved else 1)
    d.text((cx, y0 + ROW_H / 2), label, font=mono(17, medium=moved),
           fill=SPOT if moved else INK, anchor="mm")

def elbow(parent_row, px, children_x, child_row):
    """A parent down to its children, squared off the way an engraving would."""
    top = rows[parent_row] + ROW_H
    bottom = rows[child_row]
    mid = top + (bottom - top) / 2
    d.line([(px, top), (px, mid)], fill=EDGE, width=1)
    if len(children_x) > 1:
        d.line([(min(children_x), mid), (max(children_x), mid)], fill=EDGE, width=1)
    for cx in children_x:
        d.line([(cx, mid), (cx, bottom)], fill=EDGE, width=1)

tracked((275, rows[0] - 30), "BEFORE", mono(13), INK_FAINT, 2.2, centre=True)
tracked((925, rows[0] - 30), "AFTER", mono(13), INK_FAINT, 2.2, centre=True)

# before: the filter sits above the join
elbow(0, 275, [275], 1); elbow(1, 275, [275], 2); elbow(2, 275, [190, 360], 3)
node(275, 0, 180, "Project"); node(275, 1, 180, "Filter", moved=True)
node(275, 2, 180, "Join");    node(190, 3, 140, "Scan"); node(360, 3, 140, "Scan")

# after: it has moved inside, onto the side it constrains
elbow(0, 925, [925], 1); elbow(1, 925, [840, 1030], 2); elbow(2, 840, [840], 3)
node(925, 0, 180, "Project"); node(925, 1, 180, "Join")
node(840, 2, 160, "Filter", moved=True); node(1030, 2, 140, "Scan")
node(840, 3, 140, "Scan")

# the rewrite itself, between the two
ay = rows[1] + ROW_H / 2
d.line([(505, ay), (688, ay)], fill=SPOT, width=2)
d.polygon([(700, ay), (687, ay - 7), (687, ay + 7)], fill=SPOT)
tracked((600, ay - 36), "PREDICATE PUSHDOWN", mono(14, medium=True), SPOT, 1.8, centre=True)

# --- save -----------------------------------------------------------------
# Flat colour and type, so an adaptive palette costs nothing visually and a
# great deal in bytes. The page's paper grain is deliberately left off: at
# three percent it is invisible at the size a card is ever seen, it defeats
# PNG compression entirely (18 KB becomes 150), and the platforms re-encode
# these to JPEG anyway, which would throw it away regardless.
img = img.quantize(colors=64, method=Image.MEDIANCUT, dither=Image.Dither.NONE)
os.makedirs(os.path.dirname(OUT), exist_ok=True)
img.save(OUT, optimize=True)
print(f"wrote {OUT} ({W}x{H}, {os.path.getsize(OUT) / 1024:.0f} KB)")
