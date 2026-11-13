<img src="doc/logo/shikiri.svg" alt="shikiri" width="330">

# Shikiri（仕切り）

権限を分け、メモリ保護（PMP/MPU）でアプリを仕切る、Rust 製の自作 OS（開発中）。カーネルは M モード、アプリは U モードで動かし、PMP で分離する。今は M5Stack Tab5（ESP32-P4 rev v1.x）向けで、同じ構成（特権の分離とメモリ保護）をとれるほかのハードウェアにも移していく予定。

## 特徴

- **アプリを仕切る**: カーネルは M モード、アプリは U モードで動く。アプリごとにメモリの領域を分け、PMP（RISC-V のメモリ保護）でほかの領域に触れさせない（[doc/01](doc/01-cpu-privilege-and-protection.md)、[doc/07](doc/07-architecture-proposal.md)）。MMU のない MCU 向け。
- **署名した許可だけが効く**: アプリは静的 PIE で、署名した形式（T5A）でフラッシュのストアに入る。使ってよいもの（`svc:net/udp:5540`、`display` など）は、署名するときに許可として埋め込む。書いていないものは拒否する。サービスは、接続してきた相手の身元と許可を見て答える。許可は後から手放せる（[doc/14](doc/14-m3-design.md)、[doc/52](doc/52-access-control.md)、[doc/56](doc/56-limits-restart-drop.md)）。
- **サービスはユーザ空間**: TCP/IP（フィルタ、NAT、IPv6）、ファイルシステム、Wi-Fi、BLE、Matter、設定、Tailscale は、どれも普通のアプリとして U モードで動く。アプリとサービスは、チャネルと共有メモリで話す（[doc/19](doc/19-n0-ipc-design.md)）。
- **落ちても立て直す**: 常駐のサービスが落ちると、カーネルが起動し直す。プロセスごとにチャネルの数の上限がある（[doc/56](doc/56-limits-restart-drop.md)）。
- **安全に更新する**: アプリはストアに入れるときに署名を確かめる。カーネルは A/B で更新し、起動できなければ前の版に戻る。tailnet 越しの OTA にも対応する（[doc/16](doc/16-m5-update-design.md)、[doc/67](doc/67-ota.md)）。
- **構造化ログだけ**: テキストのログはない。`event!` の記録をホストのツール（`t5log`）がスキーマで解読する。固まった後も、次の起動で前のログを回収できる（[doc/12](doc/12-structured-logging.md)）。
- **2 コアで動く**: カーネルは大きな鍵 1 つの SMP（[doc/33](doc/33-smp.md)）。
- **自作のエミュレータで試す**: 実機用のカーネルを、そのまま自作のエミュレータ（`t5emu`）で動かして試せる。QEMU virt でも動く（[doc/11](doc/11-emulator.md)）。

### 分類
マイクロカーネルとモノリシックの中間（ハイブリッド）。サービスを U モードに出してメッセージで話す点は、マイクロカーネル（QNX、Xous）に近い。一方、デバイスドライバ（表示、SD、SDIO の Wi-Fi、音声、I2C、キーボード、Ethernet、フラッシュ）はカーネルの中にある。この点は Tock と同じ作り。PMP の領域は 1 プロセスに数個しかないので、ドライバまでユーザ空間に出すと足りなくなる（[doc/03](doc/03-rust-ecosystem.md)）。

> **名前について**: 元は Tab5 向けの「tab5-os」だった。そのため、crate やツール、形式の名前に `t5` の接頭辞がまだ残っている（`t5_abi`、`t5_rt`、`t5log`、`elf2t5a`、アプリの `.t5a`、カーネルの `.t5k`、SD の `T5OS/`、エミュレータの `t5emu` など）。ドキュメントの「tab5-os」も同じ。移植に合わせて改める。

ロゴは障子の格子（[doc/logo/](doc/logo/)）。組子で仕切られた部屋のうち 1 つに明かりが灯り、その明かりは桟から離れている。自分の領域に閉じ込められたアプリを表す。

- 設計と調査: [doc/](doc/README.md)（結論は [doc/07](doc/07-architecture-proposal.md)）
- 進捗: M0（雛形、静的 PIE、ブートローダ）、M1-virt（QEMU virt）、M1a/M1b（実機の Tab5 で U モードの hello と保護テスト 7 件。記録は [doc/09](doc/09-bringup-m1.md)）、**M2（タイマ割り込みによるプリエンプション、sleep/yield/time、FPU の遅延切り替え。QEMU と実機で PASS。記録は [doc/10](doc/10-m2-scheduling.md)）**、エミュレータ t5emu 初版（実機用カーネルが無改造で PASS。[doc/11](doc/11-emulator.md)）、**M3（署名した T5A v1 をフラッシュのアプリストアから検証して起動。RTT のコマンドで再起動なしに起動・停止。ホストから送ったアプリをカーネルが検証してフラッシュに書き込む。ハンドルとタイマ（open/ctl/wait/close）。[doc/14](doc/14-m3-design.md)）**、**M4a（32MB の PSRAM を初期化し、アプリを PSRAM で動かす。[doc/research/m4-psram-and-clock.md](doc/research/m4-psram-and-clock.md)）**、**M4b（アプリの text をフラッシュから直接実行し、data を PSRAM に置く。[doc/15](doc/15-m4b-xip.md)）**、**M4c（電圧の自動調整 PVT を移植し、CPU を 360MHz に。[doc/research/m4-psram-and-clock.md §10](doc/research/m4-psram-and-clock.md)）**、**構造化ログ L1〜L3（テキストログを廃止。USB と RTT（probe-rs）で回収。lock-up の後も次の起動で回収できる。[doc/12](doc/12-structured-logging.md)）**

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
