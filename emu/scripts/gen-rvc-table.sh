#!/bin/sh
# Generates the RVC reference table used by `cargo test` in emu/ (rvc.rs,
# cross_check_with_llvm_table): every 16-bit encoding is disassembled with
# llvm-mc (+c,+f), and the disassembly is reassembled without C to get the
# equivalent 32-bit instruction. Usage:
#   emu/scripts/gen-rvc-table.sh OUT_DIR
#   T5EMU_RVC_TABLE=OUT_DIR/table.txt cargo test --manifest-path emu/Cargo.toml
set -eu
out=${1:?usage: $0 OUT_DIR}
mc=${LLVM_MC:-llvm-mc-18}
mkdir -p "$out"
python3 - "$out" "$mc" <<'PY'
import re, subprocess, sys
out, mc = sys.argv[1], sys.argv[2]
words = "".join(f"0x{v & 0xff:02x} 0x{v >> 8:02x}\n" for v in range(65536) if v & 3 != 3)
dis = subprocess.run([mc, "--disassemble", "-show-encoding", "-triple=riscv32", "-mattr=+c,+f,+m,+a"],
                     input=words, capture_output=True, text=True).stdout
rows = []
for line in dis.splitlines():
    if "# encoding:" not in line:
        continue
    text, enc = line.split("# encoding:")
    b = [int(x, 16) for x in re.findall(r"0x([0-9a-f]+)", enc)]
    rows.append((b[0] | b[1] << 8, text.strip()))
enc = {}
for t in sorted({t for _, t in rows}):
    r = subprocess.run([mc, "-show-encoding", "-triple=riscv32", "-mattr=+f,+m,+a"],
                       input=t + "\n", capture_output=True, text=True)
    m = re.findall(r"encoding: \[([^\]]*)\]", r.stdout)
    if m:
        enc[t] = sum(int(x, 16) << (8 * i) for i, x in enumerate(m[0].split(",")))
with open(f"{out}/table.txt", "w") as f:
    for c, t in rows:
        f.write(f"{c:04x} {enc[t]:08x} {t}\n" if t in enc else f"{c:04x} ? {t}\n")
print(f"{len(rows)} encodings -> {out}/table.txt")
PY
