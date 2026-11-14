#!/usr/bin/env python3
"""Generate an alignment test pattern PNG (standard library only).

    python3 test_pattern.py WIDTH HEIGHT OUT.png [--cell 120]

What to look for on the displays:
  * grid lines every `cell` px  -> must continue straight across screen seams
                                   and through blend overlaps (no doubling)
  * white circle                -> must look round (wrong aspect = ellipse)
  * center cross                -> marks the canvas center
  * corner blocks               -> orientation: TL red, TR green,
                                   BR blue, BL yellow
"""

import argparse
import struct
import zlib

BG = (24, 24, 28)
GRID = (110, 110, 125)
WHITE = (235, 235, 235)
CORNERS = {"tl": (220, 40, 40), "tr": (40, 200, 70), "br": (50, 90, 230), "bl": (230, 200, 40)}


def pattern(w: int, h: int, cell: int) -> bytes:
    cx, cy = w / 2, h / 2
    radius = min(w, h) * 0.45
    block = max(cell // 2, 20)
    grid_col = [x % cell < 2 or x >= w - 2 for x in range(w)]
    cross_col = [abs(x + 0.5 - cx) < 2 for x in range(w)]

    raw = bytearray()
    for y in range(h):
        raw.append(0)  # PNG filter type: none
        grid_row = y % cell < 2 or y >= h - 2
        cross_row = abs(y + 0.5 - cy) < 2
        dy2 = (y + 0.5 - cy) ** 2
        top, bottom = y < block, y >= h - block
        for x in range(w):
            left, right = x < block, x >= w - block
            if top and left:
                c = CORNERS["tl"]
            elif top and right:
                c = CORNERS["tr"]
            elif bottom and right:
                c = CORNERS["br"]
            elif bottom and left:
                c = CORNERS["bl"]
            elif cross_row or cross_col[x]:
                c = WHITE
            elif abs(((x + 0.5 - cx) ** 2 + dy2) ** 0.5 - radius) < 1.5:
                c = WHITE
            elif grid_row or grid_col[x]:
                c = GRID
            else:
                c = BG
            raw.extend(c)
    return bytes(raw)


def write_png(path: str, w: int, h: int, rgb: bytes) -> None:
    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    header = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)  # 8-bit RGB
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", header))
        f.write(chunk(b"IDAT", zlib.compress(rgb, 9)))
        f.write(chunk(b"IEND", b""))


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("width", type=int)
    ap.add_argument("height", type=int)
    ap.add_argument("out")
    ap.add_argument("--cell", type=int, default=120, help="grid spacing in px (default 120)")
    a = ap.parse_args()
    write_png(a.out, a.width, a.height, pattern(a.width, a.height, a.cell))
    print(f"wrote {a.out} ({a.width}x{a.height})")


if __name__ == "__main__":
    main()
