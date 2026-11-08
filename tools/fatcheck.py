#!/usr/bin/env python3
"""Checks the long names (VFAT LFN) in a FAT32 disk image (doc/63): every
live LFN entry belongs to a run, in order (the last part first, with the
0x40 flag), right before the 8.3 entry whose checksum it carries; no
orphans. Walks every directory. Prints each entry's names; exits 1 on a
problem.
    tools/fatcheck.py IMAGE"""
import struct
import sys


def main():
    img = open(sys.argv[1], "rb").read()
    start = struct.unpack_from("<I", img, 446 + 8)[0] * 512
    bps, spc, reserved, nfats = struct.unpack_from("<HBHB", img, start + 11)
    fat_size, root = struct.unpack_from("<I", img, start + 36)[0], struct.unpack_from("<I", img, start + 44)[0]
    assert bps == 512, "512-byte sectors only"
    fat = start + reserved * 512
    data = fat + nfats * fat_size * 512
    problems = []

    def chain(c):
        while 2 <= c < 0x0FFFFFF8:
            yield c
            c = struct.unpack_from("<I", img, fat + 4 * c)[0] & 0x0FFFFFFF

    def entries(c):
        for cl in chain(c):
            base = data + (cl - 2) * spc * 512
            for o in range(0, spc * 512, 32):
                yield img[base + o:base + o + 32]

    def csum(short):
        s = 0
        for b in short:
            s = (((s & 1) << 7) + (s >> 1) + b) & 0xFF
        return s

    def walk(c, path):
        run = []
        for e in entries(c):
            if e[0] == 0:
                break
            if e[0] == 0xE5:
                if run:
                    problems.append(f"{path}: LFN run cut by a deleted entry")
                run = []
                continue
            if e[11] == 0x0F:
                run.append(e)
                continue
            name = e[:11]
            long = None
            if run:
                n = len(run)
                ok = run[0][0] == 0x40 | n and all(r[0] & 0x3F == n - k for k, r in enumerate(run)) and all(r[13] == csum(name) for r in run)
                if not ok:
                    problems.append(f"{path}: bad LFN run before {name!r}")
                units = b"".join(r[1:11] + r[14:26] + r[28:32] for r in reversed(run))
                long = units.decode("utf-16-le").split("\0")[0]
                run = []
            short = name[:8].decode(errors="replace").rstrip() + ("." + name[8:].decode(errors="replace").rstrip() if name[8:].strip() else "")
            print(f"{path}/{long or short}  [{short}]")
            if e[11] & 0x10 and name[:2] not in (b". ", b".."):
                cl = struct.unpack_from("<H", e, 20)[0] << 16 | struct.unpack_from("<H", e, 26)[0]
                walk(cl, f"{path}/{long or short}")
        if run:
            problems.append(f"{path}: orphan LFN entries at the end")

    walk(root, "")
    for p in problems:
        print("PROBLEM:", p)
    sys.exit(1 if problems else 0)


main()
