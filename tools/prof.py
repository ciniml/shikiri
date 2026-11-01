#!/usr/bin/env python3
"""Summarise a `prof` run (doc/34) from a decoded log (t5log or t5emu).

usage: tools/prof.py LOG ELF [--top N] [--lines]

LOG is the text log with the `prof.summary`, `prof.syscall` and
`prof.pcs` events; ELF is the profiled app (linked at 0). Samples are
grouped by function (`nm`); `--lines` also groups them by source line
(`addr2line -i`, slower). A sample marked "entered" was taken right after
the kernel returned to the process: the time went to the kernel, and the
PC is just after the system call that led there.
"""

import bisect
import collections
import re
import subprocess
import sys


def symbols(elf):
    out = subprocess.run(["nm", "-n", "-C", "--defined-only", elf], capture_output=True, text=True, check=True).stdout
    addrs, names = [], []
    for line in out.splitlines():
        parts = line.split(" ", 2)
        if len(parts) == 3 and parts[1] in "tTwW" and not parts[2].startswith(("$", ".L")):
            addrs.append(int(parts[0], 16))
            names.append(parts[2])
    return addrs, names


def lookup(addrs, names, off):
    i = bisect.bisect_right(addrs, off) - 1
    return names[i] if i >= 0 else "?"


def main():
    argv = sys.argv[1:]
    top = 30
    if "--top" in argv:
        k = argv.index("--top")
        top = int(argv[k + 1])
        del argv[k:k + 2]
    args = [a for a in argv if not a.startswith("--")]
    if len(args) != 2:
        sys.exit(__doc__)
    log, elf = args
    text = None
    samples = []
    syscalls = []
    for line in open(log, errors="replace"):
        if "prof.summary" in line:
            m = re.search(r"text=0x([0-9a-f]+)", line)
            text = int(m.group(1), 16)
            print(line.strip())
        elif "prof.syscall" in line:
            syscalls.append(line.strip())
        elif "prof.pcs" in line:
            m = re.search(r"pcs=([0-9a-f]+)", line)
            b = bytes.fromhex(m.group(1))
            samples += [int.from_bytes(b[i:i + 4], "little") for i in range(0, len(b), 4)]
    if text is None:
        sys.exit("no prof.summary in the log")
    for s in syscalls:
        print(s)
    addrs, names = symbols(elf)
    user = collections.Counter()
    kernel = collections.Counter()
    for s in samples:
        off = (s & ~1) - text
        name = lookup(addrs, names, off)
        (kernel if s & 1 else user)[name] += 1
    n = len(samples)
    nk = sum(kernel.values())
    print(f"\n{n} samples: user {n - nk} ({100 * (n - nk) / max(n, 1):.1f}%), kernel {nk} ({100 * nk / max(n, 1):.1f}%)")
    print(f"\n-- user, by function (top {top})")
    for name, c in user.most_common(top):
        print(f"{c:6d} {100 * c / n:5.1f}%  {name[:150]}")
    print(f"\n-- kernel (attributed to the caller of the system call)")
    for name, c in kernel.most_common(top // 2):
        print(f"{c:6d} {100 * c / n:5.1f}%  {name[:150]}")
    if "--lines" in sys.argv:
        offs = [hex((s & ~1) - text) for s in samples if not s & 1]
        out = subprocess.run(["addr2line", "-e", elf, "-a"] + offs, capture_output=True, text=True).stdout.split("\n")
        lines = collections.Counter()
        cur = None
        for l in out:
            if l.startswith("0x"):
                cur = l
            elif cur and l:
                lines[l.split(" (")[0]] += 1
                cur = None
        print(f"\n-- user, by line (top {top})")
        for name, c in lines.most_common(top):
            print(f"{c:6d} {100 * c / n:5.1f}%  {name}")


if __name__ == "__main__":
    main()
