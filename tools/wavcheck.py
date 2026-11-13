#!/usr/bin/env python3
# Copyright (c) 2026 Kenta IDA
# Distributed under the Boost Software License, Version 1.0.
# (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
# SPDX-License-Identifier: BSL-1.0

"""Checks a tone in a 16-bit stereo WAV (t5emu --audio-out, doc/70).

wavcheck.py FILE HZ MIN_PEAK [MAX_MS]: finds the non-silent part of the left
channel and requires its frequency (from zero crossings in its 10 ms blocks
that are not silent: a dropout inside does not count) within 2 % of HZ,
its peak at least MIN_PEAK and, if given, its length at most MAX_MS (a TX
ring left unpolled repeats its last turn forever). Prints what it measured.
"""
import struct
import sys


def main():
    path, hz, min_peak = sys.argv[1], float(sys.argv[2]), int(sys.argv[3])
    max_ms = float(sys.argv[4]) if len(sys.argv) > 4 else None
    data = open(path, "rb").read()
    rate = struct.unpack_from("<I", data, 24)[0]
    left = [s[0] for s in struct.iter_unpack("<hh", data[44:])]
    loud = [i for i, s in enumerate(left) if abs(s) > 64]
    if not loud:
        print("wavcheck: silent")
        return 1
    part = left[loud[0]:loud[-1] + 1]
    secs = len(part) / rate
    block = rate // 100
    crossings = sounding = 0
    for i in range(0, len(part) - block + 1, block):
        b = part[i:i + block]
        if max(abs(s) for s in b) > 64:
            # From the sample before the block: no crossing falls between.
            w = part[max(i - 1, 0):i + block]
            crossings += sum(1 for x, y in zip(w, w[1:]) if (x < 0) != (y < 0))
            sounding += len(b)
    freq = crossings / 2 / (sounding / rate) if sounding else 0.0
    peak = max(abs(s) for s in part)
    print(f"wavcheck: {secs * 1000:.0f} ms of tone, {freq:.1f} Hz, peak {peak}")
    ok = abs(freq - hz) <= hz * 0.02 and peak >= min_peak
    return 0 if ok and (max_ms is None or secs * 1000 <= max_ms) else 1


sys.exit(main())
