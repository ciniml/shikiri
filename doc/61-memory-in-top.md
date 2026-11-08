# 61. top のメモリ表示

利用者の指示（2026-11-08）:「topでメモリ使用量出せますか？」「RAMは内蔵とPSRAM分けて出せますか？」

## 1. 見せるもの
```
PSRAM 32768 KB: apps 258 of 24576 (12 x 2048), shm 4096, fb 4096
SRAM 640 KB: kernel 240, stacks 192, net rings 128, other 79

  PID NAME             STATE        CPU%    CPU s  RAM KB  SLOT%
  101 term             waiting       9.9      0.4     186      9
```
（エミュレータ、`term` と `shell` だけのとき。単位は KB）

- **PSRAM の行**: PSRAM 全体、アプリの領域（プロセスの使う量の合計 / 領域、スロットの数 × 1 つの大きさ）、共有メモリ（doc/19）、表示用（doc/17、doc/60 の 2 枚）。PSRAM がなければ `pool` の行で、アプリの領域は内蔵 RAM。
- **SRAM の行**（内蔵の L2MEM のうち、カーネルが割り付ける所: v1.x は 0x4FF0_0000〜0x4FFA_0000 の 640 KB、v3.x は 0x4FF2_0000 からの 512 KB）: カーネルの像（RAM で動くコード、data、hot のコードと bss）、スタック（2 つのコア）、ネットワークのリング（PSRAM があるときの元のアプリの領域、doc/35 §5）、その他（ROM の取り分、ブートローダの通り道、すき間）。カーネルにヒープはないので、起動の後は変わらない。
- **プロセスの列**: `RAM KB` = RAM にあるテキスト（XIP なら 0）＋ data、bss、スタック。`SLOT%` は自分のスロット（1 プロセスの上限）のうちどれだけか。`ps` の `RAM KB` も同じ数にした（前は data、bss、スタックだけ）。

## 2. ABI（小版 24）
- **`proc:mem`**: `read` で `MEM_INFO`（40）バイト、u32 が 10 個: アプリの領域、スロット、スロットの数、共有メモリ、表示、PSRAM、内蔵 RAM、カーネルの像、スタック、リング。起動のときに決まる（`procinfo::set_mem`）。強い許可ではない。`shell` に `--permit proc:mem` を足した。
- **`proc:list` の記録**: 空いていた u16（バイト 6〜7）に、RAM にあるテキストの KiB（切り上げ、XIP は 0）。
- **`display::FRAMES`**（ctl 5）: 0 でその手の `FRAME` の事象を止める。F4（doc/60 §10）で `term`、`mtdash`、`paint` が `display` も待つようにしたら毎フレーム起こされ、エミュレータで `term` が 1 コアの 17% を使っていた。3 つとも `FRAMES` 0 にした（`top` が毎秒描く分を除き、ほぼ 0 に戻る）。
- カーネルの像の大きさは、リンカの記号（`__ram_text_start`、`__stack_bottom`、新しい `__hot_start`、`__bss_end`）から（`arch::riscv32::kernel_ram`）。内蔵 RAM の大きさは `Platform::INTERNAL_RAM`（virt は 0 で、SRAM の行を出さない）。

## 3. 試験
- p4emu-run-31: `term` で `top`、最初の画面の数を `shell.top`（used_kb、pool_kb、slot_kb）で記録する。エミュレータでは used_kb=257、pool_kb=24576、slot_kb=2048。
