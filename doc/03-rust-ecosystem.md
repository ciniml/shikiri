# 03. Rust エコシステムと既存 Rust 製 OS の調査（ESP32-P4 / M5Stack Tab5 向け）

- 調査日: 2026-10-29
- 対象: esp-hal / esp-rs の ESP32-P4 対応、ターゲット triple と ABI、riscv / riscv-rt クレート、既存 Rust 製 OS（Tock, Hubris ほか）の M/U 分離・syscall・動的ロード設計、ESP-IDF 共存案との比較
- 参照したソースのスナップショット（`git clone --depth 1`、すべて scratchpad 内）
  - esp-hal `main` @ `5c0820cf9e2be0a49719bd71c30e7a45cae48134`（2026-10-27）
  - tock `master` @ `d4979a4c09b6253fc0c44ddb202096ddcb15f445`（2026-10-29）
  - libtock-c @ `8dad482b65b37ce5eb2c79f9fd6894bca771de71`、libtock-rs @ `b7f17591e43607474958ed7d117ba831e54e1017`
  - hubris @ `c611e20caa2dac7a999668021468d641380648b8`（2026-10-27）
  - xous-core @ `e8d989c754bcc3a9cd66f9e3e1c70cac580731eb`（2026-10-26）
  - espflash @ main（2026-10 時点、CHANGELOG 参照）
  - esp-idf `master` @ `9c37a0a75676c3726284318925b5ae17bba3bd88`（2026-10-27、sparse checkout）
  - ローカル crates: `~/.cargo/registry/src/*/` の esp-hal-1.1.2, esp-riscv-rt-0.14.0, riscv-0.15.0, riscv-rt-0.16.0, riscv-rt-macros-0.6.1
- 以下、`[Ex]` は末尾「参考文献」の番号。

---

## 0. OS 設計への結論

- **前提（一次情報で確認済み）: 手元の Tab5 の ESP32-P4 は rev v1.0 であり、esp-hal の対象外。**
  - 手元の Tab5 のブートログ（`~/repos/tab5_claude_client/logs/boot.log:15`）に `I (10) boot: chip revision: v1.0` とある。ROM は `ESP-ROM:esp32p4-eco2-20240710`（同 2 行目）、bootloader は `ESP-IDF v6.0-beta1-624-g6534932340`（同 11 行目）[E21]。
  - ほかの Tab5 個体でも v1.3 の報告がある（esp-hal #6179）[E4]。NuttX の Tab5 ページも「rev v1.0 で出荷」として `CONFIG_ESP32P4_SELECTS_REV_LESS_V3=y` を設定している [E20]。
  - esp-hal は 1.2.0（2026-09）および main @5c0820c のどちらでも「Revisions below v3.0 are not supported.」と明記している（`esp-hal/README.md:36`、`esp-metadata/devices/esp32p4/soc.toml:15`）。v1.x 対応のコントリビューションも受け付けないと明言されている（#3962、#2285 の 2026-03-09 コメント）[E1][E2][E3]。
  - メンテナは「v1.x と v3.x は ROM も CLIC も独自命令セットも違い、別 MCU として扱う必要がある」としている [E2]。
  - **esp32p4 PAC 0.3 / 0.4 が v3 SVD 由来かどうかは、一次情報で明記されていない（未確認）。** 状況証拠は次のとおり。
    - esp-pacs #425（2026-04-16、「P4: Use new base SVD (regenerated from new CSVs)」、本文は「strictly experimental」のみ）以降の SVD は、v3 専用の esp-hal とだけ組み合わせて検証されている。
    - Espressif 公式の `espressif/svd` の `esp32p4.svd` は 2024-01-24 が最終更新で、v3 以前の世代。
    - （推測）PAC 0.3 以降は v3 寄り、0.1 / 0.2（2024 年）は v1 世代の SVD 由来。§1.3 参照。
- **推奨する土台: 自前の M モードカーネル（`riscv` クレート＋自作の起動/トラップ asm）。v1.x 向けのレジスタ定義は ESP-IDF の `hw_ver1` ヘッダを一次情報とし、カーネルが実際に触るペリフェラルの分だけ手書き（または生成）する。esp-hal はロジックの参照にとどめる**（§1.6 で案を比較）。
  - esp-hal をクレートとしてリンクする案は不可。v1.x では CLIC の版、ROM 関数、L2MEM / ROM 予約領域、PMP 配置（後述）が違う。
  - esp32p4 PAC 0.2.0 を v1 用に使う案は、補助的にはあり得る。ただし 2024-04 で更新が止まった、検証されていない SVD であり、`hw_ver1` ヘッダと突き合わせる前提になる。
  - v1.x の PMP は 16 エントリ。ESP-IDF の rev<3 用レイアウトは 0〜15 番だけを使い、v3 用は 27 / 29 番まで使う（`components/esp_hw_support/port/esp32p4/private_include/pmp_layout.h`、`cpu_region_protect.c:255-`）[E12]。粒度は 128B（`soc_caps.h:204`）。TOR ペアにすると U モードアプリに割ける領域は最大 8 個、実際にはカーネル用を引いて 5〜6 個程度（推測）。
  - **ブートローダが PMP / PMA をロックしていく点に注意。** ESP-IDF の `pmp_layout.h` には「rev < 3.0 peripheral entries are locked by shipped bootloaders and must never move」とあり、エントリ 0–2（CPU subsystem RW / CPU peripheral RW / ROM RX）と rev<3 の 13（または 15）番が、出荷済み bootloader によってロックされうる。現行 master の bootloader はロック付きの PMA（無効領域）だけを設定し、PMP は設定しない（`cpu_region_protect.c:449-488` のコメント「the valid (PMP) regions are configured and locked by the application only」）。
    - **手元の v6.0-beta1 bootloader がどちらの挙動かは未確認（U10）。** ロック済みの RW エントリは U モードにも権限を与えるため、もし残っていれば、より若い番号のエントリで U モードを拒否できない限り、ペリフェラル空間が U モードから見えてしまう。起動直後に `pmpcfg0..3` をダンプして確認すること。
  - v3.x に載せ替える場合に限り、esp-hal を M モードのドライバ層として使える。ただし次の手当てが必要: `ESP_HAL_CONFIG_ENABLE_PMP`（デフォルト有効、**ロック付き**の PMP エントリを作る）を無効にする。トラップエントリを U モード対応に差し替える。FPU 等のコプロセッサの退避を足す。
  - **Tock 移植は勧めない。** 理由は次のとおり。(1) Tock の RISC-V コンテキストスイッチは浮動小数点レジスタを保存しない（`arch/riscv/src/syscall.rs` に `fsw`/`flw` が 0 件）。ilp32f ハードフロートの P4 アプリを U モードで動かすには、アーキ層に手を入れる必要がある。(2) Tock の RISC-V アプリは PIC ではなく、アドレス固定で複数ビルドする方式 [T6][T7]。「アプリの動的ロード」には弱い。(3) ESP32-P4 の chip/board は Tock に存在しない（`grep` で 0 件）。(4) ESP32-C3 移植も CPU バグ回避（後述）を含む実験段階の完成度。
  - 一方で、**Tock の設計（TRD104 syscall ABI、PMP 抽象 `TORUserPMP`、TBF ヘッダ、grant、process checker、dynamic process loading）は最良の“借りるべき設計”**。
- **借りる設計の要点**
  - syscall ABI: `ecall`、引数 a0–a3、クラス ID を別レジスタ（Tock は a4、rCore/Linux 流は a7）、戻り値は a0–a3。値の型（Success/Failure の variant）はレジスタで符号化する（Tock TRD104）[T3]。
  - 保護: TOR ペアの PMP で「アプリの flash（RX）」「アプリの RAM（RW）」を U モードに見せ、カーネル領域は見せない。カーネル自体の保護には、Smepmp（`mseccfg.MML`）が使えるならそれを使う（Tock `kernel_protection_mml_epmp`）。P4 の Smepmp 有無は未確認（U3）。
  - アプリ形式: **ELF（PIE）を自前ローダで再配置する**か、Tock 風の「TBF ヘッダ＋固定アドレス複数ビルド」にするか。動的ロードを目標にするなら前者を推奨。
  - 更新: カーネルは ESP-IDF 2nd stage bootloader の OTA パーティション機構に乗る。手元の Tab5 には `ota_0` / `ota_1` / `otadata` が既にある。`esp-bootloader-esp-idf` クレートの `ota` / `ota_updater` は esp-hal 依存なので、形式だけ写す [E9]。アプリはカーネル管理のストレージ領域へ書き込む（Tock の `SequentialDynamicBinaryStorage` [T8]、Xous の detached app [X1] が参考）。
  - **Xous から借りる設計**（§4.4）[X1]
    - syscall は「a0 = 番号、a1–a7 = 引数、戻り値 a0 = Result のタグ、a1–a7 = 値」。整数レジスタ 8 本を往復で使い切る ABI。
    - IPC は Scalar / BlockingScalar（レジスタだけで完結）と、Borrow / MutableBorrow / Move（メモリ貸与）の 5 種。貸与中は送信側からのアクセスを止めるという意味論を、PMP の範囲付け替えで再現する。
    - サーバ（SID）/ 接続（CID）とユーザ空間の名前サービス（xous-names）。
    - アプリはカーネルイメージと独立した「detached app」とし、署名して配布する。
    - syscall 層を差し替えて Linux 上でテストする Hosted モード。
    - 一方、Sv32 ページ単位の貸借、固定仮想アドレスへのコンテキスト配置、全ドライバのユーザ空間化は、PMP 16 エントリでは成り立たないので採らない（推測）。
- **ターゲット: カーネル・アプリとも `riscv32imafc-unknown-none-elf`（ilp32f）で揃える。** これは esp-hal の P4 公式ターゲットでもある [E1]。アプリには `-C relocation-model=pie`（または pic）を付けるか、カスタム target JSON を作る。ESP 独自拡張（Xespv / Xesploop / Xespdsp）は rustc の target feature に存在しないため（rustc 1.99.1 で確認）、カーネル・アプリとも使わない前提にする。
- **ESP-IDF（esp-idf-hal / esp-idf-sys）の上に作る案は、「M/U 分離」という目標と衝突する。** FreeRTOS 全体が M モードで動くため。ただ、ESP-IDF は v1.x P4 を `CONFIG_ESP32P4_SELECTS_REV_LESS_V3` で正式に扱える唯一の SDK でもある。ドライバ（MIPI-DSI 等）のレジスタ手順を読む参照元、bootloader の供給元として使う価値は高い [E12][E13]。

---

## 1. esp-hal / esp-rs の ESP32-P4 対応状況（2026-10 時点）

### 1.1 バージョンと P4 サポートの時系列

| 時期 | 出来事 | 出典 |
|---|---|---|
| 2024-10 | 「Initial P4 support tracking issue」#2285 を作成。当時は v1.0 devkit でのみ println が動作 | [E2] |
| 2025-08 | ロードマップ #3962 で、ECO5（= rev v3.0）待ちとして P4 をブロック | [E3] |
| 2026-03-09 | MabezDev が「NRND（v1.x）の P4 はサポートしないし受け付けない。v1.x には ESP-IDF を推奨」と表明 | [E2] コメント |
| 2026-05-05 | PR #5400「Initial ESP32-P4 support」をマージ（P4X = rev v3.1+ を対象。esp-hal 1.1.0-rc.0 ベース） | [E5] |
| 2026-06-15 | #2285 をクローズ（「I'd consider the P4 supported at this point」） | [E2] |
| 2026-08-24 | esp-hal v1.2.0-rc.0。CHANGELOG に「Initial ESP32-P4 (chip revision v3.0+) support」 | [E6] |
| 2026-09-01/03 | **esp-hal v1.2.0 リリース**（crates.io の最新は 1.2.2、2026-09-18） | [E6][E7] |
| 2026-10 | ロードマップ #3962 に「v1.3 chip support is not planned and contributions to add it will not be accepted.」 | [E3] |

- **ローカルにある esp-hal-1.1.2 には `esp32p4` feature が無い。** `Cargo.toml` の chip feature は esp32 / c2 / c3 / c5 / c6 / c61 / h2 / s2 / s3 のみ（`~/.cargo/registry/src/*/esp-hal-1.1.2/Cargo.toml:153-235`）。`src/` 内の "esp32p4" は 3 箇所のコメントだけ（`rtc_cntl/mod.rs:270,275`, `interrupt/riscv/clic.rs:148`, `analog/adc/riscv.rs:177`）。P4 を使うには **1.2.x 以降が必要**。
- esp-hal main の `Cargo.toml` では、`esp32p4` feature が esp32p4 PAC を git rev 指定で引いている（`esp-hal/Cargo.toml:127,260`）[E1]。
- README の対応表: `ESP32-P4 | riscv32imafc-unknown-none-elf | Revisions below v3.0 are not supported.`（`esp-hal/README.md:36`）[E1]。
- メタデータの冒頭にも `support_note = "Revisions below v3.0 are not supported."` とある（`esp-metadata/devices/esp32p4/soc.toml:15`）[E1]。

### 1.2 ペリフェラル対応（esp-hal main, v3.x 向け）

`esp-metadata/devices/esp32p4/soc.toml` の `support_status` から抜粋した（"partial" は一部対応）[E1]。

| 状態 | ドライバ |
|---|---|
| supported | GPIO, UART, SPI master, I2C master |
| partial | 割り込み, flash, DMA(AXI/AHB GDMA), timergroup, ADC, Ethernet, MIPI-DSI, I2S, LP_I2C, PCNT, RMT, RNG, LP timer, SDMMC(host), SDM, sleep, SPI slave, UHCI, AES/SHA/RSA/ECC, dedicated GPIO |
| not_supported | MIPI-CSI, camera, ISP, PPA, JPEG, H.264, LEDC, MCPWM, TWAI, LP UART, I2C slave, I3C, touch, temp sensor, ULP RISC-V(LP コア), key manager, ETM ほか |
| 個別の記載 | USB OTG FS/HS（CHANGELOG #5558/#5560）、USB Serial/JTAG、PSRAM（AP-HEX、L2 キャッシュ分割を設定可）、マルチコア（#5535） |

- CHANGELOG（v1.2.0-rc.0 の節）: 「P4: Initial peripheral support for GPIO, UART, I2C, SPI, DMA, USB Serial/JTAG, eFuse, SYSTIMER」「Initial MIPI-DSI support」「USB_HS support」「Ethernet: Basic support」など（`esp-hal/CHANGELOG.md:43-150`）[E6]。
- Wi-Fi: Tab5 の Wi-Fi は外付け ESP32-C6（SDIO 経由）[E20]。esp-hal に「P4 から外付け C6 を使う」ホスト側ドライバは見当たらない（未確認 U6）。
- 2026-10 時点の未解決例: #6586「ESP32-P4 v3.0: interrupt dispatch panics on INTR_STATUS_REG_4 bits」（open）[E8]。

### 1.3 PAC（esp-pacs / `esp32p4` クレート）

- crates.io の `esp32p4` は 0.1.0（2024-02-28）、0.2.0（2024-04-17）、0.2.1 / 0.3.0（2026-08-21）、0.4.0（2026-09-15）[E10]。
- esp-pacs では 2026-04-16 に「P4: Use new base SVD (regenerated from new CSVs) (#425)」が入り、以降は v3 向けに更新されている [E11]。
  - #425 の PR 本文は「this is strictly experimental」のみで、どのリビジョンの CSV かは書かれていない [E11]。
  - Espressif 公式の `espressif/svd` リポジトリの `svd/esp32p4.svd` は、2023-11-27 に初版、2024-01-24 に「Add updated SVD for ESP32-P4」が入ったのが最後（GitHub API で確認）[E18]。esp32p4 PAC 0.1 / 0.2（2024-02 / 2024-04）の時期と一致する。
  - （推測）0.2.x 以前は v1.x 世代の SVD、0.3 以降は v3.x 世代。v1.x 用のレジスタ定義が欲しい場合、esp32p4 0.2.0 または ESP-IDF の `components/soc/esp32p4/register/hw_ver1/` が出発点になる。ESP-IDF には `hw_ver1`（`soc/` 配下 188 ファイル）と `hw_ver3`（同 194 ファイル）の 2 ディレクトリがある（esp-idf @9c37a0a, `components/soc/esp32p4/register/`）[E12]。

### 1.4 ランタイム（esp-riscv-rt）とブート方式

- esp-riscv-rt は upstream の `riscv-rt` を、`pre-init, no-exceptions, no-interrupts, single-hart, v-trap, pre-default-start-trap` の features 付きでラップしている（ローカル `esp-riscv-rt-0.14.0/Cargo.toml` の `[dependencies.riscv-rt]`）。`clic-48` feature は CLIC の 48 割り込み向けトラップハンドラを生成する（同 CHANGELOG #4883）。crates.io の最新は 0.15.0（2026-08-24）[E7]。
- **ブートは ESP-IDF 2nd stage bootloader に載る app image 方式**（direct boot ではない）。
  - P4 のリンカスクリプト `esp-hal/ld/esp32p4/memory.x:38` は `ROM : ORIGIN = 0x40000000 + 0x20`。コメントは「External flash (XIP via cache); +0x20 skips the IDF app image header」[E1]。
  - 同ファイルのコメントによれば、2nd stage bootloader が L2MEM のキャッシュ/RAM 分割（`CACHE.L2_CACHE_CACHESIZE_CONF`）を設定し、esp-hal 側は `ESP_HAL_CONFIG_L2_CACHE_SIZE` で合わせる。ROM BSS/stack 用に L2MEM 上端約 72KB を避ける（`0x4FFAE000` 以上）[E1]。
  - `esp-bootloader-esp-idf` クレート（crates.io 0.6.0、2026-08-26）は、IDF bootloader が読む `esp_app_desc` を `esp_app_desc!` マクロで埋め込む。ほかに `partitions`, `ota`, `ota_updater` モジュールを持つ（`esp-bootloader-esp-idf/src/lib.rs:125-131,377`）[E9]。`esp32p4` feature あり（同 `Cargo.toml:98`）。
- 参考: Tock の ESP32-C3 は direct boot（magic `0xaedb041d` ×2 で ROM が flash を 0x42000000 にマップして直接ジャンプ）を使う [T5]。NuttX の Tab5 は「SIMPLE_BOOT」でオフセット 0x2000 に書く [E20]。P4 v1.x の ROM が direct boot / simple boot に対応するかどうかは、NuttX の実例から（推測）対応している。

### 1.5 esp-hal を M モードカーネルの部品として使う場合の注意

- **PMP をロック付きで占有する**: `esp-hal/src/lib.rs:762-763` で `#[cfg(all(feature = "rt", enable_pmp, riscv))] crate::soc::enable_pmp();` が呼ばれる。中身は `.rwtext` を X-R、`.stack` を -WR として TOR で保護し、**pmpcfg に L ビット（bit7）を立てる**（`esp-hal/src/soc/mod.rs:364-540`、特に 419-420 行 `// Off, no permissions, locked` / `// TOR, locked`）[E1]。L ビットはリセットまで解除できず、M モードにも効く。U モードアプリ用の PMP 管理と衝突するので、`ESP_HAL_CONFIG_ENABLE_PMP=false` にすること（`esp-hal/esp_config.yml:240-244`、デフォルト true、`riscv && !esp32c2 && !esp32c3` で有効）[E1]。
- P4 の PMP 粒度は 128 バイト（`soc.toml` の `cpu_pmp_granularity = 128`、ESP-IDF `soc_caps.h` の `SOC_CPU_PMP_REGION_GRANULARITY 128`）[E1][E12]。アプリ領域は 128B アラインで割り当てる必要がある。エントリ数は rev<3 で 16 個、v3 は ESP-IDF が 27 / 29 番まで使うことから 16 個より多い（`pmp_layout.h` [E19]、v3 の正確な数は未確認）。
- esp-hal は割り込みコントローラに CLIC（48 割り込み、8 優先度）を使う前提（`soc.toml` の `controller = { Riscv = { flavour = "clic", interrupts = 48, priority_levels = 8 } }`）[E1]。v1.x の CLIC は版が違い（PR #5400 レビューで「CLIC v1.x issue compare to C5/C61's CLIC v2.x」）、そのままでは流用できない [E2]。
- Zcmp ハードウェアバグ回避として、P4 では `mintthresh`（CSR 0x347）を 0xff にしてから `mstatus.MIE` を落とす（`esp-sync/src/raw.rs:38-45`、PR #5400 本文）[E5]。PR 本文によれば `riscv32imafc-unknown-none-elf` では rustc が Zcmp 命令を 0 個しか出さないが、ROM 等の他コードに対する安全策として入っている。

### 1.6 v1.x の Tab5 向けにレジスタ定義と HAL をどう用意するか

| 案 | 内容 | 利点 | 欠点 / リスク | 評価 |
|---|---|---|---|---|
| A. ESP-IDF `hw_ver1` ヘッダから手書き / 生成 | `components/soc/esp32p4/register/hw_ver1/soc/*_reg.h`（`*_struct.h`）を一次情報に、カーネルが触るブロック（UART, HP_SYS / LP_SYS clock, IO_MUX / GPIO, SYSTIMER, CLIC / INTMTX, I2C, MIPI-DSI, PSRAM / cache）だけを Rust 化する。`*_reg.h` の `#define` を簡易スクリプトで `const` に変換、または `tock-registers` / `volatile-register` で手書き | v1.0 / v1.3 の実機で ESP-IDF が実際に使っている値と一致する。必要な分だけ作るので小さい | 作業量。`hw_ver1` 内でも v1.0 と v1.3 の差分は `chip_revision` で分岐している箇所がありうる（未確認） | **推奨（主）** |
| B. v1 用 PAC（esp32p4 0.2.0 を固定利用、または `espressif/svd` 2024-01 版から svd2rust で自前生成） | 既存 PAC をそのまま使う | 全ペリフェラルの型付き API がすぐ揃う | SVD の正確さが未検証（esp-hal 側は v1.0 devkit で println 程度までしか確認していない [E2]）。保守されていない | 補助（A と突き合わせて使うなら可） |
| C. esp-hal（v3 用）をロジック参照に | クロックツリーの扱い、DMA 記述子、MIPI-DSI 初期化シーケンス、`enable_pmp` や Zcmp 回避などの実装パターンを読んで移植する | 実装済み Rust コードの書き方・API 設計を学べる | v1 とレジスタやビット位置、ROM のアドレスが違う。そのままコピーすると壊れる | **参照用として推奨** |
| D. esp-hal をそのままリンク | – | – | v1.x では起動もしない（#6179, #5968 の実例 [E4]） | 不可 |

- ブートローダ: ESP-IDF の rev<3 用 2nd stage bootloader（手元は v6.0-beta1。espflash 4.4+ も rev<300 用を同梱 [E14]）に、app image（`esp_app_desc` 付き）として載せるのが最短。
  - `esp-bootloader-esp-idf` クレートは esp-hal に依存する（`esp-bootloader-esp-idf/Cargo.toml:98` の `esp32p4` feature が `esp-hal/esp32p4` を要求）。v1 では、`esp_app_desc` 構造体（`src/lib.rs:143-202`）と OTA データ形式を**自前で写す**ことになる（推測）。
  - 手元の Tab5 には `otadata` / `ota_0` / `ota_1` パーティションが既にある（`boot.log` の partition table）。カーネル更新に OTA 方式をそのまま使える。

### 1.7 ツール（espflash / probe-rs）

- espflash（crates.io 最新 4.6.0、2026-09-10）[E14]
  - 3.2.0: 「ability to read chip revision for ESP32-P4 (#686)」
  - 4.4.0（2026-04-16）: 「**esp32p4rc1 support for esp32p4 chips with revision < 3.0 (#922)**」「Added ESP32-P4 rev < 300 bootloader (#1016)」「Support for binaries with INIT_ARRAY sections」
  - 4.5.0: 「Update ESP32-P4 rev >= 300 bootloader」
  - Unreleased: P4 の uncached RAM alias へのセクション配置（#1096）
  - → **espflash は v1.x の P4 にも書き込めるし、v1.x 用 bootloader も同梱している**。ただし #6179 では、esp-hal でビルドした app が v1.3 上で bootloader からジャンプした直後に止まっている [E4]。
- probe-rs: 「esp32p4: add initial support」PR #3649 で P4 対応（#2285 の 2026-04-23 のコメント）[E2]。v1.x で使えるかは未確認（U2）。最新リリースは 0.32.0（2026-07-22）。

---

## 2. ターゲット triple と ABI

### 2.1 `riscv32imafc-unknown-none-elf` は妥当か

- rustc 1.99.1 の target spec（`--print target-spec-json`）: `features: "+m,+a,+c,+f"`、`llvm-abiname: "ilp32f"`、`relocation-model: "static"`、`panic-strategy: "abort"`、`max-atomic-width: 32`、tier 2（ローカルで確認）。
- esp-hal 公式の P4 ターゲットであり（`soc.toml:11`）[E1]、ESP-IDF の rev<3 向け `-march=rv32imafc_zicsr_zifencei`（＋PIE 有効時は `_xespv2p1`）と基本 ISA が一致する（esp-idf `components/soc/project_include.cmake:21-22,48,70-74`）[E12]。
- v3.x 向けの ESP-IDF は `rv32imafcb` ＋（Zcmp バグが無い場合）`_zcb_zcmp_zcmt` ＋ `_xesploop` ＋ `_xespv` ＋ `-mabi=ilp32f`（同ファイル 23-26, 50-84 行）[E12]。v3.x で B 拡張（Zba/Zbb/Zbs）を使いたければ `-C target-feature=+zba,+zbb,+zbs` を足せる（rustc の target-features 一覧に存在）。v1.x では使わないこと（ESP-IDF が v1.x に `b` を付けていないため）。
- 結論: **カーネル・アプリとも `riscv32imafc-unknown-none-elf` が適切。** v1.x / v3.x のどちらにも安全な共通部分集合になっている。

### 2.2 ESP 独自拡張（Xespv = PIE / Xesploop / Xespdsp）

- rustc 1.99.1 の `--print target-features` に `xesp*` は存在しない（CORE-V の `xcv*` と WCH の `xwchc` はある）。ローカルで確認。
- ESP-IDF 側の記述: 「PIE coprocessor assembly is only supported with GCC compiler」（`soc_caps.h` の SOC_CPU_HAS_PIE のコメント）。Clang 向けには Espressif の LLVM fork 専用オプションを使う（`tools/cmake/llvm_optimizations.cmake:17-22`）[E12]。
- → Rust からは使えない（`asm!` で `.insn` を直書きすれば可能だが、（推測）実用的でない）。**OS としてはこれらのコプロセッサを「アプリに使わせない」方針にする。** その場合も、ROM 関数や IDF bootloader がこれらを有効のまま残さないかは確認が要る（U4）。
- ESP-IDF の P4 には `SOC_CPU_COPROC_NUM 3`（FPU / HWLOOP / PIE）、`SOC_CPU_HAS_FPU_EXT_ILL_BUG`（EXT_ILL CSR が FLW/FSW を扱えない）、`SOC_CPU_HAS_HWLOOP_STATE_BUG` がある（`soc_caps.h:187-194`）[E12]。FPU の遅延退避を `mstatus.FS` で行う場合も、これらの癖に注意する。

### 2.3 ハードフロート ABI とカーネル/アプリ間 ABI

- ilp32f では f0–f31 のうち fa0–fa7 が引数、fs0–fs11 が callee-saved。U↔M のトラップでは、**syscall 境界を整数レジスタのみで定義すれば、関数呼び出し ABI の一致は不要**（推測ではなく設計上の帰結）。
- ただし、プリエンプションによるタスク切替では f0–f31 と fcsr の退避が必須。riscv-rt のデフォルトトラップは f レジスタを保存しない（§3.3）。Tock も保存しない（§4.1）。
- アプリと kernel で ilp32 / ilp32f が混在すると、リンク時に ELF の `e_flags`（float ABI）が不一致になる。**アプリも ilp32f に統一する**。

### 2.4 U モードアプリ用ターゲット（PIC/PIE）

- rustc は `riscv32imafc-unknown-none-elf` に対して `-C relocation-model=pie|pic|ropi|rwpi|ropi-rwpi` を受け付ける（`rustc --print relocation-models` で確認）。RISC-V で意味があるのは `pie` / `pic`。ROPI/RWPI は ARM 向けで、RISC-V LLVM は未対応（推測）。
- 方式の候補
  1. **PIE ELF ＋ カーネル内ローダ**: `-C relocation-model=pie -C link-arg=-pie`（＋`--no-dynamic-linker`、静的 PIE）でリンクする。ローダは `R_RISCV_RELATIVE` だけ処理すればよい。P4 は flash を MMU でページ（64KB）単位マップするため（`soc.toml` の `[device.mmu] page_size = 0x10000`）[E1]、アプリの text を flash 上で XIP する場合は「64KB 境界にロード → 再配置は .data/.got のみ RAM で」という設計が可能（推測）。
  2. **カスタム target JSON**: `relocation-model: "pie"`、`position-independent-executables: true`、`static-position-independent-executables: true` を入れた `riscv32imafc-tab5app-none-elf.json` を作る（nightly で `-Z build-std=core,alloc` が必要）。stable で済ませたいなら、1 の rustflags 方式が楽。
  3. **固定アドレス複数ビルド（Tock 方式）**: RISC-V の libtock-c / libtock-rs は PIC 未対応で、ボードごと・スロットごとにアドレス固定でビルドしている（libtock-c `Configuration.mk:82-84` の `ESP32_C3_TOCK_TARGETS` に 3 スロット、libtock-rs README:10-15）[T6][T7]。動的ロードの柔軟性が低いため非推奨。

---

## 3. `riscv` / `riscv-rt` クレート

（ローカル: riscv 0.15.0, riscv-rt 0.16.0。crates.io の最新は riscv 0.16.1 / riscv-rt 0.18.0（2026-05-28）。esp-riscv-rt 0.14 は riscv 0.15 / riscv-rt 0.16 に依存。）

### 3.1 M モード前提の設計

- riscv-rt はデフォルトで M モード（`mtvec`, `mret`）。`s-mode` feature で S モード用に切り替える（`riscv-rt-0.16.0/Cargo.toml` の features `s-mode`、`src/lib.rs:568`）。
- `_start` → `__pre_init` → RAM 初期化 → `main`。トラップベクタは `v-trap` が無ければ direct mode で `_start_trap` に設定される（`riscv-rt-0.16.0/src/lib.rs:710-732`）。

### 3.2 U モードへ降りる API

- `riscv::register::mstatus::set_mpp(MPP::User)`（`riscv-0.15.0/src/register/mstatus.rs:408`、unsafe）、`mepc::write`、`mstatus::set_mpie` → `asm!("mret")`。`riscv` クレートに `mret` のラッパは無いので、インラインアセンブリで書く（コンテキスト全体を復元してからの `mret` なので、いずれにせよ自前 asm になる）。
- `riscv::asm::ecall()` がある（`riscv-0.15.0/src/asm.rs:61`）。U モード側の syscall スタブに使えるが、引数レジスタを指定するため、実際には `asm!("ecall", inlateout("a0") ..., in("a7") ...)` を自前で書くことになる（rCore 方式 [R1]）。

### 3.3 PMP CSR アクセス API

- `riscv::register::{pmpcfg0..pmpcfg3, pmpaddr0..pmpaddr15}`（`riscv-0.15.0/src/register/pmpcfgx.rs:113-`）。`Permission`（NONE/R/W/RW/X/RX/WX/RWX）と `Range`（OFF/TOR/NA4/NAPOT）の列挙、`set_pmp(index, range, permission, locked)` / `clear_pmp(index)` マクロ生成関数がある。
- Tock は独自の CSR ラッパで同様のことをしている（`tock/arch/riscv/src/pmp.rs`、2669 行）。

### 3.4 トラップハンドラと ecall

- riscv-rt のデフォルト `_start_trap`（`riscv-rt-macros-0.6.1/src/lib.rs` の `default_start_trap`、601-632 行付近）は **現在の `sp` 上に caller-saved だけの `TrapFrame` を積み**、`_start_trap_rust` を呼んで `mret` で戻る。`TrapFrame` は ra, t0–t6, a0–a7 のみ（`riscv-rt-0.16.0/src/lib.rs:743-`）。
  - **U モードからのトラップでは `sp` がユーザスタックのままなので、そのままでは使えない**（ユーザが不正な sp を置けばカーネルが落ちる）。`mscratch` との `csrrw sp, mscratch, sp` によるスタック切替と、s0–s11 / f0–f31 / fcsr を含むフル退避を自前で書く必要がある。Tock の `arch/riscv/src/syscall.rs:359-` の `switch_to_process` と `_start_trap` が、そのまま参考になる [T2]。
- 例外の振り分けは `_dispatch_exception` → `UserEnvCall` 等の名前付きハンドラ（`riscv-rt-0.16.0/src/exceptions.rs:25,44,61`）。esp-riscv-rt は `no-exceptions` で `_dispatch_exception` を自前で定義し、`_start_trap_rust_hal` に渡す（`esp-riscv-rt-0.14.0/src/lib.rs:23-29`）。`pre-default-start-trap` 機構で `_pre_default_start_trap` に CLIC 用の前処理を挟んでいる（同 97-141 行）。
- esp-hal の RTOS 用コンテキストスイッチ（`esp-hal/src/interrupt/__rtos_implementation.rs:243` 付近、`mscratch` に t0 を退避して `mret`）は M モード内のタスク切替用で、U モードは想定していない [E1]。
- → **riscv-rt は「ブート＋RAM 初期化」までを使い（または自作）、トラップエントリは自作に差し替える**のが現実的。`riscv` クレートは CSR アクセサとして全面的に使える。

---

## 4. 既存 Rust 製 OS/RTOS の設計

### 4.1 Tock OS（最重要の参考先）

- 最新は Tock 2.3.0（2026-10-08）、その前が 2.2（2025-01-06）[T0]。
- **特権分離**: カーネルは M モード、プロセスは U モード。`arch/riscv/src/syscall.rs:773-792` で、`mcause` が `UserEnvCall | MachineEnvCall` なら `state.pc += 4` して、`a4`（クラス）, `a0..a3` から syscall を組み立てる [T2]。
- **syscall ABI（TRD104）** [T3]
  - RISC-V ではクラス ID が `a4`（`trd104-syscalls.md:103-107`）、引数と戻り値が `a0`–`a3`。
  - クラス: 0 Yield, 1 Subscribe, 2 Command, 3 Read-Write Allow, 4 Read-Only Allow, 5 Memop, 6 Exit（同 368-374 行）。Yield と Exit 以外は非ブロッキング。
  - Subscribe / Command / Allow は (driver number, sub number) で多重化する。Command 0 は「driver が存在するか」の予約。
  - 戻り値は a0 に variant（Success / Failure / Success_u32 ...）、a1–a3 に値。
  - upcall（コールバック）は Yield 時にのみ配送する。
- **保護（PMP）** [T1]
  - `TORUserPMP<MAX_REGIONS>` トレイト（`pmp.rs:880`）、`PMPUserMPU`（`pmp.rs:1027`）、実装として `simple`（`pmp.rs:1608`）、`kernel_protection`（`pmp.rs:1829`）、`kernel_protection_mml_epmp`（Smepmp, `pmp.rs:2178`）がある。
  - カーネル共通 API は `kernel/src/platform/mpu.rs`（`allocate_app_memory_region`:239, `configure_mpu`:302 など）。
  - プロセスごとに「flash（RX）」「RAM（RW、grant 領域を除く）」を TOR ペアで設定する。
- **ESP32-C3 ポート** [T4][T5]
  - `boards/esp32-c3-devkitM-1` と `boards/esp32-c3-devkit-rust-2`、`chips/esp32-c3`（chip, flash, i2c, intc, rng, sysreg, usb_serial_jtag 程度）がある。target は `riscv32imc`（カスタム JSON、`boards/esp32-c3-devkitM-1/.cargo/config.toml:13`）。
  - direct boot、XIP。カーネルは 0x42000000、アプリは 0x42100000 から（`layout.ld`, `Makefile`）。
  - **CPU バグの回避**: U モードからトラップを受けた際に `mtvec` のベクタ表が U モードから X 可でないとフォルトするため、PMP 16 エントリのうち最後の 2 つを、ベクタ表に対する「TOR, X のみ」に使い、ユーザ用は 14 エントリ = 7 リージョンに減らしている（`chips/esp32-c3/src/chip.rs:34-53, 131-165`）[T4]。
    - （推測）同系統の Espressif コアである P4 にも同種の癖がある可能性がある。実機で要検証（U5）。
  - 完成度: ペリフェラルは最小限。Wi-Fi/BLE は無し。Tock 公式の hardware CI 対象かは未確認。
- **プロセスロードと形式**
  - TBF（Tock Binary Format）ヘッダ＋バイナリを flash に連結し、起動時に `kernel/src/process_loading.rs` が走査する。署名・ハッシュは `process_checker.rs`、AppID は `trd-appid.md`。
  - 動的ロードは `kernel/src/dynamic_binary_storage.rs`（`SequentialDynamicBinaryStorage`:178）。TRD「Dynamic Process Loading」は Draft（2025-03-05）[T8]。non-XIP（RAM 実行）は `trd-non-xip.md`（Draft）。
  - RISC-V アプリは PIC ではない（§2.4）[T6][T7]。
- **FPU**: `arch/riscv/src/syscall.rs` に f レジスタの保存処理が無い（`grep -c 'fsw|flw|fcsr'` = 0）。ESP32-C3 は F 無し、Tock の RISC-V ボードはすべて rv32imc / rv32imac。
- **ESP32-P4 への移植性（評価）**
  - chip crate（CLIC 割込み、P4 の clock / IO_MUX / UART、L2MEM マップ、flash MMU）の新規作成が必要。Tock には汎用の `arch/riscv/src/clic.rs` がある。
  - アーキ層への FPU 退避の追加。
  - v1.x の PMP / トラップの癖の検証。
  - アプリ側（libtock-rs）の ilp32f 対応と固定アドレスビルド。
  - （推測）工数は「自前カーネル＋Tock の設計を写す」と同等以上。しかも Tock の設計判断（静的 grant、upcall モデル）に縛られる。

### 4.2 Hubris（Oxide）

- 特徴（[H1] `doc/*.adoc`）
  - タスク構成はビルド時に静的に決める（`app.toml`）。動的ロードは設計上やらない。
  - syscall は同期 IPC（`SEND`(0), `RECV`(1), `REPLY`(2)、ほかに borrow / lease、notification、IRQ 制御など）。
  - タスクは MPU で分離した非特権モード。
  - ARMv6/7/8-M の ABI は「引数 `r4`–`r10`、syscall 番号 `r11`、戻り値 `r4`–`r11`」（`doc/syscalls.adoc:47-61`）。
- **RISC-V**: `doc/syscalls.adoc:74-76` は「Syscalls are invoked using the `ECALL` instruction. The rest is TBD.」のみ。`sys/kern/src/arch/` は `arm_m.rs` だけで、RISC-V 実装はリポジトリ main に存在しない（@c611e20）[H1]。
- 参考になる点: 「タスクが落ちてもスーパーバイザが再起動する」障害モデル、lease による安全なバッファ貸与（Tock の Allow に相当）、カーネルを小さく保つ方針。

### 4.3 Embassy / RTIC（参考）

- いずれも単一イメージ・単一特権レベル（全コードが M モード、または Cortex-M の特権スレッド）で動く協調/割込み駆動フレームワーク。メモリ保護、別ビルドのアプリのロード、syscall 境界という概念が無い [O1][O2]。
- esp-hal 自体も async（Embassy 互換）前提の HAL で、esp-rtos はスケジューラを提供するが、特権分離は無い。
- → 不向き。ただし **カーネル内部のドライバ実装スタイル（async ドライバ、`embedded-hal` トレイト）にはそのまま使える**。

### 4.4 Xous（betrusted-io/xous-core）

参照: xous-core `main` @ `e8d989c754bcc3a9cd66f9e3e1c70cac580731eb`（2026-10-26）[X1]。Precursor（FPGA 上の VexRiscv RV32IMAC＋Sv32 MMU）と Baochip（bao1x）向けのメッセージパッシング型マイクロカーネル。rustc には Tier 3 ターゲット `riscv32imac-unknown-xous-elf` がある（`rustc --print target-list` で確認）。**Sv32 前提なので P4（PMP のみ）へはそのまま移植できない**という前提で、設計を読む。

#### 特権構成とブート
- **loader（M モード）→ kernel（S モード）→ プロセス（U モード）**。
  - loader は `mideleg` / `medeleg` で割り込みと例外を S に委譲し、`satp` を設定して `mret` で kernel へ入る（`loader/src/asm.rs:270-296`）。
  - kernel のトラップは `stvec` / `sscratch` / `sret`（`kernel/src/arch/riscv/asm.S:29,87,196,270`）。
- トラップ入口では `csrw sscratch, sp` → `li sp, 0xff801000` とし、**全プロセス共通の固定仮想アドレス `0xff801000`（スレッドコンテキスト領域）** にスレッド番号×128B のオフセットで x1–x31 と `sepc` を保存する（`asm.S:84-135`）。
  - アドレス空間を切り替えると、この固定アドレスが自動的に各プロセスのコンテキストページを指す。MMU に強く依存したトリック。
- `ecall`（`scause` 8 = U, 9 = S）の処理では `sepc += 4` する（`kernel/src/arch/riscv/irq.rs:246-253`）。
- プロセスごとのレイアウト（`docs/memory.md:38-65`、`kernel/src/arch/riscv/mem.rs:134-137`）
  - ページテーブルは `0xff400000`、プロセス固有データ（ルートページテーブル）は `0xff800000`。
  - カーネルは 4MB メガページ 1023 番（`0xffc00000`）として全プロセスにマップする。

#### syscall ABI
- `ecall`。**a0 = syscall 番号、a1–a7 = 引数 7 個。戻り値は a0 = 戻り値の種別タグ（`Result` の variant）、a1–a7 = 値**（`docs/syscalls.md:11-39`、`xous-rs/src/arch/riscv/syscall.rs:18-31`）。
  - 通常の呼出規約では戻り値は a0 / a1 だけだが、Xous は 8 個まで返すため、`inlateout` で a0–a7 を全部受ける。
- `SysCallNumber`（`xous-rs/src/syscall.rs:581-632`）の主なもの
  - メモリ: MapMemory 2, IncreaseHeap 10, UpdateMemoryFlags 12, UnmapMemory 19, ReturnMemory 20
  - スケジューリング: Yield 3, WaitEvent 9, CreateThread 18, JoinThread 36
  - 割り込み: ClaimInterrupt 5, FreeInterrupt 6
  - IPC: CreateServerWithAddress 14, ReceiveMessage 15, SendMessage 16, Connect 17, TrySendMessage 24, ReturnScalar1/2 26/27, CreateServer 29, ReturnScalar5 40, ReplyAndReceiveNext 41
  - プロセス: CreateProcess 21, TerminateProcess 22
  - その他: SetExceptionHandler 37, VirtToPhys 39, PlatformSpecific 46 など、40 余り
  - **割り込みもユーザ空間サーバが ClaimInterrupt で受ける**（ドライバはユーザ空間）。

#### メッセージパッシング
- `Message` は 5 種類（`xous-rs/src/definitions/messages/mod.rs:111-117`）
  - `Scalar`（id＋4 ワード、非同期）
  - `BlockingScalar`（同期、ReturnScalar で応答）
  - `Move`（ページ所有権の移動）
  - `Borrow`（読取専用の貸出、同期）
  - `MutableBorrow`（書込み可の貸出、同期）
- メモリ系メッセージは `MemoryMessage { id, buf: MemoryRange, offset, valid }` で、**4096B 境界・ページ単位**。
- 貸出の実装（`kernel/src/arch/riscv/mem.rs:897-`、`lend_page_inner`）
  - 送信側 PTE の VALID を落とし、ソフトウェア定義の SHARED ビットを立てる。**貸出中は送信側からアクセスできない**。受信側のアドレス空間に同じ物理ページをマップする。
  - 返却（ReturnMemory）で元に戻す。二重貸しは `ShareViolation` になる。
  - Move は所有権ごと移す（`kernel/src/mem.rs:820-875`）。
- **サーバと名前解決**: サービスは `SID`（128bit）を持つサーバを作り、クライアントは `Connect` で `CID` を得る。
  - 名前 → SID の解決はユーザ空間サービス `xous-names` が担う。サーバ側は `register_name(name, max_conns)`、クライアント側は `request_connection(name)`（`api/xous-api-names/src/lib.rs:62,125`）。
  - 上位層の `xous-ipc` は rkyv で構造体をページバッファに直列化し、Lend / Move で送る。
  - ドライバ類（`services/` 配下の graphics-server, keyboard, net, pddb, xous-ticktimer, xous-log など）はすべてユーザプロセス。

#### loader・イメージ形式・プロセス起動と更新
- イメージは `tools/src/bin/xous-create-image.rs` が作る「タグ付き引数ブロック」形式（`docs/arguments.md`）。
  - `XKrn`: カーネル位置とサイズ。
  - `IniE`: 初期プロセス。退化した ELF（MiniElf）で、セクションごとのロードオフセットとサイズを持つ。
  - `IniF`: IniE と同じだが、ページ内オフセットが仮想アドレスと 1:1 になるように配置し、**XIP（flash から直接実行）でマップできる**。
  - `IniS`: swap 用。
  - loader はこれを解釈してページテーブルを組み、`.data` / `.bss` をコピーする（`loader/src/main.rs:153-157,884-1042`、`loader/src/minielf.rs`）。
- 初期プロセスはイメージに同梱する。追加アプリは **「detached app」**: カーネルイメージに触らずに別領域へ置くスタンドアロンアプリで、`xous-create-detached-app` で作る。ローダが署名を検証する（`README-baochip.md:82,111`、`loader/src/main.rs:269-451`）。
- 実行時のプロセス生成は `CreateProcess`（`ProcessInit`）syscall。
- 署名ツールは `xous-sign-image` / `xous-pq-sign-image`（ポスト量子署名）。

#### Hosted モード
- `cargo xtask` の hosted 構成（`xtask/src/main.rs:417-459`）では、**カーネルを Linux 上の 1 プロセスとして動かし、各 Xous プロセスを Linux の通常プロセスとして起動する。** syscall は TCP 経由でカーネルに送る（`xous-rs/src/arch/hosted/mod.rs`。環境変数 `XOUS_SERVER` で接続先を受け取る）。
- syscall 層（`xous-rs/src/arch/{riscv,hosted,arm}`）の差し替えだけで、サービス群をホスト上でテストできる。
- → **本プロジェクトでも「syscall スタブを trait / feature で差し替え、アプリとカーネルロジックを x86_64 Linux 上でテストする」構成は流用価値が高い**（推測: カーネルの syscall ディスパッチ部分を `no_std` の純ロジックに分離しておけば、ホストの `cargo test` で検証できる）。

#### PMP のみ（単一アドレス空間・物理アドレス）で再現する場合（推測を含む）

| Xous の仕組み | PMP のみでの扱い |
|---|---|
| プロセスごとのアドレス空間（Sv32） | **諦める**。全プロセスが物理アドレス直の単一空間に住む。アプリは PIE で任意アドレスにロードする（§2.4）（推測） |
| 固定アドレス `0xff801000` のコンテキスト保存 | **流用不可**。`mscratch` にカレントスレッドの TCB ポインタを置き、`csrrw sp, mscratch, sp` で切り替える方式（Tock 方式）に置き換える（推測） |
| Lend / Borrow（ページ単位で送信側を無効化し、受信側にマップ） | 受信側の PMP エントリに送信側バッファの範囲（TOR、**128B 粒度**）を一時的に追加し、送信側の PMP からは外す。仮想アドレスが変わらないため「マップ先アドレス」の概念は不要になる。代わりに、**PMP エントリ数（16、TOR で 8 領域）が同時に貸せるバッファ数の上限になる**。受信側の領域を分断しないよう、バッファは 128B アラインの連続領域に限る（推測） |
| Move（所有権移動） | 物理範囲の所有者メタデータを付け替えるだけで実現できる。ただし断片化しやすい（推測） |
| Scalar / BlockingScalar | **そのまま流用可**。レジスタだけで完結し、MMU に依存しない |
| サーバ / SID / CID / xous-names | **そのまま流用可**（カーネル内のテーブルとユーザ空間の名前サービス） |
| ユーザ空間ドライバ（ClaimInterrupt） | 原理的には可能。ただしペリフェラルの MMIO 範囲もドライバプロセスの PMP に載せる必要があり、エントリ数を圧迫する。P4 のペリフェラルアクセスは APM/TEE 等の別機構で制御できる可能性がある（未確認）（推測） |
| IniF（XIP マップ） | P4 の flash MMU（64KB ページ）で「アプリ text を任意の flash 位置から固定仮想アドレスへ」マップできる。これを使えば、MMU なしでも text だけは疑似的な固定アドレス化が可能（推測） |
| 8 戻り値の syscall ABI、タグ付き Result | **流用可** |
| Hosted モード | **流用可**（上記） |
| swap、メモリ暗号化等 | 諦める（Precursor 固有） |

- 結論（推測）: Xous は「マイクロカーネル＋ユーザ空間サーバ＋IPC」という**設計思想と API 形状**を借りる先。メモリ管理と IPC のゼロコピー部分は、PMP の 8 領域制約に合わせて大幅に縮退させる必要がある。P4 規模で全ドライバをユーザ空間に出すと PMP エントリが足りなくなるため、**「ドライバはカーネル内（Tock 流）、アプリ間・アプリ↔サービス間の IPC は Xous 流の Scalar / Borrow」**というハイブリッドが現実的。

### 4.5 その他

| OS | 概要 | 本プロジェクトへの関係 |
|---|---|---|
| rCore-Tutorial-v3 [R1] | RV64、RustSBI(M) ＋ カーネル(S) ＋ アプリ(U)、Sv39 ページング。syscall は `ecall`、番号 `a7`(x17)、引数 `a0..a2`、戻り値 `a0`、Linux 風の番号（write=64, exit=93, yield=124, exec=221...）。ELF ローダあり（`user/src/syscall.rs`, `os/src/syscall/mod.rs`）。最終 push 2026-07-03 | **U モードへの降り方、TrapContext、ELF ロード、syscall 表の教材として最適**。ただし S モードと MMU 前提なので、PMP ＋ 物理アドレス直の P4 には「仮想メモリ抜き」で読み替える必要がある |
| xv6-riscv（C） | M モードで最小初期化 → S モードカーネル → U。trampoline / trapframe 設計 | uservec/userret のレジスタ退避手順が参考になる（MMU 前提部分は除く） |
| Theseus / Redox / Hermit | Theseus は単一アドレス空間＋言語安全性による分離。Redox は MMU 前提のマイクロカーネル（x86_64 / aarch64 / riscv64）。Hermit はユニカーネル | MMU 無し・PMP のみの P4 には設計前提が合わない（参考程度） |
| NuttX（C） | **Tab5（rev v1.0）を公式ボードとしてサポート**。UART, USB-Serial-JTAG, I2C, PSRAM, GPIO, IO エキスパンダ, MIPI-DSI, ST7121/ST7123 パネル。SIMPLE_BOOT で 0x2000 に書き込む [E20] | **v1.x P4 の初期化手順（クロック、PSRAM、DSI）とブート方式の生きた参照実装**。NuttX は RISC-V で PMP による protected build（カーネル / ユーザ分離）を持つ（他チップでの実績。P4 で有効かは未確認 U7） |
| ESP-IDF ESP-TEE | C6 等で TEE / REE を分離する Espressif の枠組み [E15] | P4 対応や権限モードの詳細は今回確認できず（U7） |

### 4.6 比較表

| | Tock 2.3 | Hubris | Xous | rCore-Tutorial-v3 | Embassy / RTIC | 本プロジェクト案 |
|---|---|---|---|---|---|---|
| 特権分離 | カーネル M / アプリ U（RISC-V） | カーネル特権 / タスク非特権（ARM のみ） | loader M / カーネル S / プロセス U | M(SBI) / S(カーネル) / U | 無し | カーネル M / アプリ U |
| 保護機構 | PMP（TOR ペア、Smepmp 対応）、grant | ARMv7/8-M MPU | Sv32 ページング（プロセスごとのアドレス空間、4KB ページ） | Sv39 ページング | 無し | PMP（128B 粒度、TOR）、Smepmp は要確認 |
| syscall ABI | `ecall`、class=a4、args a0–a3、TRD104（7 クラス） | ARM: r4–r10 引数、r11 番号。RISC-V は TBD | `ecall`、番号=a0、args a1–a7、戻り値 a0=タグ＋a1–a7（約 46 番まで） | `ecall`、id=a7、args a0–a2、ret a0 | – | `ecall`、id=a7（または a4）、args a0–a5、ret a0–a1（推奨） |
| IPC | upcall＋Allow（カーネルドライバ経由） | 同期 send / recv / reply＋lease | Scalar / BlockingScalar / Move / Borrow / MutableBorrow（ページ単位）、名前解決は xous-names | pipe 等（教材） | – | Scalar＋Borrow 型（PMP で範囲貸与） |
| 動的ロード | あり（TBF、dynamic process loading TRD は Draft）。RISC-V は非 PIC | なし（静的構成） | あり（CreateProcess、detached app、MiniElf / IniE / IniF） | あり（ELF、MMU で配置自由） | なし | PIE ELF ＋ R_RISCV_RELATIVE 再配置 |
| 更新 | tockloader でアプリ単位、カーネルは別書込み | イメージ一括 | 署名付きイメージ＋detached app を個別更新 | – | イメージ一括 | カーネル: IDF OTA（`esp-bootloader-esp-idf::ota`）、アプリ: カーネル管理領域 |
| RISC-V / ESP 対応 | rv32i 系多数、ESP32-C3 あり（CPU バグ回避込み）、**P4 なし**、FPU 退避なし | RISC-V 未実装 | VexRiscv（Precursor）/ bao1x、ARM（atsama5d27）、Hosted（Linux）。ESP なし、MMU 必須 | QEMU virt / K210 等（RV64） | esp-hal（P4 v3 は esp-hal 1.2〜） | P4（v1.x / v3.x）を自前で |

---

## 5. ESP-IDF と共存する案 vs ベアメタル

| 観点 | ESP-IDF 上（esp-idf-hal / -sys / -svc） | ベアメタル（esp-hal または自前） |
|---|---|---|
| P4 v1.x 対応 | **可**。`CONFIG_ESP32P4_SELECTS_REV_LESS_V3=y` と `CONFIG_ESP32P4_REV_MIN_100=y` が必要（esp-idf-sys #376 の根本原因分析）[E13]。ターゲット `riscv32imafc-esp-espidf`（esp-idf-hal README）[E16] | esp-hal は**不可**（v3.0+ のみ）。自前なら可 |
| P4 v3.x 対応 | 可 | esp-hal 1.2+ で可 |
| M/U 分離 | FreeRTOS と IDF ドライバ全体が M モード。U モードアプリを置くには IDF の割込み・トラップ・PMP 設定（`SOC_CPU_IDRAM_SPLIT_USING_PMP`）と競合する。ESP-TEE は逆向き（TEE 側が M）で目的が違う | 完全に自由 |
| ドライバ量 | MIPI-DSI / CSI、PPA、JPEG、USB、SDIO（esp-hosted 経由の C6 Wi-Fi）まで揃う | esp-hal v3 でも DSI は partial、CSI / PPA / JPEG は無し |
| 「カーネル更新」 | IDF OTA そのもの | IDF bootloader ＋ `esp-bootloader-esp-idf::ota` で同等 |
| リポジトリ状況 | esp-idf-hal / -sys / -svc は `esp-rs/esp-idf` モノレポに統合済み（esp-idf-sys 0.38.1, esp-idf-hal 0.47.0, esp-idf-svc 0.53.0, 2026-09）[E16][E7] | esp-hal 1.2.2 |

- 結論: OS の目標（M/U 分離）を優先すると、ベアメタル一択。**ESP-IDF は「v1.x のブートローダ供給元」と「レジスタ手順の参照元」として使う**（hw_ver1 のヘッダ、`esp_hw_support`、`esp_lcd` の DSI 手順など）。
- 折衷案（推測）: ESP-IDF の 2nd stage bootloader（rev<3 用にビルド、または espflash 同梱の rev<300 bootloader [E14]）で自前カーネルの app image を起動し、OTA パーティションでカーネル更新を実現する。esp-hal が使っている方式と同じで、app image ヘッダ（`esp_app_desc`）だけ自前で用意すればよい。

---

## 6. 未確認事項

- U1: 手元の Tab5 は v1.0 で確定（`~/repos/tab5_claude_client/logs/boot.log:15`）。残る疑問は、M5Stack が v3.x に切り替えた出荷ロットを出すか（出れば esp-hal が使える）。Espressif は v1.3 の EOL と v3.2 への移行を計画中（PCN202600801、#5880 のコメント）[E17]。v1.0 と v1.3 のレジスタ差分も未確認。
- U10: 手元の bootloader（ESP-IDF v6.0-beta1-624）が、app に渡す時点で PMP エントリ（0–2, 13）をロック済みにしているか。現行 master は PMA のみ設定するが、`pmp_layout.h` は「shipped bootloaders」がロックしている前提で書かれている。起動直後に `pmpcfg0..3` / `pmpaddr0..15` をダンプして確認する。
- U11: esp32p4 PAC 0.3 / 0.4 の SVD の元になった CSV のリビジョン（v3 由来と推測）。
- U2: probe-rs / OpenOCD による v1.x P4 の JTAG デバッグ可否。
- U3: P4（v1.x / v3.x）の HP コアが Smepmp（`mseccfg`）、PMP 16 エントリ、U モードに対応しているか。PMA との関係。TRM（ハードウェア担当の文書）で要確認。
- U4: IDF bootloader / ROM が PIE・HWLOOP・FPU をどの状態で app に渡すか（`mstatus.FS` の初期値など）。
- U5: ESP32-C3 で Tock が回避している「U モードからのトラップ時に mtvec が U モードから X 可でないとフォルトする」バグが、P4 にもあるか。
- U6: esp-hal（v3）に P4 ホストから外付け ESP32-C6 を esp-hosted / SDIO で使う経路があるか。調べた範囲では見当たらない。
- U7: NuttX の protected build（PMP）と ESP-TEE が P4 で使えるか。
- U8: esp32p4 PAC 0.2.0（2024-04）が v1.x の SVD に基づくかどうか（日付からの推測にとどまる）。
- U9: `riscv32imafc-unknown-none-elf` ＋ `relocation-model=pie` の静的 PIE を rust-lld で出したときの `.rela.dyn` の内容（実際にビルドして確認する必要あり。今回はビルド禁止のため未実施）。

---

## 参考文献

### esp-rs / Espressif
- [E1] esp-hal main @5c0820c: https://github.com/esp-rs/esp-hal/tree/5c0820cf9e2be0a49719bd71c30e7a45cae48134 — `esp-hal/README.md:36`, `esp-hal/Cargo.toml:127,260`, `esp-hal/ld/esp32p4/memory.x`, `esp-hal/src/lib.rs:762`, `esp-hal/src/soc/mod.rs:364-540`, `esp-hal/esp_config.yml:240-244`, `esp-metadata/devices/esp32p4/soc.toml`, `esp-sync/src/raw.rs:38-45`
- [E2] esp-hal #2285 "Initial P4 support tracking issue"（2024-10-07 作成、2026-06-15 クローズ）: https://github.com/esp-rs/esp-hal/issues/2285
- [E3] esp-hal #3962 "Chip support road map": https://github.com/esp-rs/esp-hal/issues/3962
- [E4] esp-hal #6179 "unable to run any examples against early-revision esp32p4"（M5Stack Tab5、v1.3、2026-08-24）: https://github.com/esp-rs/esp-hal/issues/6179 ／ 類例 #5968: https://github.com/esp-rs/esp-hal/issues/5968 ／ #5880: https://github.com/esp-rs/esp-hal/issues/5880
- [E5] esp-hal PR #5400 "Initial ESP32-P4 support"（2026-05-05 マージ、merge commit 2ccf50d）: https://github.com/esp-rs/esp-hal/pull/5400
- [E6] esp-hal CHANGELOG（v1.2.0 / v1.2.0-rc.0 節）: https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-hal/CHANGELOG.md ／ Releases: https://github.com/esp-rs/esp-hal/releases
- [E7] crates.io API（2026-10-29 取得）: esp-hal 1.2.2, esp-riscv-rt 0.15.0, esp-bootloader-esp-idf 0.6.0, riscv 0.16.1, riscv-rt 0.18.0, espflash 4.6.0, esp-idf-sys 0.38.1, esp-idf-hal 0.47.0, esp-idf-svc 0.53.0
- [E8] esp-hal #6586（open）: https://github.com/esp-rs/esp-hal/issues/6586
- [E9] esp-bootloader-esp-idf: https://github.com/esp-rs/esp-hal/tree/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-bootloader-esp-idf ／ ローカル `~/.cargo/registry/src/*/esp-bootloader-esp-idf-0.5.0`
- [E10] crates.io esp32p4 versions: https://crates.io/crates/esp32p4/versions
- [E11] esp-pacs esp32p4: https://github.com/esp-rs/esp-pacs/tree/main/esp32p4 （commit 788901b "P4: Use new base SVD" 2026-04-16）
- [E12] ESP-IDF @9c37a0a: `components/soc/project_include.cmake:11-110`, `components/soc/esp32p4/include/soc/soc_caps.h:180-215`, `components/soc/esp32p4/register/{hw_ver1,hw_ver3}`, `tools/cmake/llvm_optimizations.cmake` — https://github.com/espressif/esp-idf/tree/9c37a0a75676c3726284318925b5ae17bba3bd88
- [E13] esp-idf-sys #376 "Can't properly run on esp32p4 (Instruction access fault)": https://github.com/esp-rs/esp-idf-sys/issues/376
- [E14] espflash CHANGELOG（3.2.0 / 4.4.0 / 4.5.0 / 4.6.0 / Unreleased）: https://github.com/esp-rs/espflash/blob/main/espflash/CHANGELOG.md
- [E15] ESP-TEE（ESP-IDF Programming Guide, esp32c6）: https://docs.espressif.com/projects/esp-idf/en/latest/esp32c6/security/tee/tee.html
- [E16] esp-rs/esp-idf モノレポ（旧 esp-idf-hal / -sys / -svc）: https://github.com/esp-rs/esp-idf （`esp-idf-hal/README.md` のターゲット表）
- [E17] Espressif PCN（#5880 のコメントで引用）: https://documentation.espressif.com/en/PCN20250501_Upgrade_of_ESP32-P4_Series_Products_to_Chip_Revision_v1.3.html , https://documentation.espressif.com/en/PCN202600801_ESP32-P4_Chip_Revision_v3.2_Upgrade_Chip_Revision_v1.3_Demand_Collection_and_EOL_Plan_Description.html
- [E18] espressif/svd `svd/esp32p4.svd` のコミット履歴（2023-11-27 初版、2024-01-24 最終）: https://github.com/espressif/svd/commits/main/svd/esp32p4.svd ／ esp-pacs PR #425: https://github.com/esp-rs/esp-pacs/pull/425
- [E19] ESP-IDF @9c37a0a の P4 PMP 設定: `components/esp_hw_support/port/esp32p4/private_include/pmp_layout.h`, `components/esp_hw_support/port/esp32p4/cpu_region_protect.c:20-72,255-330,449-488`, `components/bootloader_support/src/bootloader_mem.c:60-64` — https://github.com/espressif/esp-idf/blob/9c37a0a75676c3726284318925b5ae17bba3bd88/components/esp_hw_support/port/esp32p4/cpu_region_protect.c
- [E20] NuttX "M5Stack Tab5" ボードページ: https://nuttx.apache.org/docs/latest/platforms/risc-v/esp32p4/boards/esp32p4-tab5/index.html
- [E21] 手元の Tab5 のブートログ: `~/repos/tab5_claude_client/logs/boot.log:2,11,15`（ROM eco2-20240710、ESP-IDF v6.0-beta1-624 bootloader、chip revision v1.0、OTA パーティション構成）

### Rust 組込み
- ローカル: `~/.cargo/registry/src/*/riscv-0.15.0/src/register/{mstatus.rs:408,pmpcfgx.rs,pmpaddrx.rs}`, `riscv-0.15.0/src/asm.rs:61`, `riscv-rt-0.16.0/{Cargo.toml,src/lib.rs,src/exceptions.rs}`, `riscv-rt-macros-0.6.1/src/lib.rs:590-640`, `esp-riscv-rt-0.14.0/{Cargo.toml,src/lib.rs}`
- rust-embedded/riscv: https://github.com/rust-embedded/riscv
- rustc 1.99.1 の `--print target-spec-json / target-features / relocation-models`（ローカル実行、2026-10-29）

### 既存 OS
- [T0] Tock releases: https://github.com/tock/tock/releases （2.3.0, 2026-10-08）
- [T1] Tock `arch/riscv/src/pmp.rs`（@d4979a4）: https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/arch/riscv/src/pmp.rs ／ `kernel/src/platform/mpu.rs`
- [T2] Tock `arch/riscv/src/syscall.rs:359-800`: https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/arch/riscv/src/syscall.rs
- [T3] Tock TRD104 Syscalls: https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md
- [T4] Tock `chips/esp32-c3/src/chip.rs:34-53,131-165`: https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/chips/esp32-c3/src/chip.rs
- [T5] Tock `boards/esp32-c3-devkitM-1/{README.md,layout.ld,Makefile,.cargo/config.toml}`: https://github.com/tock/tock/tree/d4979a4c09b6253fc0c44ddb202096ddcb15f445/boards/esp32-c3-devkitM-1
- [T6] libtock-c `Configuration.mk:60-90`, `AppMakefile.mk:380-392`（@8dad482）: https://github.com/tock/libtock-c/blob/8dad482b65b37ce5eb2c79f9fd6894bca771de71/Configuration.mk
- [T7] libtock-rs README:10-15（@b7f1759）と issue #28 "Fix relocation": https://github.com/tock/libtock-rs/issues/28
- [T8] Tock `doc/reference/trd-dynamic-process-loading.md`（Draft 2025-03-05）、`kernel/src/dynamic_binary_storage.rs:178`
- [H1] Hubris（@c611e20）`doc/syscalls.adoc:36-76`, `sys/kern/src/arch/arm_m.rs`: https://github.com/oxidecomputer/hubris/blob/c611e20caa2dac7a999668021468d641380648b8/doc/syscalls.adoc
- [R1] rCore-Tutorial-v3 `os/src/syscall/mod.rs`, `user/src/syscall.rs:39-44`: https://github.com/rcore-os/rCore-Tutorial-v3
- [X1] Xous（@e8d989c, 2026-10-26）: https://github.com/betrusted-io/xous-core/tree/e8d989c754bcc3a9cd66f9e3e1c70cac580731eb — `xous-rs/src/syscall.rs:17,581-632`, `xous-rs/src/arch/riscv/syscall.rs:18-31`, `xous-rs/src/definitions/messages/mod.rs:111-117`, `docs/syscalls.md:11-39`, `docs/memory.md:38-65`, `docs/arguments.md:33-90`, `kernel/src/arch/riscv/asm.S:29-270`, `kernel/src/arch/riscv/irq.rs:246-253`, `kernel/src/arch/riscv/mem.rs:134-137,848-925`, `kernel/src/mem.rs:820-875`, `loader/src/asm.rs:270-296`, `loader/src/main.rs:153-157,269-451,884-1042`, `api/xous-api-names/src/lib.rs:62,125`, `xous-rs/src/arch/hosted/mod.rs`, `xtask/src/main.rs:417-459`, `README-baochip.md:82,111`
- [O1] Embassy: https://github.com/embassy-rs/embassy ／ [O2] RTIC: https://rtic.rs/
- xv6-riscv: https://github.com/mit-pdos/xv6-riscv ／ Theseus: https://github.com/theseus-os/Theseus ／ Redox: https://gitlab.redox-os.org/redox-os/redox ／ Hermit: https://github.com/hermit-os/kernel
