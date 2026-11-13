#!/usr/bin/env python3
# Copyright (c) 2026 Kenta IDA
# Distributed under the Boost Software License, Version 1.0.
# (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
# SPDX-License-Identifier: BSL-1.0

"""Extracts glyphs from a PSF2 console font (with a Unicode table) into the
`term` app's font file (doc/39).

usage: tools/psf2font.py FONT.psf[.gz] OUT.bin

OUT.bin: the glyphs for U+0020..U+007E in order, then one for characters
the font lacks; each `height` rows of `width` bits, rows padded to bytes,
most significant bit first (the PSF2 layout).
"""

import gzip
import struct
import sys

FIRST, LAST = 0x20, 0x7E


def main():
    src, out = sys.argv[1], sys.argv[2]
    data = open(src, "rb").read()
    if src.endswith(".gz"):
        data = gzip.decompress(data)
    if data[:4] != bytes.fromhex("72b54a86"):
        sys.exit("not a PSF2 font")
    _, header, flags, count, size, height, width = struct.unpack("<7I", data[4:32])
    if not flags & 1:
        sys.exit("no Unicode table")
    glyphs = [data[header + i * size:header + (i + 1) * size] for i in range(count)]
    # Unicode table: per glyph, UTF-8 sequences ended by 0xFF (0xFE starts
    # a sequence of combined characters, ignored).
    table, at = {}, header + count * size
    for g in range(count):
        end = data.index(b"\xff", at)
        entry = data[at:end].split(b"\xfe")[0]
        for ch in entry.decode("utf-8"):
            table.setdefault(ord(ch), g)
        at = end + 1
    missing = table.get(0xFFFD, table.get(ord("?")))
    blob = b"".join(glyphs[table.get(c, missing)] for c in range(FIRST, LAST + 1)) + glyphs[missing]
    open(out, "wb").write(blob)
    print(f"{out}: {LAST - FIRST + 2} glyphs of {width}x{height}, {len(blob)} bytes")


main()
