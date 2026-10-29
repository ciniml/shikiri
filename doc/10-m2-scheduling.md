# 10. M2: タイマ割り込み、プリエンプション、sleep/yield/time、FPU の遅延切り替え

- 日付: 2026-10-30
- 対象: QEMU virt（qemu-system-riscv32 8.2.2）と M5Stack Tab5（ESP32-P4 rev v1.0、MAC 30:ed:a0:e0:cb:86）
- 結果: **両方で selftest PASS**。U モードのアプリ 4 本（ticker ×2、spin ×2）が SYSTIMER の割り込みで切り替わりながら並行に動き、FP レジスタを使い合っても内容が壊れないことを確認した。実機ログの全文は [logs/m2-tab5-20261030.txt](logs/m2-tab5-20261030.txt)。
- 参照した ESP-IDF: `~/esp-idf/6.0`（v6.0-beta1-624-g6534932340）。以下、パスは `components/` 以下を示す。

## 1. 作ったもの

### 1.1 スケジューラ（`kernel/src/sched.rs`）
- カーネルはこれまでどおり割り込み禁止（MIE=0）のまま動く（07 §4.3）。割り込みが入るのは U モード実行中だけなので、カーネル内にロックはない。
- ループは次の 3 段階を繰り返す（判断）。
  1. Ready のプロセスを 1 つ選ぶ。
  2. ワンショットのアラームを「タイムスライスの終わり（10ms）」と「眠っているプロセスの最も早い起床時刻」の早い方に合わせる。
  3. プロセスをトラップするまで走らせ、トラップの種類に応じて処理する。
  - 周期ティックは使わない。プロセスが 1 つしかなくても、アラームはその時点で必要な時刻にしか鳴らない。
- 選び方はラウンドロビン。ただし、眠りから覚めたプロセスがあれば次に回す。ラウンドロビンだけだと、起床が「忙しいプロセスの数 × スライス」だけ遅れた（最初の実装では 30ms の sleep が 40ms になった）。
- Ready のプロセスがないときは、`wfi` で待つ（`Platform::wait_for_interrupt`）。MIE=0 のままなので、トラップには入らずに `wfi` から戻るだけで、その後でアラームを解除する。
- 実行中のプロセスは `Process.state`（Ready / Sleeping { until } / Exited）で管理する。プロセスは最大 4 つ（`MAX_PROCS`）。アプリプールを 4 等分して 1 つずつ割り当てる（M3 で可変長の割り当てに置き換える予定）。

### 1.2 システムコール（ABI 0.2）
ABI_MINOR を 2 に上げた。番号は 07 §4.2 の表のとおり。

| 番号 | 名前 | 引数 → 戻り値 |
|---|---|---|
| 0x001 | `yield` | – → – |
| 0x002 | `sleep` | a0/a1 = ナノ秒（下位/上位） → –。少なくとも指定時間は眠る（tick への変換は切り上げ） |
| 0x004 | `time` | – → a1/a2 = 起動からのナノ秒（下位/上位、単調増加） |

- tick とナノ秒の変換（`kernel/src/time.rs`）は、`ticks * 1e9` のオーバーフローを避けるために秒と端数に分けて計算する。16MHz だと素朴な計算は約 19 分であふれる。
- `t5-rt` に `yield_now`、`sleep_ns`/`sleep_ms`、`time_ns` を足した。

### 1.3 FPU の遅延切り替え（`kernel/src/arch/riscv32/fpu.rs`）
- FP レジスタの持ち主（owner）は常に 1 プロセスだけにする。持ち主は `mstatus.FS=Clean` で、それ以外のプロセスは `FS=Off` で走らせる。
- 持ち主でないプロセスが FP 命令を実行すると、不正命令（mcause=2）でトラップする。その場合カーネルは次の順に処理する。
  1. 前の持ち主がレジスタを書き換えていれば（トラップ時に FS=Dirty なら）、その内容を保存する。
  2. 新しい持ち主のレジスタを読み込む。
  3. 同じ命令から再実行させる。
- 本当に不正な命令なら、FPU を渡した状態でもう一度不正命令になる。そのときにプロセスを終了させる。
- **v1.x の EXT_ILL バグの回避（判断）**: v1.x では `mext_ill` の FP_ILL ビットが FLW/FSW に対して立たない（`soc/esp32p4/include/soc/soc_caps.h:194`、01 §5.3）。ESP-IDF は mtval のオペコードをデコードしてこれを回避している。本実装は「FS=Off のときの不正命令はすべて、いったん FPU を渡して再実行する」という方式なので、`mext_ill` を見る必要がない。代わりに、本当に不正な命令ではトラップが 1 回余計に起きる。
- 実機での確認: spin が最初に実行する FP 命令は `flw` で、まさにバグの影響を受ける命令だった。この方式で正しく切り替わった（§3）。
- **カーネルを `riscv32imac` に変えた**: カーネルのコンパイラ生成コードが FP レジスタに触れると、持ち主の状態を壊してしまう。そのため F を持たないターゲットでビルドし、保存と復元だけを `.option arch, +f` のアセンブリで書いた（`kernel/.cargo/config.toml`）。アプリは引き続き `riscv32imafc`。
- コンテキストスイッチのたびに FP レジスタを保存するわけではない。FP を使わないプロセスや、読むだけのプロセスでは保存が起きない（§3 の統計で fpu_switches=4 に対して fpu_saves=2）。

### 1.4 その他
- `trap::run_user` は U モードに戻るとき `MPIE=1` にする。M1 では 0 だった。また mret の前に `mcause` を 0 にする。CLIC モードの mret は `mcause.mpil` を割り込みレベルとして復元するので、別のプロセスが割り込まれたときのレベルを持ち越さないようにしている（CLIC 仕様の mret の動作。01 §4.1）。
- `println!`（`t5-rt`）は 1 行を 128 バイトのバッファにためて、`dbg_write` を 1 回で呼ぶようにした。core::fmt は書式の断片ごとに `write_str` を呼ぶ。そのためプリエンプションが入ると、プロセス間で行が混ざった（`[spin[ticker2] tick ...`）。`dbg_write` 自体はカーネル内で割り込まれないので、1 回の呼び出しの中身は混ざらない。

## 2. プラットフォームごとのタイマ

### 2.1 ESP32-P4 v1.x（`kernel/src/platform/esp32p4v1.rs`）

| 項目 | 値 | 出典 |
|---|---|---|
| SYSTIMER のベース | 0x500E_2000 | `soc/esp32p4/register/hw_ver1/soc/reg_base.h:116` |
| カウンタの周波数 | 16MHz（unit 0、リセット時から動作） | `esp_hw_support/port/esp32p4/systimer.c:20`、`systimer_reg.h` の `TIMER_UNIT0_WORK_EN` の既定値 1 |
| クロックゲート | APB と機能クロックはリセット時に有効 | `hp_sys_clkrst_reg.h:767,2476` |
| カウンタの読み出し | `UNIT0_OP.UPDATE` → `VALUE_VALID` を待つ → lo/hi/lo を読み、lo が一致するまで繰り返す | `hal/systimer_hal.c:46-70` |
| アラーム | target 0 をワンショットで unit 0 と比較。`COMP0_LOAD` で反映し、`CONF.TARGET0_WORK_EN` で有効化 | `hal/esp32p4/include/hal/systimer_ll.h:142-190` |
| 過去の時刻を指定したとき | 即座に発火する（取りこぼし補正あり）。再試行ループは不要 | `soc_caps.h:598`、`hal/systimer_hal.c:78-87` |
| 割り込みソース番号 | `ETS_SYSTIMER_TARGET0_INTR_SOURCE` = 53 | `soc/esp32p4/include/soc/interrupts.h:72`（途中に `= 16` の明示がある）、マップレジスタ +0xD4（`interrupt_core0_reg.h:653`） |
| ルーティング | 割り込みマトリクス（0x500D_6000 + 4×ソース）に「CPU 割り込み線 + 16」を書く。本実装は線 1、つまり CLIC 番号 17 | `hal/include/hal/interrupt_clic_ll.h:41`、`soc/esp32p4/include/soc/clic_reg.h:14` |
| CLIC の設定 | 0x2080_1000 + 4×17 に 1 ワード（[31:24] ctl = 0x3F で優先度 1、[23:16] attr = 0 でレベル・非ベクタ、[8] ie = 1） | `clic_reg.h:18,64-110` |
| 閾値 | MMIO 0x2080_0008 の [31:24]（v1.x は mintthresh CSR がない）。0 を書く | `clic_reg.h:61`、01 §0 |
| mcause | 割り込みビット + コード 17（下位 12 ビットでマスクして比較） | 実機で確認 |

### 2.2 QEMU virt（`kernel/src/platform/virt.rs`）
- CLINT の `mtime`（0x0200_BFF8、10MHz）と `mtimecmp`（0x0200_4000）を使い、`mie.MTIE` を立てる。mcause は 0x8000_0007。
- RV32 では 64 ビットの比較レジスタを 2 回に分けて書く。途中の値が過去の時刻にならないよう、先に上位を最大値にしておく。

## 3. 実機（Tab5）の結果

```
m2: 4 processes, 10 ms slices
[ticker1] tick 0 slept 30019 us
[ticker2] tick 0 slept 45024 us
[ticker1] tick 1 slept 30024 us
[spin1] 6627 iterations, 5 gaps, fp registers intact
[spin2] 5490 iterations, 4 gaps, fp registers intact
...
[ticker2] tick 4 slept 45017 us
[ticker1] exit 0: ok
[ticker2] exit 0: ok
[spin1] exit 0: ok
[spin2] exit 0: ok
m2: 245 ms, preemptions=12 fpu_switches=4 fpu_saves=2 idle_waits=6
selftest: PASS
```

- M1 の 9 ケース（hello と保護テスト）も、スケジューラ経由に変えたうえで引き続き PASS。
- アプリの内容:
  - ticker: `period_ms` ずつ 5 回眠り、実際に眠った時間が指定より短ければ失敗する。
  - spin: 32 本の FP レジスタに自分の id から作った値を入れ、`time` だけを呼びながら 100ms 回る。最後にレジスタを確かめる。時刻が 2ms 以上跳んだ回数（gaps）が 0 なら、プリエンプションされなかったとして失敗する。
- カーネルの統計（preemptions、fpu_saves、idle_waits）が 0 なら、selftest を失敗させる。こうして各経路を必ず通ったことを確かめている。

### 実機で確認できた事実

| 項目 | 結果 |
|---|---|
| U モード実行中の M モード割り込み | SYSTIMER → 割り込みマトリクス → CLIC の経路で、U モードを止めて trap_entry に入る。CLIC のモード（mtvec の mode=3）で、非ベクタ割り込みは mtvec のベースに来る |
| MIE=0 での `wfi` | CLIC で有効にした割り込みが保留になると戻る（idle_waits=6 で、ハングしない） |
| `mstatus.FS=Off` | U モードの `flw`/`fsw` は不正命令（mcause=2）になる。FS=Clean にして再実行すれば続行する |
| `mstatus.FS` の Dirty | FP レジスタに書いたプロセスがトラップすると、FS=Dirty が読める |
| sleep の精度 | 指定より 17〜35µs 長い（トラップ、スケジューラ、USB への出力を含む） |
| 1 回の `time` syscall | spin の 1 周（ecall の往復と SYSTIMER の読み出し）は約 7.5µs（CPU 時間の約半分で 6627 周したことからの概算） |

- 最後の値は QEMU（同じ条件で 2〜3 万周）より遅い。CPU クロックがブートローダの設定のまま（360MHz に上げていない）である可能性が高い（推測。M4 でクロックを設定したら測り直す）。

## 4. テストの効き目の確認
- FPU の保存と復元を無効にしたカーネルで QEMU のテストを回すと、`[spin1] ... fp registers CORRUPTED` → `selftest: FAIL (2)` になった。spin のテストが切り替えの不具合を検出できることを確かめた。
- プリエンプションが起きない場合は、spin が gaps=0 で exit 2 になり、統計の preemptions=0 でも失敗する。

## 5. 残っていること
- カーネルは割り込み禁止で動く。長い syscall（M3 以降のフラッシュ読み出しなど）は、プロセスをブロックさせて分割する必要がある（07 §4.3）。
- PMA の状態は未確認のまま（09 の残件）。
- Core1 は使っていない。
- 割り込みの入れ子、つまり割り込み中の割り込みは扱っていない（カーネル内で MIE=0 なので起きない）。
- エミュレータ（案 B）に入れるべき実機の挙動: SYSTIMER の取りこぼし補正、CLIC の mcause の上位ビット、MIE=0 での wfi の復帰、FS=Off のときの FLW/FSW のトラップ。
