# tab5-os

M5Stack Tab5（ESP32-P4 rev v1.x）向けの Rust 製の自作 OS（開発中）。カーネルは M モード、アプリは U モードで動かし、PMP で分離する。

- 設計と調査: [doc/](doc/README.md)（結論は [doc/07](doc/07-architecture-proposal.md)）
- 進捗: M0（雛形、静的 PIE、ブートローダ）、M1-virt（QEMU virt）、M1a/M1b（実機の Tab5 で U モードの hello と保護テスト 7 件。記録は [doc/09](doc/09-bringup-m1.md)）、**M2（タイマ割り込みによるプリエンプション、sleep/yield/time、FPU の遅延切り替え。QEMU と実機で PASS。記録は [doc/10](doc/10-m2-scheduling.md)）**

## 構成

| ディレクトリ | 内容 |
|---|---|
| `kernel/` | M モードカーネル（stable Rust）。`arch/riscv32` は共通、`platform/{virt,esp32p4v1}` は機種ごと |
| `abi/` | syscall 番号、エラーコード、T5A イメージ形式（カーネル、アプリ、ツールで共有） |
| `user/` | アプリのランタイム `t5-rt` とアプリ（nightly、静的 PIE） |
| `tools/elf2t5a/` | アプリの ELF を検査して T5A に変換するホストツール |
| `boot/` | ESP-IDF 6.0 のブートローダとパーティションテーブル（v1.x 向け、PMP のロックなし） |

## ビルドとテスト

```sh
make test-virt     # QEMU virt でセルフテスト（M1: U モードと保護テスト 9 件、M2: 4 プロセスの並行実行）。成功で終了コード 0
make image-p4      # ESP32-P4 v1.x 用のカーネル app image（build/t5-kernel-esp32p4v1.bin）
make boot          # ESP-IDF ブートローダとパーティションテーブル（要 ~/esp-idf/6.0）
```

必要なもの: rustup（stable と nightly-2026-09-10、アプリ用の `riscv32imafc-unknown-none-elf` とカーネル用の `riscv32imac-unknown-none-elf`）、`qemu-system-riscv32`、espflash 4.4 以上、ESP-IDF 6.0。

`make backup-flash` / `make flash-p4` は実機を読み書きする。flash-p4 はブートローダとパーティションテーブルを書き換え、既存のファームウェアを上書きする。
