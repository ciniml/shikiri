# 13. probe-rs の ESP32-P4 対応の調査

- 日付: 2026-10-30
- 状態: 調査結果。ユーザーの判断（2026-10-30）で、段階 0〜1 を実施した。**実機で接続と、LP SRAM の RTT の回収を確認した（§8）。**
- 目的: RTT でのログ回収とデバッグを、OpenOCD ではなく probe-rs で行えるかを調べる。必要なら上流へ P4 対応を出す。
- 事実には出典を付け、推測には「（推測）」、設計判断には「（判断）」と書く。
- 参照したソース:
  - probe-rs: `git clone --depth 1` した master、コミット `dd4909231da2e17bea25c9277196d9fdf3ade53d`（2026-10-29）。以下 `PR:` は probe-rs リポジトリのルートからのパス。
  - openocd-esp32: ローカルの `~/.espressif/tools/openocd-esp32/v0.12.0-esp32-20260424/openocd-esp32/share/openocd/scripts/`（以下 `OCD:`）と、GitHub の `src/target/espressif/esp32p4.c`（コミット `947525820c9`。以下 `esp32p4.c`）。
  - esp-flash-loader: GitHub master（コミット `374f3e9824c`）。
  - ESP-IDF: `~/esp-idf/master`（以下 `IDFm`）。

## 0. 結論

1. **probe-rs は既に ESP32-P4 に対応している。新しく移植する必要はない。** 最初の対応は PR #3649（@xobs、2025-12-17 マージ、rev v1.3 で動作確認）で、**0.31.0（2026-01-17）から入っている**。doc/12 §1.2 の「probe-rs は P4 を挙げていない」は、ローカルにある 0.27.0 の話で、上流の現状とは違う。
2. 対応は **v1.x と v3.x の両方**を対象にしている。チップ検出は IDCODE `0x12c25` だけで行い、デバッグシーケンスは ROM の ECO 番号（`0x4fc00014`）で v1.x（ECO 0〜4）と v3.x（ECO 5 以上）を分けている（`PR:probe-rs-espressif/src/sequences/esp32p4.rs:263,275-301`）。フラッシュ用のアルゴリズムも、v1.0 の ROM（ECO2〜ECO4）の表を持っている。
3. **Tab5（v1.0）での L2MEM の読み書きは、プログラムバッファ経由になる。** そのため、読み書きのたびに core0 を一時的に止める。キャッシュとの不整合は起きない（core0 自身の lw/sw なので）が、RTT をポーリングするたびに core0 が止まる。
4. **LP SRAM（0x5010_8000〜）は、上流の `esp32p4.yaml` のメモリマップにない。** doc/12 の案どおり RTT のバッファを LP SRAM に置くと、probe-rs は `BufferNotReachable` で拒否する（`PR:probe-rs/src/rtt/channel.rs:219-239`）。**独自のチップ記述（`--chip-description-path`）で RAM 領域を足せば済む**（判断）。システムバス経由なので、コアを止めずに読める見込み（推測）。
5. **接続時に、probe-rs はウォッチドッグ（TIMG0/1、RWDT、スーパーウォッチドッグ）をすべて止める**（`esp32p4.rs:200-250,308-313`）。doc/12 の L3（RWDT によるリセットからの回収）を試すときは、probe-rs をつないだままにしてはいけない。
6. 最新リリースは 0.32.0（2026-07-22）。その後の master には P4 向けの修正が入っている（UART0 のクロック #4251、ハートの選択 #4358、キャッシュ同期 #4310）。**使うなら master の特定コミットを `cargo install --git ... --rev` で入れる**（判断）。
7. LP コアのデバッグは、probe-rs ではまだ定義されていない（`esp32p4.yaml` のコアは cpu0/cpu1 だけ）。上流に貢献できる余地はここと、v1.0 での動作報告にある（判断）。
8. 工数は、段階 1（接続、停止、メモリの読み出し、RTT）で 0.5〜1 日、段階 2（リセット、ブレークポイント）で 1 日程度（推測）。フラッシュの書き込みは、当面 espflash のままにする（判断）。

## 1. 上流の状況

### 1.1 probe-rs の PR と issue（`gh` で 2026-10-30 に確認）

| 番号 | 状態 | 日付 | 内容 |
|---|---|---|---|
| #2047 | closed | 2024-01-12 作成、2025-12-17 close | 「Initial ESP32-P4 support」（@MabezDev）。#3649 に引き継がれて閉じた |
| #3649 | merged | 2025-12-17 | 「esp32p4: add initial support」（@xobs）。フラッシュとリセットを含む。**rev v1.3 で確認済み** |
| #3989 | merged | 2026-05-05 | ベンダ固有のフラッシュ関数を一般化し、チップのリビジョンを読む（`ChipRevision`）。P4 v3 を修正 |
| #4002 | merged | 2026-05-20 | P4 の割り込みコントローラの範囲（0x2000_0000〜）を読めるようにする |
| #4009 | **open** | 2026-05-14 | 「ESP32-P4 related tweaks」（@bugadani）。割り込みコントローラのレジスタ、ハート選択の堅牢化 |
| #4077 | merged | 2026-06-26 | P4 v3.1、v3.2 に対応 |
| #4251 | merged | 2026-08-12 | リセット後に UART0 のクロックを有効のままにする（ROM が起動に失敗するため） |
| #4293 | merged | 2026-08-30 | Espressif のチップを、メモリを読まずに IDCODE で検出する |
| #4294 | closed | 2026-08-30 | 非キャッシュの RAM をシステムバスで読む案。「キャッシュされたメモリをシステムバスで読むとテストが固まる」 |
| #4309/#4310 | merged | 2026-09-05 | RISC-V の修正と、キャッシュ保守（`CacheSync`）を差し込む仕組み。P4 v3 は L2MEM をシステムバスで読むようになった |
| #4311 | merged | 2026-09-05 | Espressif の領域ごとのアクセス方法の上書きを、**v1 の P4 を除いて**削除 |
| #4357 | closed | 2026-09-16 | 「esp32p4: reset fails」。#4358（dmcontrol の hartsel を読み戻して確認）で修正 |

- 出典: https://github.com/probe-rs/probe-rs/pull/3649 など（各番号）。
- `CHANGELOG.md` では、0.31.0 の節に「Add support for ESP32-P4 (#3649)」、0.32.0 の節に「Added support for ESP32-P4 v3.1 and 3.2 (#4077)」がある（`PR:CHANGELOG.md:205,65`）。
- #4310、#4311、#4358、#4251 は 0.32.0 の後のマージで、**未リリース**。

### 1.2 esp-rs と Espressif の方針
- **esp-hal は v1.x（NRND 品）を対応しないと明言している**: 「We don't want, nor will we accept support for these variants.」（@MabezDev, 2026-03-09, https://github.com/esp-rs/esp-hal/issues/2285#issuecomment-4022872793 ）。Tab5 で esp-hal の例が動かないという issue（esp-rs/esp-hal#6179, 2026-08-24）も、この発言を参照して閉じられた。
- 一方、**probe-rs 側は v1.x の経路を残している**（#4311 の本文「except for the v1 P4 chips」、`esp32p4.rs:275-301`）。esp-flash-loader も v1.x の ROM 表を持つ（§2.6）。ただし、v1.x の動作確認は今後も上流の優先度が低い（推測）。
- esp-hal#6179 の起動ログに、Tab5 の ROM は `ESP-ROM:esp32p4-eco2-20240710` と出ている。本プロジェクトの Tab5（doc/09: v1.0）も ECO2 と思われる（推測）。

## 2. probe-rs での Espressif RISC-V の実装

### 2.1 構成
- Espressif の対応は、独立したクレート `probe-rs-espressif` にまとまっている（#3813「Vendor code as plugins」）。中身はターゲット YAML（`targets/*.yaml`）、デバッグシーケンス（`src/sequences/*.rs`）、USB-JTAG のプローブドライバ（`src/espusbjtag/`）、IDF 形式のイメージ（`src/image_format.rs`）。
- チップ名からシーケンスを選ぶ: `PR:probe-rs-espressif/src/lib.rs:121-122`（`esp32p4` → `ESP32P4::create()`）。

### 2.2 ターゲット YAML（`PR:probe-rs-espressif/targets/esp32p4.yaml`）
- 検出: `idcode: 0x12c25`、`variants: {}`（1-10 行）。変種が 1 つで magic を省略しているので、IDCODE だけで決まる（`lib.rs:79-81`）。コメントに「v1.x と v3.x の両方」とある（9 行）。
- コア: `cpu0`（hart 0）と `cpu1`（hart 1）。どちらも `jtag_tap: 1`（12-22 行）。
- JTAG のスキャンチェーン: `lpcpu`（IR 長 5）、`hp`（IR 長 5）の順（59-64 行）。つまり TAP 0 が LP、TAP 1 が HP。
- メモリマップ（23-56 行）:

| 範囲 | 種類 | 備考 |
|---|---|---|
| 0x0000_0000–0x0400_0000 | Nvm（フラッシュ、boot） | フラッシュのオフセット |
| 0x4000_0000–0x4400_0000 | Nvm（別名） | XIP の窓 |
| 0x4FF0_0000–0x4FFC_0000 | Ram | L2MEM（キャッシュあり） |
| 0x8FF0_0000–0x8FFC_0000 | Ram「Uncached RAM」 | L2MEM の非キャッシュの別名（+0x4000_0000） |

- **LP SRAM（0x5010_8000–0x5011_0000）と周辺レジスタは、メモリマップにない。**
- `default_binary_format: idf`（65 行）。

### 2.3 デバッグシーケンス（`PR:probe-rs-espressif/src/sequences/esp32p4.rs`）
- `on_connect`（307-313 行）: `configure_memory_access` のあと、`disable_wdts` を呼ぶ。
- `disable_wdts`（200-250 行）: TIMG0/TIMG1 の MWDT、LP_WDT（RWDT, 0x5011_6000）を止め、スーパーウォッチドッグを無効化して自動で餌をやる設定にし、割り込みの状態を消す。OpenOCD の `esp32p4_wdt_disable`（`OCD:target/esp32p4.cfg:35-64`）と同じレジスタと値。
- `configure_memory_access`（252-304 行）:
  - 0x2000_0000–0x3000_0000（CLIC/CLINT などの CPU 周辺）は、すべての幅でプログラムバッファ経由。
  - ROM の ECO 番号を `0x4fc00014` から読む（263 行。IDF の `_rom_eco_version = 0x4fc00014`, `IDFm:components/esp_rom/esp32p4/ld/esp32p4.rom.version.ld:14`）。
  - **ECO ≥ 5（v3.x）**: L2MEM をシステムバスで読み、前後でキャッシュを書き戻す/無効化する（`ESP32P4CacheSync`, 80-151 行）。
  - **ECO < 5（v1.x、Tab5）**: フラッシュの窓、L2MEM、非キャッシュの別名を、すべてプログラムバッファ経由にする。コメントは「System bus access to RAM appears broken, and returns garbage values.」（287-299 行）。
  - ECO 0〜4 が v3 未満であることは、IDF のリンカスクリプトの選び方から分かる（`IDFm:components/esp_rom/CMakeLists.txt:88-89` で `CONFIG_ESP32P4_SELECTS_REV_LESS_V3` のとき `eco0_4` を使う）。
- `reset_system_and_halt`（315-384 行）: OpenOCD の `esp32p4_soc_reset`（`OCD:target/esp32p4.cfg:74-117`）を移植したもの。PMU で両コアのストールを解き、`LP_SYS_SYS_CTRL_REG`（0x5011_0008）でシステムリセット、周辺のクロックゲートを強制オンにし、その後 UART0 のクロックを有効にしてから（#4251）ハートをリセットして止める。最後に周辺のリセットを手で掛け直す。
- セミホスティングは、IDF 互換の処理を共通コードで持つ（`esp.rs`、`esp32p4.rs:386-392`）。

### 2.4 USB-JTAG のプローブドライバ
- `PR:probe-rs-espressif/src/espusbjtag/mod.rs`（187 行）と `protocol.rs`（827 行）。チップに依存しない汎用の実装で、JTAG のビット列を USB で送る。
- 同じ USB デバイスのシリアル（CDC）を、JTAG と並行して開ける（`mod.rs:131` の `target_serial`、`changelog/added-esp-usb-jtag-target-serial.md`）。本プロジェクトのカーネルは USB Serial/JTAG の FIFO に直接書いているので（doc/09）、CDC の出力と RTT を同時に使える見込み（推測）。

### 2.5 マルチ TAP、マルチコア、LP コア
- コアごとの `jtag_tap` で TAP を選ぶ（`PR:probe-rs/src/core.rs:739-743`、`session.rs:280,462`）。複数のデバッグモジュールへの対応は #2386（2024-04）。
- **C6 の LP コア**は、HP と同じデバッグモジュールの hart 1 として、別のチップ記述 `esp32c6_lp.yaml` になっている（`PR:probe-rs-espressif/targets/esp32c6_lp.yaml:6-11`）。P4 の LP コアは別の TAP なので、同じ方法は使えない。
- **P4 の LP コアは、probe-rs ではまだ定義されていない**（`esp32p4.yaml` のコアは cpu0/cpu1 だけ）。
- OpenOCD の P4 の構成:
  - TAP は 2 つ。どちらも IDCODE `0x0012c25`、IR 長 5。`_LP_TAPNUM 1`、`_HP_TAPNUM 1`（`OCD:target/esp32p4.cfg:11-16`）。`_LP_TAPID` を定義していないので「Legacy chips: all taps use the same TAP ID」の経路で作られる（`OCD:target/esp_common.cfg:93-134`）。
  - HP の 2 コアは TAP 1 に、LP コアは TAP 0 に作られる（`esp_common.cfg:136-176`）。probe-rs の YAML と一致する。
  - core1 は、examine のときにリセットを解き、クロックを入れる（`esp32p4.cfg:130-145`）。probe-rs は、接続時に動いていないハートを後で再確認する（`changelog/fixed-riscv-late-hart-debug.md`）。
  - LP コアを含む 3 コアの構成 `esp32p4-lpcore.cfg` がある（`_ONLYCPU 0x13`、6 行）。LP コアのリセットと `LPPERI_CPU_REG.lpcore_dbgm_unavaliable = 0`（60 行）で、デバッグできるようにしている。doc/01 §5.4 の「HP と LP を同時にはデバッグできない」（TRM §2.11.1.1）と食い違うように見える。この設定が v1.x で動くかは未確認（推測）。

### 2.6 フラッシュの書き込み
- probe-rs は、esp-flash-loader（https://github.com/esp-rs/esp-flash-loader ）をビルドしたアルゴリズムを YAML に埋め込んでいる（`esp32p4.yaml:66-101`）。
- アルゴリズムは非キャッシュの L2MEM（`load_address: 0x8ff50000`、`data_load_address: 0x8ff60000`）に置かれ、ROM の SPI フラッシュ関数を呼ぶ（esp-flash-loader `ld/esp32p4.x`）。
- ROM のデータ/BSS の表は、リビジョンごとに持っている（esp-flash-loader `src/chip/esp32p4.rs`）: ECO0（0）、ECO1（1）、**ECO2〜ECO4（100 = v1.0 以上）**、ECO5（300）、ECO6（301）、ECO7（302）。**v1.0 も対象に入っている。**
- `default_binary_format: idf` なので、ELF を書くときは probe-rs が IDF のアプリイメージを作り、ブートローダとパーティションテーブルも書く（espflash の仕組みを流用。#3989 の本文は「espflash がリビジョンでブートローダを選ぶようになった」）。本プロジェクトは独自設定のブートローダ（doc/05、doc/09）を使っているので、そのままでは置き換わるおそれがある（推測）。

### 2.7 RTT
- RTT は汎用の実装（`PR:probe-rs/src/rtt.rs`、`rtt/channel.rs`）。制御ブロックは ELF の `_SEGGER_RTT` から探すか、RAM 領域を走査する（`rtt.rs:574` の `ScanRegion`）。
- **チャネルのバッファは、メモリマップの RAM 領域に完全に収まっていないと拒否される**（`rtt/channel.rs:219-239`、`Error::BufferNotReachable`）。
- メモリは `Core` のメモリインタフェースで読むので、§2.3 のアクセス方法がそのまま使われる。v1.x の L2MEM ではポーリングのたびに core0 を止める（§3.3）。
- フラッシュアルゴリズム自身も RTT でログを出す（`rtt_location: 0x8ff72c08`、`esp32p4.yaml:85`）。

## 3. P4 固有の点と、Tab5（v1.0）にとっての意味

### 3.1 OpenOCD と probe-rs の比較

| 項目 | openocd-esp32 | probe-rs master | 差 |
|---|---|---|---|
| TAP / IR 長 | 2 TAP、IR 5、`0x12c25` | 同じ（`esp32p4.yaml:59-64`） | なし |
| HP 2 コア | SMP、core1 を examine 時に起こす | cpu0/cpu1、遅れて現れたハートを再確認 | 起こし方が違う（推測: probe-rs は core1 を自分では起こさない） |
| LP コア | `esp32p4-lpcore.cfg` | なし | **probe-rs に無い** |
| ウォッチドッグ | `esp32p4_wdt_disable` | `disable_wdts`（同じ値） | なし |
| システムリセット | `esp32p4_soc_reset` | 移植済み + UART0 クロック | なし |
| L2MEM の読み書き | 全リビジョンで sysbus + キャッシュ同期（`esp32p4.c:375-429,476-478`） | v3.x は sysbus + 同期、**v1.x は progbuf** | **v1.x の扱いが違う** |
| リビジョン判定 | eFuse から `hw_rev`（`esp32p4.c:275-300`） | ROM の ECO（`0x4fc00014`） | 方法が違う |
| v1/v3 の CSR の違い | v1 では v3 専用の CSR（mintthresh、u* 系）を隠す（`esp32p4.c:318-339`） | 標準の CSR のみ | probe-rs は独自 CSR を見せない（推測） |
| フラッシュ | スタブ（作業領域: v1 は 0x8FF0_0000、v3 は 0x4FF8_0000） | esp-flash-loader（0x8FF5_0000〜） | どちらも L2MEM の一部を上書きする |

### 3.2 キャッシュ
- L2MEM は書き戻し方式の L1 D キャッシュの後ろにある（doc/09 §2）。デバッグモジュールのシステムバスはキャッシュを通らない（`esp32p4.rs:80-83` のコメント）。
- OpenOCD は、システムバスで読む前に対象の行を書き戻し、書いた後に L1 を無効化する（`esp32p4.c:415-429`）。probe-rs の `ESP32P4CacheSync` も同じレジスタ（`CACHE_SYNC_MAP/ADDR/SIZE/CTRL`, 0x3FF1_0098〜）を同じ順で使う。
- **v1.x では、probe-rs はこの経路を使わない**。プログラムバッファの `lw` は core0 自身のロードなので、core0 の D キャッシュと整合する。core1 が書いたデータとの整合は保証されない（推測）が、本プロジェクトは今は core0 しか使っていない。
- LP SRAM は周辺の空間にあり、キャッシュされない（doc/12 §1.3、推測）。probe-rs の既定のアクセス方法はシステムバス（`PR:probe-rs/src/architecture/riscv/communication_interface.rs:1091-1115`）で、領域の上書きもないので、**LP SRAM はコアを止めずに読める見込み**（推測。v1.x で LP SRAM へのシステムバスアクセスが正しく動くかは要実機確認）。

### 3.3 プログラムバッファ経由の読み書きの副作用
- プログラムバッファでの読み書きは `halted_access` で包まれる。動いているコアを止め、終わったら再開する（`communication_interface.rs:1277-1281,1357`、`perform_memory_read_progbuf` は 1931-1935 行）。
- デバッグモードのプログラムバッファは M モードの権限で動く（RISC-V Debug Spec 0.13.2 §4.1。mstatus.MPRV は dcsr.mprven=0 なら無視）。U モードで止めても、ロックされていない PMP エントリには引っかからない。**カーネルが L2MEM を読めない設定でロックした PMP エントリを作ると、デバッガも読めなくなる**（doc/01 §5.4 と同じ注意）。
- RTT のポーリングのたびに core0 が止まる（ポーリングの間隔は確認していない）。SYSTIMER やタイマ割り込みのタイミングがずれる（推測）。

### 3.4 本プロジェクトの設計（doc/12）への影響
1. **RTT のバッファは LP SRAM に置く方針のままでよい**（判断）。v1.x ではコアを止めずに読める見込みがあり、L2MEM より有利。ただし、LP SRAM を RAM として足した独自のチップ記述が要る（§4 の段階 1）。
2. **probe-rs をつなぐと RWDT とスーパーウォッチドッグが止まる。** カーネルが RWDT を有効にしている場合、接続後は効かなくなる。L3 の試験（RWDT によるリセット）は probe-rs を外して行う（判断）。
3. **フラッシュの書き込みは、当面 espflash（`make flash-p4`）のままにする**（判断）。probe-rs の IDF 形式の書き込みは、ブートローダとパーティションテーブルを自分の判断で書くので、本プロジェクトの設定（`CONFIG_BOOTLOADER_RESERVE_RTC_SIZE 0x0` など）と合うか分からない。
4. フラッシュアルゴリズムは 0x8FF5_0000〜0x8FF8_0000 付近（L2MEM の 0x4FF5_0000〜）を使う。probe-rs で書き込んだ後は必ずリセットするので問題ない（推測）。

## 4. 推奨する計画と工数

| 段階 | 内容 | 実機 | 工数（推測） |
|---|---|---|---|
| 0 | probe-rs master（コミットを固定）をビルドして入れる。LP SRAM を RAM として足した独自のチップ記述 `tools/probe-rs/esp32p4-tab5.yaml` を作る（上流の YAML を写し、`!Ram 0x5010_8000–0x5010_FA80` を追加）。`probe-rs chip info` で読めることを確かめる | 不要 | 0.5 日 |
| 1 | `probe-rs info`（スキャンチェーン、IDCODE）、`attach`（止めない）、`read`（LP SRAM、L2MEM）、`halt`/`resume`、RTT の回収（`probe-rs attach --chip-description-path ... kernel.elf`）。CDC の出力と並行して動くかも見る | **要** | 0.5〜1 日 |
| 2 | `reset`（`reset_system_and_halt`）、ハードウェアブレークポイント（3 本）、U モードのアプリでの停止、core1 がリセット中のときの挙動 | **要** | 1 日 |
| 3 | フラッシュの書き込み（`--binary-format bin` でカーネルの app イメージだけを 0x10000 に書く、など）。espflash と結果が一致するか | **要（書き込み）** | 1〜2 日 |
| 4（任意） | 上流への貢献: (a) v1.0 での動作報告、(b) P4 の LP コアの対応（別 TAP の LP コアを `esp32p4_lp` として定義し、`lpcore_dbgm_unavaliable` を解く処理を移植）、(c) LP SRAM をメモリマップに入れる PR | 要 | (a) 0.5 日、(b) 3〜5 日、(c) 0.5 日 |

- 段階 1 で動かなかった場合は、doc/12 のとおり OpenOCD の RTT（`rtt setup 0x50108000 ...`）に戻る（判断）。両者は同じ SEGGER RTT の制御ブロックを使うので、カーネル側は変えなくてよい。

## 5. リスク

1. **v1.x の動作確認が上流で手薄になる**（推測）。esp-hal は v1.x を扱わない（§1.2）ので、probe-rs の CI やメンテナの手元でも v3.x が中心になる。#4311 のような変更で v1.x の経路が壊れても気付かれにくい。→ 使うコミットを固定し、更新するときは段階 1 の確認をやり直す。
2. **未リリースの修正に依存する**。0.32.0 には #4251（リセット後に ROM が起動しない問題）、#4358（ハート選択）が入っていない。
3. **v1.x の L2MEM がプログラムバッファ経由**なので、RTT を L2MEM に置くとポーリングのたびに core0 が止まる。LP SRAM を使えば避けられる見込みだが、LP SRAM へのシステムバスアクセスが v1.x で正しく動くかは未確認。
4. **ウォッチドッグが止まる**ので、接続中は WDT 関連の試験ができない。
5. probe-rs は P4 独自の CSR（`mexstatus`、PMA、HWLP など）を見せない（推測）。トラップの解析で必要になったら、OpenOCD の `expose_csrs`（`OCD:target/esp32p4.cfg:147-238`）の一覧が参考になる。
6. 接続時のチップ検出がメモリを読まない（#4293）ので、v1.x でも検出で失敗しにくい。ただし、ECO の読み出し（`0x4fc00014`）は接続直後にシステムバスで行うので、ここが v1.x で失敗すると、v3.x 扱い（システムバス + キャッシュ同期）になるおそれがある（推測）。ログ（`RUST_LOG=probe_rs=debug`）でどちらの経路になったか確認する。

## 6. 実機なしでできること / 実機が要ること

- **実機なしでできる**:
  - probe-rs master のビルドとインストール。独自のチップ記述の作成と、`probe-rs chip info` での読み込みの確認。
  - カーネルの ELF に `_SEGGER_RTT` があり、バッファが LP SRAM の範囲に収まることの確認（`nm`、`readelf`）。
  - RTT の制御ブロックの形式を、t5emu のメモリから読む試験（doc/12 L2）。t5emu は JTAG のデバッグモジュールを模していないので、probe-rs 自体はつなげない（doc/11 の対象一覧に JTAG/DM がない）。
  - probe-rs の単体テスト（`cargo test -p probe-rs-espressif`）。
- **Tab5 が要る**（実機への書き込み・接続は、ユーザーの確認後に行う）:
  - USB-JTAG への接続、スキャンチェーン、ECO の値の読み出し、v1.x の経路が選ばれること。
  - LP SRAM と L2MEM の読み出し、RTT の回収、CDC との並行動作。
  - リセット、ブレークポイント、core1 の扱い。
  - フラッシュの書き込み（段階 3）。

## 7. 確認したいこと

1. 段階 0〜1（probe-rs master を入れ、LP SRAM を足したチップ記述で RTT を回収する）を、OpenOCD の L2 の代わりに進めてよいか。
2. 段階 1 の実機での確認（USB-JTAG で接続、CPU を止める、メモリを読む。フラッシュは書かない）の許可。
3. 上流への貢献（§4 の段階 4）を、どこまで目標に入れるか。
4. ~~doc/12 §1.2 の probe-rs の記述を直してよいか~~ → 直した（2026-10-30）。

## 参考
- probe-rs: https://github.com/probe-rs/probe-rs （PR #2047, #3649, #3989, #4002, #4009, #4077, #4251, #4293, #4294, #4309, #4310, #4311, #4358、issue #4357）
- probe-rs のリリース: https://github.com/probe-rs/probe-rs/releases （0.31.0: 2026-01-17、0.32.0: 2026-07-22）
- esp-flash-loader: https://github.com/esp-rs/esp-flash-loader （PR #36, #38、`src/chip/esp32p4.rs`、`ld/esp32p4.x`）
- esp-hal の v1.x への方針: https://github.com/esp-rs/esp-hal/issues/2285#issuecomment-4022872793 、issue #5880、#6179
- openocd-esp32: https://github.com/espressif/openocd-esp32/blob/master/src/target/espressif/esp32p4.c 、ローカルの `target/esp32p4.cfg`、`target/esp32p4-lpcore.cfg`、`target/esp_common.cfg`、`board/esp32p4-builtin.cfg`
- RISC-V External Debug Support 0.13.2 §4.1（Debug Mode の権限）

## 8. 段階 0〜1 の結果（2026-10-30）

- 段階 0: `make probe-rs` で、コミット `dd4909231da2` を `build/probe-rs` に入れた（`probe-rs 0.32.0 (git commit: dd49092)`）。独自のチップ記述は `tools/probe-rs/esp32p4-tab5.yaml`。
- 段階 1: Tab5 の内蔵 USB-JTAG（プローブ `303a:1001:30:ED:A0:E0:CB:86`）で、次のことを確認した。フラッシュは書いていない。
  - `Using sequence Riscv(ESP32P4)`、IDCODE `0x00012C25`、TAP 2 つ（§2.5 のとおり）。
  - ウォッチドッグの停止、core0 の停止、トリガ 3 本（type 4）の検出、core1 の扱い。いずれもエラーなし。
  - **ELF の `_SEGGER_RTT`（0x5010_E018）から制御ブロックを見つけ、LP SRAM をシステムバスで読んだ。** ポーリング中に core0 は止まらない。L2MEM のときにプログラムバッファ経由になる問題（§0-3）を避けられた。
  - RTT で受けたバイト列は、USB の CDC で受けたものと完全に一致した。詳細は 12 §6.3。
- その後、t5log が probe-rs をライブラリとして使い、RTT を直接読むようにした（12 §8）。`core.reset()`（JTAG のリセット）も、v1.0 で動いた。
- 段階 2 のうちブレークポイント、段階 3（書き込み）、段階 4（上流への貢献）は未実施。
