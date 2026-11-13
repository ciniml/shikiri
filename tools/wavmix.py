#!/usr/bin/env python3
# Copyright (c) 2026 Kenta IDA
# Distributed under the Boost Software License, Version 1.0.
# (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
# SPDX-License-Identifier: BSL-1.0

"""Checks two tones mixed in a 16-bit stereo WAV (t5emu --audio-out, doc/70 §10).

wavmix.py FILE HZ1 HZ2 MIN_LEVEL MIN_MS: measures the left channel's level at
HZ1 and HZ2 (Goertzel over 10 ms blocks, as the amplitude of a sine) and
requires both at least MIN_LEVEL together for MIN_MS in all, and the peak
above MIN_LEVEL (the sum, not one tone clipped to the other). Prints what it
measured.
"""
import math
import struct
import sys


def level(block, hz, rate):
    c = 2 * math.cos(2 * math.pi * hz / rate)
    s1 = s2 = 0.0
    for x in block:
        s1, s2 = x + c * s1 - s2, s1
    p = s1 * s1 + s2 * s2 - c * s1 * s2
    return 2 * math.sqrt(max(p, 0.0)) / len(block)


def main():
    path, hz1, hz2 = sys.argv[1], float(sys.argv[2]), float(sys.argv[3])
    min_level, min_ms = float(sys.argv[4]), float(sys.argv[5])
    data = open(path, "rb").read()
    rate = struct.unpack_from("<I", data, 24)[0]
    left = [s[0] for s in struct.iter_unpack("<hh", data[44:])]
    block = rate // 100
    both = one = 0
    peak = 0
    for i in range(0, len(left) - block + 1, block):
        b = left[i:i + block]
        a1, a2 = level(b, hz1, rate), level(b, hz2, rate)
        if a1 >= min_level and a2 >= min_level:
            both += 1
            peak = max(peak, max(abs(s) for s in b))
        elif a1 >= min_level or a2 >= min_level:
            one += 1
    print(f"wavmix: both tones for {both * 10} ms (peak {peak}), one for {one * 10} ms")
    return 0 if both * 10 >= min_ms and peak > min_level * 1.5 else 1


sys.exit(main())
