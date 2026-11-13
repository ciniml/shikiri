#!/usr/bin/env python3
# Copyright (c) 2026 Kenta IDA
# Distributed under the Boost Software License, Version 1.0.
# (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
# SPDX-License-Identifier: BSL-1.0

"""Builds the font service's glyph file (doc/45).

usage: tools/mkfont.py OUT.bin [--sheet SHEET.png]

Narrow glyphs (16 x 32) come from Terminus Font console fonts (PSF2 with
a Unicode table, SIL OFL 1.1); wide glyphs (32 x 32) are rendered from
Noto Sans Mono CJK JP (SIL OFL 1.1) for the East Asian wide and fullwidth
characters of JIS X 0208, and narrow ones from it for the halfwidth
katakana. ASCII stays in `term` itself.

OUT.bin, little-endian u32s (doc/45 §2):
  header: magic "T5F1", narrow count, narrow codes offset, narrow bitmaps
          offset, wide count, wide codes offset, wide bitmaps offset
  codes:  sorted code points
  narrow: 32 rows of u16 per glyph, bit 15 the leftmost pixel
  wide:   32 rows of u32 per glyph, bit 31 the leftmost pixel
"""

import gzip
import struct
import sys
import unicodedata

from PIL import Image, ImageDraw, ImageFont

CONSOLE = "/usr/share/consolefonts/"
PSFS = ["Lat15", "Uni2", "Uni3", "FullGreek", "FullCyrSlav"]
NOTO = "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"
NOTO_NAME = "Noto Sans Mono CJK JP"
H = 32
# The ideographic em box: 28 pixels, 2 from the top of the cell.
EM, TOP = 28, 2


def psf_glyphs(path):
    data = gzip.decompress(open(path, "rb").read())
    if data[:4] != bytes.fromhex("72b54a86"):
        sys.exit(f"{path}: not a PSF2 font")
    _, header, flags, count, size, height, width = struct.unpack("<7I", data[4:32])
    assert (height, width) == (32, 16) and flags & 1
    table = data[header + count * size:]
    out, i, pos = {}, 0, 0
    while i < count:
        end = table.index(b"\xff", pos)
        entry = table[pos:end]
        pos = end + 1
        # Single code points only (0xFE starts sequences).
        for cp in entry.split(b"\xfe")[0].decode("utf-8"):
            rows = data[header + i * size:header + (i + 1) * size]
            out.setdefault(ord(cp), [rows[2 * r] << 8 | rows[2 * r + 1] for r in range(32)])
        i += 1
    return out


def noto():
    for index in range(16):
        f = ImageFont.truetype(NOTO, EM, index=index)
        if " ".join(f.getname()).startswith(NOTO_NAME):
            return f
    sys.exit("no " + NOTO_NAME)


def render(font, ch, width):
    img = Image.new("L", (width, H), 0)
    d = ImageDraw.Draw(img)
    ascent, _ = font.getmetrics()
    # Baseline so that the em box (ascent 0.88 em) starts at TOP.
    d.text((width / 2, TOP + EM * 0.88), ch, font=font, fill=255, anchor="ms")
    px = img.load()
    rows = []
    for y in range(H):
        v = 0
        for x in range(width):
            v = v << 1 | (px[x, y] >= 128)
        rows.append(v)
    return rows


def jis0208():
    out = set()
    for hi in range(0xA1, 0xFF):
        for lo in range(0xA1, 0xFF):
            try:
                out.add(bytes([hi, lo]).decode("euc_jp"))
            except UnicodeDecodeError:
                pass
    return out


def main():
    out = sys.argv[1]
    narrow = {}
    for name in PSFS:
        for cp, rows in psf_glyphs(f"{CONSOLE}{name}-Terminus32x16.psf.gz").items():
            narrow.setdefault(cp, rows)
    for cp in range(0x20, 0x7F):
        narrow.pop(cp, None)
    f = noto()
    for cp in range(0xFF61, 0xFFA0):
        narrow.setdefault(cp, render(f, chr(cp), 16))
    wide = {}
    chars = jis0208() | {"～", "〜", "―", "—", "－", "−"}
    for ch in sorted(chars):
        if unicodedata.east_asian_width(ch) in ("W", "F"):
            wide[ord(ch)] = render(f, ch, 32)
    blob = bytearray()
    header = 4 * 7
    nc = sorted(narrow)
    wc = sorted(wide)
    n_codes = header
    n_bits = n_codes + 4 * len(nc)
    w_codes = n_bits + 64 * len(nc)
    w_bits = w_codes + 4 * len(wc)
    blob += b"T5F1" + struct.pack("<6I", len(nc), n_codes, n_bits, len(wc), w_codes, w_bits)
    blob += b"".join(struct.pack("<I", c) for c in nc)
    blob += b"".join(struct.pack("<32H", *narrow[c]) for c in nc)
    blob += b"".join(struct.pack("<I", c) for c in wc)
    blob += b"".join(struct.pack("<32I", *wide[c]) for c in wc)
    open(out, "wb").write(blob)
    print(f"{out}: {len(nc)} narrow, {len(wc)} wide, {len(blob)} bytes")
    if "--sheet" in sys.argv:
        text = "日本語の表示テスト：漢字かなカナ。「東京」ｶﾀｶﾅ─│┌┐ äöü αβγ"
        img = Image.new("L", (16 * len(text) * 2, H), 0)
        x = 0
        for ch in text:
            cp = ord(ch)
            if cp in wide:
                rows, w = wide[cp], 32
            elif cp in narrow:
                rows, w = narrow[cp], 16
            else:
                rows, w = [0xFFFF] * 32, 16
            for y, v in enumerate(rows):
                for i in range(w):
                    if v >> (w - 1 - i) & 1:
                        img.putpixel((x + i, y), 255)
            x += w
        img.crop((0, 0, x, H)).resize((x * 2, H * 2), Image.NEAREST).save(sys.argv[sys.argv.index("--sheet") + 1])


main()
