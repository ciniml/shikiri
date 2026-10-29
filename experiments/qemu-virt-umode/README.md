# QEMU virt での U モード / PMP / ecall の試作（調査用）

doc/08 §4 の検証に使った asm の試作。再現手順は次のとおり。

```sh
sh build.sh   # riscv32-esp-elf-gcc（ESP-IDF 付属）が必要。esp32c3 版もビルドされる
qemu-system-riscv32 -M virt -cpu rv32,s=off,h=off,mmu=off,d=off,pmp=on \
  -bios none -nographic -kernel umode_VIRT.elf
```

期待される出力は `run_virt.log` にある。製品コードではなく、M1-virt の雛形として参照する。
