# 12. 構造化ログサービスの設計案

- 日付: 2026-10-30
- 状態: 設計はユーザーの確認済み（2026-10-30）。**L1〜L3 を実装し、QEMU virt、t5emu、実機で動作を確認した（§5〜§7）。**
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
- ~~probe-rs は P4 を挙げていない~~ → **訂正（13）**: ローカルの 0.27.0 にないだけで、上流の probe-rs は 0.31.0（2026-01-17）から P4（v1.x を含む）に対応している（PR #3649）。ただし、v1.x では L2MEM をプログラムバッファ経由で読む（読むたびに core0 が止まる）。また、上流のメモリマップに LP SRAM がない。RTT の回収に OpenOCD と probe-rs のどちらを使うかは、L2 で 13 の段階 1 の結果を見て決める（判断）。
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
- 各呼び出しで、次のスキーマ文字列を `concat!` でコンパイル時に作る（実装では JSON ではなく、行単位の `key=value` にした。`macro_rules!` の繰り返しでは、JSON の配列の区切りのカンマをうまく作れないため）:
  ```
  t5log/1
  name=sched.preempt
  level=Info
  file=kernel/src/sched.rs
  line=123
  fields=pid,pc,
  msg=
  （空行で終わる）
  ```
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

## 5. L1 の実装記録（2026-10-30）

### 5.1 実装したもの
| 場所 | 内容 |
|---|---|
| `abi/src/log.rs` | ワイヤ形式 v1（タグ、varint、COBS、FNV-1a）、`Payload`、`Field` トレイト（整数、bool、f32、str、[u8]、`Addr`）、`event!` マクロ、`FmtBuf`（パニックのメッセージ用）、単体テスト |
| `kernel/src/log.rs` | `t5log_emit`（カーネルの出力先）、`record`（検証、seq/t_us/source の付与、COBS、リング、コンソール）。リングは P4 では LP SRAM の先頭 24KiB、virt ではカーネルの RAM |
| `kernel/src/process.rs` | `log_emit`（0x005。範囲の検査、取り込み、検証。拒否したら `log.rejected` を出して `-Invalid` を返す）、`log_level`（0x006） |
| `user/rt` | `t5log_emit` = `log_emit` syscall、`log_level()`、`raw_syscall`（試験用）。パニックは `app.panic` |
| `tools/t5log` | `schemas`（ELF から集めて検査し、ファイルに書く）、`check`、`decode`（JSON Lines か `--pretty`、ファイル、標準入力、TCP） |
| `emu/` | LP SRAM を模す。コンソールのフレームを解読して表示し、`selftest.result` の `pass=true` で合否を判定する |
| `Makefile` | `schemas-virt`、`schemas-p4`、`test-virt`（QEMU の出力をファイルに取ってから解読）、`run-virt`、`monitor`（`stty raw` して `t5log decode --pretty`） |

- ABI は 0.3 にした。`dbg_write`（0xF00）、`kprintln!`、`platform::Console`、`t5-rt` の `println!`、`DebugConsole` は削除した。テキストの出力経路は、Rust のコードには残っていない（ホストのツールの標準出力を除く）。
- カーネルとアプリのイベントは、合わせて 25 種類（`build/t5log.schemas`）。
- セルフテストに `fault/bad-log` を足した。形の壊れたペイロード 2 種は `Invalid`、大きすぎるものは `Size`、カーネルのアドレスは `BadAddress` で拒否されることを確かめる。

### 5.2 確認したこと
- QEMU virt（`make test-virt`）、t5emu（`make test-p4emu`）、Tab5 の実機の 3 つで `selftest.result failures=0 pass=true` を確認した。実機のログ（解読後）は [logs/l1-tab5-20261030.txt](logs/l1-tab5-20261030.txt)。
- 実機では、ブートローダのテキスト（`| ` 付きで表示）とカーネルのイベントが正しく分かれた。解読できないフレームは 0 件だった。カーネルは最初に 0x00 を 1 バイト出して、それまでの出力を区切る。
- アプリのイベントの `src` は、カーネルが付けた `pid1` などになる（アプリは偽れない）。
- 実機の `boot.start` の時刻は 0.052s。SYSTIMER はチップのリセットから数えているので、ブートローダの時間を含む。

### 5.3 設計からの変更と、残っていること
- スキーマの書式は JSON ではなく行単位にした（§2.2）。
- 予約されたフィールド名は `seq`、`t_us`、`src`、`level`、`event` だけにした。`id` は、スキーマが不明なときにしか JSON のキーに使わないので外した。
- 未実装:
  - 出力先を非同期に進めること（今は `event!` のたびに同期で USB に書く）。
  - プロセスごとのレート制限。
  - 実行時のしきい値の変更（`log::KEEP` は定数で Trace）。
  - `log.dropped`。
  - リングのヘッダ（L3）。

## 6. L2 の実装記録（2026-10-30）: RTT を probe-rs で回収する

ユーザーの判断（2026-10-30）で、回収には OpenOCD ではなく probe-rs を使うことにした（13）。

### 6.1 カーネル側
- `.log_mem` セクションを足した。NOLOAD で、P4 では LP SRAM（`link-esp32p4v1.ld` の `LP_SRAM`、0x5010_8000、長さ 0x7A80）に、virt では RAM に置く。
  - 中身は、ログのリング（16KiB）、RTT の制御ブロック `_SEGGER_RTT`、up チャネル 0（`t5log`、8KiB）、down チャネル 0（16B、未使用）。
  - app image のセグメント数は 3 のまま変わらない（NOLOAD はブートローダの対象外。`esptool image_info` で確認）。
- 制御ブロックは、SEGGER_RTT.h の形（16 バイトの id、MaxNumUp/DownBuffers、24 バイトのバッファ記述子）にした。
  - `init` で組み立てる。id は最後に書く（作りかけの制御ブロックをデバッガに見つけさせないため）。
- 書き込みは NO_BLOCK_SKIP 相当にした。フレームが丸ごと入らなければ書かずに数え、次に書けたときに `log.dropped { sink: "rtt", count }` を出す。
  - probe-rs は既定で、チャネルを「満杯なら待つ」モードにしようとする（`flags` に書く）。しかし、カーネルは `flags` を見ないので、カーネルが止まることはない（判断）。
- RTT のチャネルに流すのは、USB に流すのとまったく同じ COBS のフレーム。違いは、USB にだけ最初に区切りの 0x00 があることだけ。

### 6.2 ホスト側
- `make probe-rs`: probe-rs を、13 で読んだコミット `dd4909231da2` に固定して `build/probe-rs` に入れる（全体にインストールされている 0.27 は置き換えない）。
- `tools/probe-rs/esp32p4-tab5.yaml`: 上流の `esp32p4.yaml` に LP SRAM（0x5010_8000〜0x5010_FA80）を足した独自のチップ記述。
  - 変種の名前は `esp32p4` のままにした。probe-rs は、名前が `esp32p4` と完全に一致するときだけ P4 のデバッグシーケンスを使う（`probe-rs-espressif/src/lib.rs:121`）。
- `make rtt PROBE=303a:1001:<MAC>`（中身は `tools/probe-rs/rtt.sh`）。
  - probe-rs はバイナリのチャネルを、改行なしの 16 進テキスト（`"0a 1b "`）で出力する（`probe-rs-tools/.../stream_decoder.rs:206-209`）。パイプにすると出力がバッファに溜まったまま流れない。そのため `--target-output-file` に書かせ、`tail -F` で追いかけて `t5log decode --hex` で解読する。
  - probe-rs はフォアグラウンドで動かし、Ctrl-C で止まるようにした。プロセスグループに SIGINT を送る試験で、probe-rs、tail、t5log がすべて終了することを確認した。
- `PROBE` は必須にした。この PC には ESP のデバッグプローブが 2 つつながっているため（`probe-rs list`）。
- t5emu は、デバッガの役もする。4096 命令ごとに up チャネルを読んで RdOff を進め、最後に「RTT で受けたバイト列」と「USB のバイト列」が一致するかを確かめる（不一致なら終了コード 1）。`--rtt-poll N`、`--log-from-rtt` で、ホストが遅い場合やいない場合も試せる。

### 6.3 確認したこと
- **実機（Tab5、v1.0）で、probe-rs から RTT のログを回収できた。** 72 件のイベントを解読でき、USB で取ったバイト列と完全に一致した（2,546 バイト）。ログは [logs/l2-tab5-rtt-20261030.txt](logs/l2-tab5-rtt-20261030.txt)。
- **LP SRAM はシステムバスで読まれ、ポーリング中に CPU は止まらなかった。** `RUST_LOG=debug` のログでは、`0x5010e03c`（up チャネルの WrOff）などの読み出しは 86 回すべて `using SystemBus` だった。コアが止まったのは、接続と切断の処理のときだけ（`Core halted` 9 回）。L2MEM（プログラムバッファ経由、13 §0-3）でなく LP SRAM に置いた判断が効いた。
- probe-rs はアタッチのときにウォッチドッグを止める（ログの `Disabling ESP32P4 watchdogs`）。13 の記述どおり。
- 注意: RTT は、一度読んだデータを再び読めない。カーネルは自己試験の後はログを出さないので、2 回目のアタッチでは何も表示されない。もう一度見るにはリセットする（今回は USB の RTS でリセットした）。
- t5emu: RTT と USB のバイト列が一致した（2,495 バイト）。バッファを 512B に縮めた試験では、ホストが遅いと `log.dropped sink=rtt count=10` などが出ること、ホストがいないと入る分だけが欠けずに残ることを確かめた（試験の後に元に戻した）。

### 6.4 残っていること
- アタッチ中に CPU が止まる時間が、ログの時刻にどう影響するか（SYSTIMER は止まらない設定のはず。`TIMER_UNIT0_CORE0_STALL_EN` は既定で 0）。まだ測っていない。
- probe-rs の CLI に、バイナリのチャネルをそのまま（16 進にせずに）ファイルへ書く機能があれば、`--hex` は要らない。上流への提案の候補（13 §4 の段階 4）。
- L3（リングのヘッダ、前回の起動のログの回収、RWDT との組み合わせ）。

## 7. L3 の実装記録（2026-10-30）: リセットをまたいだログの回収

### 7.1 実装
- **リングのヘッダ**（`kernel/src/log.rs` の `RingHeader`）: マジック `T5LR`、boot_id、書き込み位置、wrap したかどうか、これらの FNV-1a のチェック値。レコードを書くたびに更新する。電源投入直後のゴミをリングと取り違えないよう、チェック値まで合ったときだけ「前回の起動のリング」とみなす。
- **起動時の再送**: ヘッダが有効なら、前回のフレームを古い順に RTT と USB へ流し直す（リングには入れない）。前後を `log.prev_begin { boot_id, bytes }` と `log.prev_end { boot_id, frames }` で挟む。
  - wrap していた場合は、最初の 0x00 まで読み飛ばす（先頭のフレームは途中から始まっているため）。
  - その後、boot_id を 1 増やして新しいリングを始める。電源投入直後（ヘッダが無効）は 1 から始める。
- **`boot.start`** に `boot_id` と `reset_cause` を足した。`reset_cause` は `LP_CLKRST_RESET_CAUSE_REG`（0x5011_1010）の HPCORE0 の欄（[12:7]）で、値は ROM の `RESET_REASON`（`esp_rom/esp32p4/include/esp32p4/rom/rtc.h`）。
- **RWDT**: カーネルはもう RWDT を止めない。次のように設定して使う。
  - stage 0 の動作を 3（HP システムだけをリセットし、RTC/LP ドメインは残す）にする。ブートローダは 4（RTC も含めてリセット）にしている（`bootloader_init.c:93`）。
  - タイムアウトは約 4 秒（`CONFIG1.WDT_STG0_HOLD = 272000`）。
  - スケジューラのループで餌をやる。すべてのプロセスが眠っているときも、1 秒以内に一度は起きて餌をやる。
  - 意図して止めるとき（`Platform::exit`）だけ RWDT を止める。
- **`boot.rwdt { config0, stg0_hold }`** イベントで、実際の設定値を記録する。
- **試験用のビルド** `--features test-lockup`: 自己試験の後、カーネルが（最後のプロセスからのトラップ処理中のまま）0x4 を読む。リリースには入れない。
- **t5log と t5emu**: `BootContext` で、`prev_begin`/`prev_end` に挟まれたレコードに印を付ける（`--pretty` では `prev#ID`、JSON では `"prev_boot":ID`）。
  - t5emu は、RWDT の満了と lock-up の後に「システムリセット」を模す（`--resets N`）。stage 0 の動作が 3 なら LP SRAM を残し、そうでなければ失う。
  - `--expect-prev-event NAME` で、NAME が前回の起動から再送されたことを合否にする。

### 7.2 実機（Tab5）で確かめたこと
- **lock-up → RWDT のリセット → 次の起動で、直前のログを回収できた。** `rst:0x9 (HP_SYS_LP_WDT_RESET)` の後の起動で、前回の 73 フレーム（`selftest.lockup_now` まで）が再送された。ログは [logs/l3-tab5-lockup-20261030.txt](logs/l3-tab5-lockup-20261030.txt)。
- **カーネルの例外は、やはり lock-up になった**（11 §4 の推測を実機で確認）。panic の表示は出ず、ROM の `CPU_LOCKUP_RESET`（26）による即時のリセットも起きなかった。RWDT が満了するまで止まったままになる。
- **LP SRAM は、HP システムのリセット（`HP_SYS_LP_WDT_RESET` = 9）でも、USB-UART からのチップリセット（`CHIP_USB_UART_RESET` = 23、RTS によるもの）でも内容が残った。** RTC ドメインごとリセットする場合（stage 0 = 4）は試していない。
- `reset_cause` は、RWDT のリセット後が 9、RTS のリセット後が 23。ROM の表示と一致した。
- **RWDT のタイムアウトの実測**:

  | `STG0_HOLD` | 設定上の意図 | 実測のリセット間隔 |
  |---|---|---|
  | 1,224,000（ブートローダの値。9 秒 × 136kHz） | 9 秒 | 19.5 秒 |
  | 272,000（カーネルの値） | 約 4 秒（下の推測から） | 4.57 秒 |

  - リセット間隔は、lock-up までの約 0.35 秒と起動にかかる時間を含む。
  - 2 点とも「タイムアウト = 2 × hold ÷ 136kHz」で説明できる（推測）。ESP-IDF の `lpwdt_ll_config_stage` は `hold = ticks >> (1 + WDT_DELAY_SEL)` と書く（`esp_hal_wdt/esp32p4/include/hal/lpwdt_ll.h:120`）。一方、ブートローダが書いた値はシフトされていない 9 秒分だった。このずれと合っている（推測）。
  - t5emu のモデルもこの式にした。
- 確認の後、通常のカーネルを ota_0 に書き戻した（`selftest.result pass=true`）。

### 7.3 残っていること
- RTC ドメインのリセット（電源投入、stage 0 = 4）の後は、ヘッダが無効になって boot_id が 1 に戻ることを想定している。実機では試していない。
- 再送するとき、RTT の空きが足りないとフレームの途中で切れることがある（64 バイト単位で書くため）。解読側は 0x00 で同期し直すので、壊れたフレームは 1 件の「解読できないフレーム」として出る。
- 時刻は起動ごとに 0 から始まる（SYSTIMER がリセットされる）。起動をまたいで時刻を比べたいときは boot_id と組にする。
