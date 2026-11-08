#!/bin/sh
# A microSD image for the emulator (doc/64): an MBR with one FAT32
# partition from sector 2048, made with mtools, holding files with long
# names (as a PC would write them) and an 8.3 one.
#   tools/mksdimg.sh OUT.img [MIB]
set -e
out=$1
mib=${2:-64}
rm -f "$out"
dd if=/dev/zero of="$out" bs=1M count="$mib" status=none
sectors=$((mib * 2048))
python3 - "$out" "$sectors" <<'PY'
import struct, sys
out, sectors = sys.argv[1], int(sys.argv[2])
start = 2048
entry = struct.pack("<B3sB3sII", 0, b"\0\0\0", 0x0C, b"\0\0\0", start, sectors - start)
with open(out, "r+b") as f:
    f.seek(446)
    f.write(entry + bytes(48) + b"\x55\xaa")
PY
img="$out@@1M"
export MTOOLS_SKIP_CHECK=1
mformat -i "$img" -F -T $((sectors - 2048)) -v T5EMU ::
tmp=$(mktemp -d)
printf 'hello from a long name\n' > "$tmp/long"
seq 1 60 | sed 's/^/line /' > "$tmp/lines"
printf 'short\n' > "$tmp/short"
mcopy -i "$img" "$tmp/long" "::/Long File Name.txt"
mcopy -i "$img" "$tmp/lines" "::/sixty-lines-of-text.txt"
mmd -i "$img" "::/My Documents"
mcopy -i "$img" "$tmp/long" "::/My Documents/notes-about-tab5.txt"
mcopy -i "$img" "$tmp/short" "::/SHORT.TXT"
rm -r "$tmp"
