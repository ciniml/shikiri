#!/bin/sh
set -e
CC=$HOME/.espressif/tools/riscv32-esp-elf/esp-14.2.0_20260121/riscv32-esp-elf/bin/riscv32-esp-elf-gcc
cd "$(dirname "$0")"
for p in VIRT:0x80000000 ESP32C3:0x40380000; do
  n=${p%%:*}; b=${p##*:}
  sed "s/BASE/$b/" link.ld.in > link_$n.ld
  $CC -march=rv32imac_zicsr -mabi=ilp32 -nostdlib -nostartfiles -DPLAT_$n -T link_$n.ld -o umode_$n.elf umode.S
done
ls -la *.elf
