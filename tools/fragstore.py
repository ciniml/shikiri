#!/usr/bin/env python3
# Copyright (c) 2026 Kenta IDA
# Distributed under the Boost Software License, Version 1.0.
# (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
# SPDX-License-Identifier: BSL-1.0

"""A fragmented copy of an app store image (doc/65), for the emulator: the
same apps, moved so that one or two free extents lie before each and none
after the last, so no free run is longer than two extents though many
extents are free. The catalog is rebuilt (sector 0, sequence number 1).
    tools/fragstore.py IN_DIR OUT_DIR   (appmeta.bin, apps.bin)"""
import os
import struct
import sys
import zlib

SECTOR, ENTRY, HEADER, EXTENT, EXTENTS = 4096, 64, 16, 0x10000, 128


def main():
    src, dst = sys.argv[1], sys.argv[2]
    meta = open(os.path.join(src, "appmeta.bin"), "rb").read()
    apps = open(os.path.join(src, "apps.bin"), "rb").read()
    apps = apps + b"\xff" * (EXTENTS * EXTENT - len(apps))
    count = struct.unpack_from("<H", meta, 6)[0]
    entries = [bytearray(meta[HEADER + i * ENTRY:HEADER + (i + 1) * ENTRY]) for i in range(count)]
    entries.sort(key=lambda e: struct.unpack_from("<H", e, 32)[0])
    used = sum(struct.unpack_from("<H", e, 34)[0] for e in entries)
    free = EXTENTS - used
    assert free >= len(entries), "not enough free extents to spread"
    # One or two free extents before each app.
    gaps = [1] * len(entries)
    for i in range(free - len(entries)):
        gaps[i % len(gaps)] += 1
    assert max(gaps) <= 2, "too few apps to keep the runs short"
    out = bytearray(b"\xff" * (EXTENTS * EXTENT))
    at = 0
    for e, gap in zip(entries, gaps):
        first, n, length = struct.unpack_from("<HHI", e, 32)
        at += gap
        out[at * EXTENT:at * EXTENT + length] = apps[first * EXTENT:first * EXTENT + length]
        struct.pack_into("<H", e, 32, at)
        at += n
    assert at == EXTENTS
    sector = bytearray(b"\xff" * SECTOR)
    sector[:HEADER] = meta[:HEADER]
    struct.pack_into("<I", sector, 8, 1)
    for i, e in enumerate(entries):
        sector[HEADER + i * ENTRY:HEADER + (i + 1) * ENTRY] = e
    struct.pack_into("<I", sector, SECTOR - 4, zlib.crc32(bytes(sector[:SECTOR - 4])))
    os.makedirs(dst, exist_ok=True)
    rest = b"\xff" * (len(meta) - SECTOR)
    open(os.path.join(dst, "appmeta.bin"), "wb").write(bytes(sector) + rest)
    open(os.path.join(dst, "apps.bin"), "wb").write(bytes(out))
    print(f"{len(entries)} apps, {used} extents used, {free} free in runs of at most {max(gaps)}")


main()
