# 12. 構造化ログサービスの設計案

- 日付: 2026-10-30
- 状態: **設計案（実装前。方針の確認待ち）**
- 前提となるユーザー方針（2026-10-30）: **OS はテキストログを提供せず、構造化ログだけを提供する。** RTT や defmt のような、ログを記録して回収する仕組みを最初から持つ。
- 事実には出典を付け、推測には「（推測）」、設計判断には「（判断）」と書く。

## 0. 要約

1. **ログの単位は「イベント」にする。** イベントは名前（`sched.preempt` など）、レベル、名前付きで型を持つフィールドからなる。書式付きの文字列は作らない。
2. **スキーマ（名前、レベル、フィールド名、ソースの位置）は ELF のロードされないセクションに置く。** レコードには、スキーマの 32 ビットのハッシュ ID とバイナリの値だけを入れる。defmt の「文字列を ELF に追い出す」考え方を借りるが、ID をハッシュにすることで、静的 PIE のアプリでもアドレスの問題が起きない（§2.2）。
3. **カーネルは、時刻、ソース（カーネルかプロセスか）、通し番号を自分で付ける。** アプリはこれらを偽れない。
4. **リングバッファは LP SRAM（0x5010_8000〜）に置く。** LP SRAM は非キャッシュの領域で、リセット後も内容が残る（推測、§1.3）。デバッガからも読める。
5. 出力先は 3 つ: (a) USB Serial/JTAG（COBS のフレーム）、(b) RTT（OpenOCD で回収）、(c) 次回の起動時に回収する「前回の起動のログ」。
6. ホストツール `t5log` が ELF 群（カーネルとアプリ）からスキーマ表を作り、JSON Lines に解読する。人が読むための表示もそこから作る。t5emu も同じ解読器を使い、`selftest.result` イベントで合否を判定する。
7. `kprintln!`、`dbg_write` syscall（0xF00）、`t5-rt` の `println!` は廃止する。

## 1. 調べた事実

### 1.1 defmt の仕組み（参考にする部分）
- defmt は、ログの呼び出しごとに 1 バイトの `static` を作り、`.defmt.<シンボル名>` というセクションに置く。シンボル名には書式文字列などのデータを埋め込み、**そのシンボルのアドレスを ID として使う**（`defmt-macros-1.2.0/src/construct.rs:78-110` の `linker_section` と `static_variable`）。
- ワイヤ形式のフレーミングは、既定で rzCOBS（`defmt-1.2.0/src/encoding/mod.rs:1-5`、`encoding/rzcobs.rs`）。
- ロガーはバイナリごとに 1 つ（`#[global_logger]`、`defmt-1.2.0/src/traits.rs:89-96`）。
- **そのまま使わない理由（判断）**:
  - (1) ID がアドレスなので、静的 PIE のアプリ（04）では、`&SYM` が実行時のアドレス（ロード先 + オフセット）になってしまう。ロード先は毎回変わる。
  - (2) 書式文字列は位置で引数を参照し、フィールド名を持たない。構造化ログにするには、文字列から名前を推測する規約が必要になる。
  - (3) カーネルと複数のアプリが 1 本のストリームを共有するので、「1 バイナリに 1 ロガー」というモデルに合わない。

### 1.2 デバッガ経由の回収（RTT）
- Espressif の OpenOCD フォークは P4 に対応している。
  - P4 の対応が入ったのは v0.12.0-esp32-20230921（https://github.com/espressif/openocd-esp32/releases/tag/v0.12.0-esp32-20230921 。リリースノートでは「flash 未対応の基本的なデバッグ」）。
  - ローカルにある最新版 `~/.espressif/tools/openocd-esp32/v0.12.0-esp32-20260424` には `board/esp32p4-builtin.cfg`（内蔵の USB-JTAG を使う設定）がある。
  - `help` の出力に、`rtt server start <port> <channel>` などの RTT コマンドがある（2026-10-30 にローカルで確認）。
  - NuttX の P4 の手順も、OpenOCD と内蔵 USB-JTAG を使っている（https://nuttx.incubator.apache.org/docs/latest/platforms/risc-v/esp32p4/index.html ）。
- probe-rs は P4 を挙げていない。ローカルの 0.27.0 の `chip list` は esp32、c2、c3、c6、h2、s2、s3 だけ。Espressif の Rust book も、USB-JTAG を使える例に P4 を挙げていない（https://docs.espressif.com/projects/rust/book/getting-started/tooling/probe-rs.html ）。**当面は OpenOCD を使う**（判断）。
- **キャッシュの問題**: L2MEM は書き戻し方式の L1 D キャッシュの後ろにある（09 §2）。デバッガがシステムバスで読むと、キャッシュに残ったログを読み損なうおそれがある（推測）。
  - 非キャッシュの別名 `SOC_NON_CACHEABLE_OFFSET = 0x4000_0000`（`soc/esp32p4/include/soc/ext_mem_defs.h:68`、`hal/esp32p4/include/hal/cache_ll.h:27`。DMA の記述子に使われている）を使う手がある。
  - ただし、次の §1.3 の LP SRAM は周辺の空間（0x5000_0000〜）にあり、もともとキャッシュされない（推測）。そのため本案では LP SRAM を使う。

### 1.3 LP SRAM（リセット後も内容が残る置き場所）
- アドレス範囲: `SOC_LP_RAM_LOW/HIGH = 0x5010_8000 / 0x5011_0000`（32KiB、`soc/esp32p4/include/soc/soc.h:176-177`）。
- 上端の `0x5010_FA80` から上は、LP ROM が使う（LP ROM のスタックを含む。`esp_system/ld/esp32p4/memory.ld.in:43-45`）。**使える範囲は 0x5010_8000〜0x5010_FA80（31,360 バイト）**。
- ESP-IDF は、ここに `RTC_NOINIT_ATTR`（「再起動しても値を保つ」。`esp_common/include/esp_attr.h:97-99`）を置く。
- ブートローダは、`CONFIG_BOOTLOADER_RESERVE_RTC_MEM` が有効なときだけ先頭を使う（`bootloader_support/src/bootloader_common_loader.c:261-277`）。本プロジェクトのブートローダは `CONFIG_BOOTLOADER_RESERVE_RTC_SIZE 0x0` で、使わない。
- U モードからは PMP で見えない（周辺の空間なので、どのエントリにも当たらない）。
- **未確認**（いずれも推測）:
  - RWDT によるリセット、ソフトウェアリセット、lock-up 後のリセットで内容が実際に残るか。
  - 1 段目の ROM が触らないか。
  - デバッガから読めるか。

## 2. 設計

### 2.1 ログを出す側の API（Rust、カーネルとアプリで共通）

```rust
// 例
event!(Info, "sched.preempt", pid = p.pid as u8, pc = frame.pc as u32);
event!(Warn, "loader.reject", app = name, reason = "abi_mismatch", major = h.abi_major);
event!(Error, "kernel.panic", file = loc.file(), line = loc.line(), msg = &*buf);
```

- `event!(レベル, "名前", フィールド名 = 値, ...)`。名前はドット区切りの小文字。名前空間の先頭はサブシステムにする（`sched.`、`loader.`、`proc.`、`selftest.`、アプリは `app.` 以下を自由に使う。判断）。
- 必要なら、人が読むための文（テンプレート）も付けられる: `event!(Info, "proc.exit", msg = "{name} exited with {code}", ...)`。これは表示のためだけの付加情報で、レコードには入らない。
- レベルは Error、Warn、Info、Debug、Trace の 5 つ。コンパイル時の上限（feature）と、実行時のしきい値（カーネルが管理）の 2 段で絞る。
- フィールドに使える型:

  | タグ | 型 | 備考 |
  |---|---|---|
  | 0x01–0x04 | u8、u16、u32、u64 | リトルエンディアンの固定長 |
  | 0x05–0x08 | i8、i16、i32、i64 | 同上 |
  | 0x09 | bool | 1 バイト |
  | 0x0A | f32 | IEEE 754 |
  | 0x0B | str | varint の長さ + UTF-8。上限 128 バイト（超えた分は切り詰め、印を付ける） |
  | 0x0C | bytes | varint の長さ + 値。上限 128 バイト |
  | 0x0D | addr | u32 だが「アドレス」という意味を持たせる（表示は 16 進） |

- 型は、`Into<FieldValue>` のようなトレイトで値から自動で決める。フィールドの数は 1 イベントにつき 16 まで（判断）。

### 2.2 スキーマとその ID
- `event!` は `macro_rules!` で実装し、手続きマクロは使わない（判断。ビルドを軽くし、カーネルの stable ツールチェーンでも使えるようにする）。
- 各呼び出しで、次のスキーマ文字列を `concat!` でコンパイル時に作る:
  `{"v":1,"name":"sched.preempt","level":"info","fields":["pid","pc"],"msg":null,"file":"kernel/src/sched.rs","line":123}`
- **ID はスキーマ文字列の FNV-1a 32 ビットハッシュにする。** const fn で計算する。アドレスを使わないので、カーネル（絶対番地）と静的 PIE のアプリ（ロード先が変わる）で同じ扱いにできる。
- スキーマ文字列そのものは、`#[used] #[link_section = ".t5log"] static` に置く。リンカスクリプトでこのセクションを `(INFO)`、つまり非ロードにする。デバイスのメモリは使わない。
  - カーネル: `kernel-common.ld` に足す。
  - アプリ: `t5app.ld` に足す。`elf2t5a` は T5A に入れない（判断）。
- ホストツールは ELF の `.t5log` を読み、`ID → スキーマ` の表を作る。
  - 異なる内容が同じ ID になる衝突は、ビルド時（`elf2t5a` と `t5log check`）にエラーにする。
  - 同じ内容は同じ ID になる。

### 2.3 レコードの形式（ワイヤ形式 v1）

```
record :=
  u8      header      bit7..5 = level (0=error .. 4=trace), bit4..0 = フィールド数
  u32     schema_id   FNV-1a 32 of the schema text (LE)
  varint  seq         リングごとの通し番号（欠落の検出用）
  varint  t_us        起動からのマイクロ秒
  varint  source      0 = カーネル、1.. = プロセス ID
  field*  fields      (u8 tag, value)
```

- `seq`、`t_us`、`source` は**カーネルが付ける**。アプリが `log_emit` で渡すのは、header、schema_id、fields だけ（§2.5）。
- フィールドのタグをレコードに入れるので、スキーマの表が手に入らなくても値だけは読める（判断）。
- 起動ごとに、最初のレコードとして `log.boot { boot_id: u32, reset_reason: u8, schema_ver: u8, kernel_build: u32 }` を出す。
- プロセスの開始時には `proc.start { pid, name: str, app_build: u32 }` を出す。解読する側は、このイベントでソースとアプリを結び付ける。

### 2.4 カーネルのリングと出力
- **LP SRAM に 1 本のリングを置く**（例: 24KiB。上限は §1.3 の 31,360 バイト）。
  - 先頭にヘッダを置く: マジック、版、boot_id、書き込み位置、wrap 回数、CRC。
  - 古いものから上書きする（判断。クラッシュの直前のログを残すことを優先する）。
- 出力先（drain）は、カーネルのループの空き時間と、syscall から戻るときに進める。カーネルは割り込み禁止で動くので、ロックは要らない。
  1. **USB Serial/JTAG**: レコードを COBS で包み、0x00 で区切って流す。FIFO が満杯なら待たずに次に回し、取りこぼしは `log.dropped { sink, count }` として後で出す。
     - ブートローダのテキスト（0x00 を含まない）とは区切りで見分けられる。ホスト側は、フレームでない部分を「ブートローダの出力」として別に表示する。
  2. **RTT**: SEGGER RTT 互換の制御ブロック（`"SEGGER RTT"`）を LP SRAM に置く。up チャネル 0 を、同じバイト列（COBS フレーム）のミラーにする。OpenOCD の `rtt setup 0x50108000 0x7A80 "SEGGER RTT"` → `rtt server start <port> 0` で TCP から読む。
     - 満杯のときは「書かずに数える」モード（RTT の NO_BLOCK_SKIP 相当）にする。
  3. **前回の起動のログ**: 起動時にリングのヘッダが有効なら（マジックと CRC が合い、boot_id が前回のもの）、その中身を `log.prev_boot` で始まる区切りに続けて 1 回だけ流し直す。その後、新しい boot_id でリングを初期化する。
     - RWDT のリセットや lock-up（11 §4）の直前の記録を、次の起動で回収するための仕組み。
- QEMU virt には LP SRAM がない。そのためリングはカーネルの RAM に置き、出力先は UART だけにする。`Platform::LOG_RING` で切り替える。

### 2.5 アプリからのログ（syscall）

| 番号 | 名前 | 引数 → 戻り値 |
|---|---|---|
| 0x005 | `log_emit` | buf, len → –。buf は header、schema_id、fields の並び（最大 256 バイト） |
| 0x006 | `log_level` | – → 現在の実行時しきい値（アプリ側で、出さないログを早めに捨てるため） |

- カーネルは、長さ、ヘッダのフィールド数、各タグの範囲と長さを検査する。そのうえで seq、t_us、source を付けてリングに入れる。スキーマの中身はカーネルにとって意味を持たない。
- プロセスごとに、レート制限（トークンバケツ。例: 平均 200 件/秒、バースト 64 件）をかける。超えた分は捨てて、`log.dropped { source, count }` にまとめる（判断。ログでカーネルが詰まらないようにする）。
- ABI を 0.3 にする。`dbg_write`（0xF00）は 0.3 で削除する。ABI は 1.0 より前なので、互換は考えない。

### 2.6 パニックと異常時
- **カーネルの panic**: `kernel.panic { file, line, msg: str }`。`msg` は core::fmt で 128 バイトの固定バッファに書き出して作る。値としての文字列なので、構造化ログの範囲内である。リングに直接書き、USB への出力を同期で流しきってから止まる。
- **アプリの panic**: `t5-rt` の panic ハンドラが `app.panic { file, line, msg }` を出してから終了する。
- **プロセスの異常終了**: `proc.fault { pid, cause, pc: addr, tval: addr }`。
- **lock-up（P4）**: カーネルはもう何も書けない。対策は §2.4 の 3 と、RWDT を止めずにカーネルのループで餌をやる運用（別途。11 §4）。

### 2.7 ホストツール `t5log`（`tools/t5log`）
- `t5log decode --elf build/t5-kernel-esp32p4v1.elf --elf user/target/.../hello ... [--port /dev/ttyACM3 | --tcp localhost:9090 | FILE]`
  - 出力は既定で JSON Lines: `{"t_us":1234,"seq":10,"src":"kernel","level":"info","event":"sched.preempt","pid":2,"pc":"0x4ff40066"}`
  - `--pretty` で、人が読むための 1 行表示にする（テンプレートがあればそれを使う）。
- `t5log check ELF...`: スキーマの衝突と、名前の規約を検査する。`make apps` と `make kernel-*` から呼ぶ。
- 解読の部分は、ライブラリのクレートとして t5emu と共有する。

### 2.8 t5emu との連携
- LP SRAM（0x5010_8000〜）を、メモリとして模すようにする（今は汎用レジスタ扱い）。
- RTT の制御ブロックを RAM から直接読んでレコードを解読し、標準出力に JSON Lines か pretty で出す。
- 合否は `selftest.result { failures: u32 }` イベントで判定し、テキストの照合（11 §1）はやめる。
- 「前回の起動のログ」は、エミュレータでリセットを模すようにしてから試験する（L3）。

### 2.9 廃止するもの
- `kprintln!`、`platform::Console`: `event!` に置き換える。ブート時の診断（misa、PMP のダンプ）も `boot.cpu { misa, mstatus, mtvec }`、`boot.pmp { index, cfg, addr }` のようなイベントにする。
- `dbg_write` syscall と、`t5-rt` の `println!`、`DebugConsole`。
- セルフテストの表示（`[hello] exit 0: ok` など）は、`selftest.case { name, expected, got, ok }` と `selftest.result { failures }` にする。

## 3. マイルストーン（案）

| 段階 | 内容 | 完了条件 |
|---|---|---|
| L1 | ワイヤ形式（`abi` に定数）、`event!`（カーネルと t5-rt）、カーネルのリングと USB/UART への出力、`log_emit`/`log_level`（ABI 0.3）、`t5log decode/check`、テキスト出力の削除、t5emu をイベントで判定 | QEMU virt、t5emu、実機で、selftest の結果を `t5log` が JSON Lines で出す。テキストの出力経路がコードベースに残っていない |
| L2 | RTT の制御ブロック、OpenOCD での回収手順（`make rtt`）、t5emu からの RTT の読み出し | 実機で、USB の CDC を開かずに、OpenOCD の RTT からログを回収できる |
| L3 | LP SRAM のリングをリセット後に残し、次の起動で回収する。リセット理由の取得、RWDT に餌をやる運用 | カーネルの不具合で lock-up → RWDT のリセット → 次の起動で直前のログを回収、を実機と t5emu で実演する |

- L1 の後に M3（flash からのアプリのロード）へ進む。M3 の T5A の TLV にアプリのビルド ID を入れ、`proc.start` と結び付ける。

## 4. 確認したいこと（実装前）

1. **全体の方針**: イベント名 + 名前付きの型付きフィールド、ハッシュの ID、ワイヤ形式 v1（§2.1〜§2.3）で進めてよいか。
2. **人が読むためのテンプレート（`msg`）**: 任意の付加情報として残してよいか。完全に無くす（表示はフィールドを並べるだけ）案もある。
3. **実機での確認の許可**（L2/L3 で必要）:
   - OpenOCD で内蔵の USB-JTAG につなぐ。CPU を止める、メモリを読む、リセットする、という操作があり得る。フラッシュは書かない。
   - LP SRAM の内容がリセット後に残るかの試験。カーネルを書き換え、RWDT のリセットとソフトウェアリセットを起こす。
