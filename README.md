# tab5-os

M5Stack Tab5（ESP32-P4 rev v1.x）向けの Rust 製の自作 OS（開発中）。カーネルは M モード、アプリは U モードで動かし、PMP で分離する。

- 設計と調査: [doc/](doc/README.md)（結論は [doc/07](doc/07-architecture-proposal.md)）
- 進捗: M0（雛形、静的 PIE、ブートローダ）、M1-virt（QEMU virt）、M1a/M1b（実機の Tab5 で U モードの hello と保護テスト 7 件。記録は [doc/09](doc/09-bringup-m1.md)）、**M2（タイマ割り込みによるプリエンプション、sleep/yield/time、FPU の遅延切り替え。QEMU と実機で PASS。記録は [doc/10](doc/10-m2-scheduling.md)）**、エミュレータ t5emu 初版（実機用カーネルが無改造で PASS。[doc/11](doc/11-emulator.md)）、**M3（署名した T5A v1 をフラッシュのアプリストアから検証して起動。RTT のコマンドで再起動なしに起動・停止。ホストから送ったアプリをカーネルが検証してフラッシュに書き込む。ハンドルとタイマ（open/ctl/wait/close）。[doc/14](doc/14-m3-design.md)）**、**構造化ログ L1〜L3（テキストログを廃止。USB と RTT（probe-rs）で回収。lock-up の後も次の起動で回収できる。[doc/12](doc/12-structured-logging.md)）**

## 構成

| ディレクトリ | 内容 |
|---|---|
| `kernel/` | M モードカーネル（stable Rust）。`arch/riscv32` は共通、`platform/{virt,esp32p4v1}` は機種ごと |
| `abi/` | syscall 番号、エラーコード、T5A イメージ形式（カーネル、アプリ、ツールで共有） |
| `user/` | アプリのランタイム `t5-rt` とアプリ（nightly、静的 PIE） |
| `tools/elf2t5a/` | アプリの ELF を検査して T5A に変換するホストツール |
| `tools/t5log/` | 構造化ログのスキーマを集めて検査し、ログを JSON Lines か 1 行表示に解読するホストツール（doc/12） |
| `boot/` | ESP-IDF 6.0 のブートローダとパーティションテーブル（v1.x 向け、PMP のロックなし） |
| `emu/` | ESP32-P4 rev v1.x のエミュレータ t5emu（ホスト用、依存なし。doc/11） |

## ビルドとテスト

```sh
make test-p4emu    # 実機用カーネルを自作エミュレータ t5emu で実行（doc/11）。成功で終了コード 0
make monitor PORT=/dev/ttyACM3   # 実機のログを USB から解読して表示（doc/12）
make flash-store PORT=/dev/ttyACM3   # アプリストア（appmeta/apps）を実機に書く（doc/14 §2）
make rtt PROBE=303a:1001:<MAC> [RESET=1]   # 実機のログを RTT で読み、その場で解読（t5log が probe-rs をライブラリとして使う。doc/12 §8）
                                            # 標準入力から list / ps / run NAME [A0 [A1]] / kill PID を送れる（doc/14 §3）
make rtt PROBE=... CMD="run ticker 1 200"    # コマンドを 1 つ送り、結果を見て終わる
make probe-rs      # （任意）probe-rs の CLI をコミット固定で build/probe-rs に入れる（doc/13）
make test-emu      # t5emu の単体テスト
make test-virt     # QEMU virt でセルフテスト（M1: U モードと保護テスト 10 件、M2: 4 プロセスの並行実行）。ログを解読して表示し、成功で終了コード 0
make image-p4      # ESP32-P4 v1.x 用のカーネル app image（build/t5-kernel-esp32p4v1.bin）
make boot          # ESP-IDF ブートローダとパーティションテーブル（要 ~/esp-idf/6.0）
```

アプリは署名が必要（doc/14 §1.3）。最初に一度だけ `make keys` で開発用の鍵を作る（秘密鍵は `~/.config/tab5-os/`、公開鍵は `keys/`）。

必要なもの: rustup（stable と nightly-2026-09-10、アプリ用の `riscv32imafc-unknown-none-elf` とカーネル用の `riscv32imac-unknown-none-elf`）、`qemu-system-riscv32`、espflash 4.4 以上、ESP-IDF 6.0。

`make backup-flash` / `make flash-p4` は実機を読み書きする。flash-p4 はブートローダとパーティションテーブルを書き換え、既存のファームウェアを上書きする。
