#!/bin/sh
# Checks that the kernel's .ram_text (code that runs while the L2 cache and
# with it the flash window are suspended, doc/14 §5.2) never reaches flash:
# every jal/j stays inside the section and nothing is addressed pc-relative
# (auipc), which would point into flash-resident .text/.rodata. Calls to ROM
# go through lui+jalr with fixed addresses and pass. t5emu catches the rest
# at run time.
# Usage: check-ram-text.sh KERNEL_ELF
set -eu
elf=$1
rust-objdump -d -j .ram_text "$elf" | gawk '
/^[0-9a-f]+ <__ram_text_start>:/ { start = strtonum("0x" $1) }
/^[0-9a-f]+:/ { last = strtonum("0x" substr($1, 1, length($1) - 1)) }
/\tauipc\t/ { print "auipc in .ram_text: " $0; bad = 1 }
/\t(jal|j)\t/ {
    for (i = 1; i <= NF; i++) if ($i ~ /^0x[0-9a-f]+$/) t = strtonum($i)
    if (t < start || t > start + 0x10000) { print "jump out of .ram_text: " $0; bad = 1 }
}
END {
    if (start == 0) { print "no .ram_text in '"$elf"'"; exit 1 }
    if (bad) exit 1
    printf ".ram_text ok (%d bytes checked)\n", last - start + 2
}'
