"""MSIX logo assets from assets/icon.png (the MeshLive app icon, see
assets/PROVENANCE.md): the icon scaled onto a black tile. Writes into the
given layout folder's Assets/ directory. Usage: python make_assets.py <icon> <out_dir>"""
import sys
from pathlib import Path
from PIL import Image

SIZES = {
    "Square44x44Logo.png": (44, 44),
    "Square44x44Logo.targetsize-44_altform-unplated.png": (44, 44),
    "Square150x150Logo.png": (150, 150),
    "Wide310x150Logo.png": (310, 150),
    "StoreLogo.png": (50, 50),
}


def tile(icon: Image.Image, size):
    w, h = size
    side = min(w, h)
    canvas = Image.new("RGBA", (w, h), (0, 0, 0, 255))
    scaled = icon.convert("RGBA").resize((side, side), Image.LANCZOS)
    canvas.paste(scaled, ((w - side) // 2, (h - side) // 2))
    return canvas


def main():
    icon = Image.open(sys.argv[1])
    out = Path(sys.argv[2])
    out.mkdir(parents=True, exist_ok=True)
    for name, size in SIZES.items():
        tile(icon, size).save(out / name)
    print(f"assets={len(SIZES)} dir={out}")


if __name__ == "__main__":
    main()
