#!/usr/bin/env python3
"""Statistical tests for a dump of the Tab5's RNG (doc/47 §5).

    tools/rngtest.py FILE...

FILE is what `rngdump` filled and `t5log read` saved: little-endian 32-bit
words. For each file:

- a summary like `ent`'s: entropy per byte, the bytes' chi-square, mean,
  serial correlation, how much lzma shrinks it, and a min-entropy estimate
  from the most common byte (NIST SP 800-90B §6.3.1, the MCV estimate);
- repeats: how often a word equals the one before (a register read faster
  than it changes repeats);
- FIPS 140-2's four tests on 20000-bit blocks, as rngtest counts them;
- NIST SP 800-22 rev 1a tests on 1 Mbit sequences: frequency, block
  frequency (M=128), cumulative sums (forward), runs, longest run of ones
  (M=10000), DFT spectral, serial (m=16) and approximate entropy (m=10).
  A test passes when the proportion of sequences with p >= 0.01 is within
  the range §4.2.1 gives for that many sequences.

These find a bad generator, not a good one: a deterministic generator with
a decent mixer passes them too. Only numpy and scipy are needed.
"""

import lzma
import math
import sys

import numpy as np
from scipy.special import erfc, gammaincc

ALPHA = 0.01
SEQ_BITS = 1 << 20


def bits_of(data):
    return np.unpackbits(np.frombuffer(data, dtype=np.uint8))


def summary(data):
    b = np.frombuffer(data, dtype=np.uint8)
    n = len(b)
    counts = np.bincount(b, minlength=256)
    p = counts / n
    entropy = -np.sum(p[p > 0] * np.log2(p[p > 0]))
    expected = n / 256
    chi2 = float(np.sum((counts - expected) ** 2 / expected))
    chi2_p = float(gammaincc(255 / 2, chi2 / 2))
    x = b.astype(np.float64)
    corr = float(np.corrcoef(x[:-1], x[1:])[0, 1])
    ratio = len(lzma.compress(data, preset=6)) / n
    # MCV: an upper bound on the most common value's probability (99%).
    pmax = counts.max() / n
    pu = min(1.0, pmax + 2.576 * math.sqrt(pmax * (1 - pmax) / (n - 1)))
    h_min = -math.log2(pu)
    w = np.frombuffer(data, dtype="<u4")
    repeats = int(np.sum(w[1:] == w[:-1]))
    print(f"  bytes {n}, entropy {entropy:.6f} bits/byte, min-entropy (MCV) {h_min:.3f} bits/byte")
    print(f"  chi-square {chi2:.1f} (255 df, p={chi2_p:.3f}), mean {x.mean():.4f} (127.5), "
          f"serial correlation {corr:+.5f}, lzma {ratio:.4f} of the size")
    print(f"  words equal to the previous one: {repeats} of {len(w) - 1}")


def fips140_2(bits):
    """Failures of each test over the 20000-bit blocks (FIPS 140-2 §4.9.1,
    as rng-tools' rngtest)."""
    blocks = len(bits) // 20000
    fails = {"monobit": 0, "poker": 0, "runs": 0, "long run": 0}
    run_lo = [2315, 1114, 527, 240, 103, 103]
    run_hi = [2685, 1386, 723, 384, 209, 209]
    for k in range(blocks):
        s = bits[k * 20000:(k + 1) * 20000]
        ones = int(s.sum())
        if not 9725 < ones < 10275:
            fails["monobit"] += 1
        nib = s.reshape(-1, 4) @ np.array([8, 4, 2, 1])
        f = np.bincount(nib, minlength=16)
        x = 16 / 5000 * float(np.sum(f.astype(np.float64) ** 2)) - 5000
        if not 2.16 < x < 46.17:
            fails["poker"] += 1
        edges = np.flatnonzero(np.diff(s)) + 1
        starts = np.concatenate(([0], edges))
        lens = np.diff(np.concatenate((starts, [len(s)])))
        vals = s[starts]
        bad = False
        for v in (0, 1):
            l = np.minimum(lens[vals == v], 6)
            c = np.bincount(l, minlength=7)[1:]
            bad |= bool(np.any(c < run_lo) or np.any(c > run_hi))
        if bad:
            fails["runs"] += 1
        if lens.max() >= 26:
            fails["long run"] += 1
    return blocks, fails


def t_frequency(e):
    s = abs(int(np.sum(2 * e.astype(np.int64) - 1)))
    return erfc(s / math.sqrt(2 * len(e)))


def t_block_frequency(e, m=128):
    nb = len(e) // m
    pi = e[:nb * m].reshape(nb, m).mean(axis=1)
    chi2 = 4 * m * float(np.sum((pi - 0.5) ** 2))
    return gammaincc(nb / 2, chi2 / 2)


def t_cusum(e):
    n = len(e)
    s = np.cumsum(2 * e.astype(np.int64) - 1)
    z = int(np.max(np.abs(s)))
    from scipy.stats import norm
    k1 = np.arange((-n / z + 1) // 4, (n / z - 1) // 4 + 1)
    k2 = np.arange((-n / z - 3) // 4, (n / z - 1) // 4 + 1)
    sq = math.sqrt(n)
    p = 1.0
    p -= float(np.sum(norm.cdf((4 * k1 + 1) * z / sq) - norm.cdf((4 * k1 - 1) * z / sq)))
    p += float(np.sum(norm.cdf((4 * k2 + 3) * z / sq) - norm.cdf((4 * k2 + 1) * z / sq)))
    return p


def t_runs(e):
    n = len(e)
    pi = e.mean()
    if abs(pi - 0.5) >= 2 / math.sqrt(n):
        return 0.0
    v = 1 + int(np.sum(e[1:] != e[:-1]))
    return erfc(abs(v - 2 * n * pi * (1 - pi)) / (2 * math.sqrt(2 * n) * pi * (1 - pi)))


def t_longest_run(e, m=10000):
    # SP 800-22 §2.4.4 for n >= 750000: K=6, classes <=10 .. >=16.
    nb = len(e) // m
    pis = [0.0882, 0.2092, 0.2483, 0.1933, 0.1208, 0.0675, 0.0727]
    v = np.zeros(7)
    for k in range(nb):
        s = e[k * m:(k + 1) * m]
        padded = np.concatenate(([0], s, [0]))
        d = np.diff(padded.astype(np.int8))
        longest = int(np.max(np.flatnonzero(d == -1) - np.flatnonzero(d == 1))) if s.any() else 0
        v[min(max(longest, 10), 16) - 10] += 1
    chi2 = float(np.sum((v - nb * np.array(pis)) ** 2 / (nb * np.array(pis))))
    return gammaincc(6 / 2, chi2 / 2)


def t_dft(e):
    n = len(e)
    x = 2 * e.astype(np.float64) - 1
    m = np.abs(np.fft.rfft(x))[: n // 2]
    t = math.sqrt(math.log(1 / 0.05) * n)
    n0 = 0.95 * n / 2
    n1 = int(np.sum(m < t))
    d = (n1 - n0) / math.sqrt(n * 0.95 * 0.05 / 4)
    return erfc(abs(d) / math.sqrt(2))


def psi2(e, m):
    if m <= 0:
        return 0.0
    n = len(e)
    ext = np.concatenate((e, e[: m - 1])).astype(np.int64)
    idx = np.zeros(n, dtype=np.int64)
    for k in range(m):
        idx = (idx << 1) | ext[k:k + n]
    c = np.bincount(idx, minlength=1 << m).astype(np.float64)
    return (1 << m) / n * float(np.sum(c ** 2)) - n


def t_serial(e, m=16):
    p0, p1, p2 = psi2(e, m), psi2(e, m - 1), psi2(e, m - 2)
    d1, d2 = p0 - p1, p0 - 2 * p1 + p2
    return min(gammaincc(2 ** (m - 2), d1 / 2), gammaincc(2 ** (m - 3), d2 / 2))


def phi(e, m):
    n = len(e)
    ext = np.concatenate((e, e[: m - 1])).astype(np.int64)
    idx = np.zeros(n, dtype=np.int64)
    for k in range(m):
        idx = (idx << 1) | ext[k:k + n]
    c = np.bincount(idx, minlength=1 << m) / n
    c = c[c > 0]
    return float(np.sum(c * np.log(c)))


def t_approx_entropy(e, m=10):
    n = len(e)
    ap_en = phi(e, m) - phi(e, m + 1)
    chi2 = 2 * n * (math.log(2) - ap_en)
    return gammaincc(2 ** (m - 1), chi2 / 2)


NIST = [
    ("frequency", t_frequency),
    ("block frequency", t_block_frequency),
    ("cumulative sums", t_cusum),
    ("runs", t_runs),
    ("longest run", t_longest_run),
    ("DFT spectral", t_dft),
    ("serial", t_serial),
    ("approximate entropy", t_approx_entropy),
]


def nist(bits):
    seqs = len(bits) // SEQ_BITS
    p = 1 - ALPHA
    low = p - 3 * math.sqrt(p * (1 - p) / seqs)
    print(f"  NIST SP 800-22 on {seqs} sequences of {SEQ_BITS} bits "
          f"(pass: at least {low:.3f} of them with p >= {ALPHA}):")
    ok = True
    for name, test in NIST:
        ps = [float(test(bits[k * SEQ_BITS:(k + 1) * SEQ_BITS])) for k in range(seqs)]
        passed = sum(x >= ALPHA for x in ps) / seqs
        verdict = "pass" if passed >= low else "FAIL"
        ok &= passed >= low
        print(f"    {name:20} {passed:.3f}  min p={min(ps):.4f}  {verdict}")
    return ok


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    all_ok = True
    for path in sys.argv[1:]:
        data = open(path, "rb").read()
        print(path)
        summary(data)
        bits = bits_of(data)
        blocks, fails = fips140_2(bits)
        total = sum(fails.values())
        print(f"  FIPS 140-2: {blocks} blocks, failures: "
              + ", ".join(f"{k} {v}" for k, v in fails.items())
              + f" (a good source fails about 1 block in 1250)")
        all_ok &= nist(bits)
        all_ok &= total <= max(3, blocks // 250)
    sys.exit(0 if all_ok else 1)


if __name__ == "__main__":
    main()
