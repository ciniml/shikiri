# M4 調査: CPU 360MHz 化と PSRAM の初期化（ESP32-P4 v1.0）

対象は ESP32-P4 rev v1.0（Tab5、`CONFIG_ESP32P4_SELECTS_REV_LESS_V3`）。カーネルは ESP-IDF 6.0 の 2nd stage bootloader から M モードで起動し、フラッシュから XIP で動いている。M4a では、CPU を 360MHz に上げ、32MB の PSRAM（AP Memory、HEX/X16、1.8V、200MHz）を初期化して、アプリを PSRAM にコピーして動かす。
出典の表記: パスは `~/esp-idf/6.0/components/` からの相対パス。`ESPHAL/` は `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/esp-hal-1.2.2/`、`TCC/` は `~/repos/tab5_claude_client/`（doc/06 と同じ）。（推測）は推測、（判断）は判断を表す。PSRAM MMU のエントリ形式と窓（0x4800_0000、レジスタ 0x5008_E380/E37C）は doc/02 §3 にあるので、ここでは繰り返さない。

## 0. 結論
- **クロック**: bootloader は CPLL を 360MHz に較正したうえで、CPU を 360/4 = **90MHz**（MEM、SYS、APB もすべて 90MHz）にしている。360MHz にするには **分周器を 4 つ書き換えるだけ** でよい（CPU 1、MEM 2、SYS 1、APB 2。APB は 90MHz のまま変わらない）。PLL と電圧には触らなくてよい（§1）。SYSTIMER（16MHz）、USJ、フラッシュのクロックは影響を受けない。
- **PSRAM**: bootloader は PSRAM を初期化しない（app の仕事）。手順は、LDO VO2 を 1.8V にする → MPLL を 400MHz にする → MSPI2/3 のクロック、リセット、パッドを設定する → モードレジスタを設定する → AXI（キャッシュ側）のコマンドを設定する → タイミングチューニング（DQS 位相 4 通り × 遅延線 31 通り）→ MMU に線形マップ、の順（§2）。Rust で 400〜500 行程度（推測）。ROM にあるのは SPI コマンドの発行（`esp_rom_spi_cmd_*`）とキャッシュ操作だけ。regi2c（MPLL の設定）は ROM になく、自前で書く必要がある。
- **チューニング**: ESP-IDF は 200MHz で必ずチューニングする（`MSPI_TIMING_PSRAM_NEEDS_TUNING=1`）。所要時間は実機で約 170ms。固定値で済ませられるかは未確認。最初はチューニングを移植し、結果をログに出す（判断）。
- **Rust**: esp-hal 1.2.2 に P4 の PSRAM ドライバがあるが、**v3.0 以上が対象**で、タイミングチューニングもない。PAC `esp32p4` 0.3.0 も手元にある。どちらもロジックの参考にとどめる（§4）。

## 1. クロック

### 1.1 bootloader が残す状態
| 項目 | 値 | 根拠 |
|---|---|---|
| bootloader の CPU 周波数 | v1 では 90MHz（v3 では 100MHz） | bootloader/Kconfig.projbuild:42-47 |
| 設定する関数 | `bootloader_init()` → `bootloader_clock_configure()` → `rtc_clk_init()` | bootloader_support/src/esp32p4/bootloader_esp32p4.c:155-156; bootloader_support/src/bootloader_clock_init.c:37,51-80 |
| 90MHz の作り方 | CPLL 360MHz を CPU 分周 4。MEM 1、SYS 1、APB 1 なので、CPU = MEM = SYS = APB = 90MHz | esp_hw_support/port/esp32p4/rtc_clk.c:231-253,338-342 |
| CPLL | ROM 起動時は XTAL で動いており、CPLL は bootloader の `rtc_clk_cpu_freq_set_config` で有効化され、**360MHz に較正される**（bootloader では `s_cur_cpll_freq` が 0 から始まるため） | rtc_clk.c:30,401-421,154-172 |
| 電圧 | `rtc_clk_init` が eFuse 由来の HP/LP dbias を設定し、DCDC に切り替え、`dcm_vset` を 27（約 1.25V）以上にする | esp_hw_support/port/esp32p4/rtc_clk_init.c:56-73; port/esp32p4/private_include/pmu_param.h:20-21 |
| MSPI（フラッシュ） | クロック源は SPLL（480MHz）、コアクロック 80MHz。CPU クロックとは独立 | bootloader_support/bootloader_flash/src/bootloader_flash_config_esp32p4.c:45-49; bootloader_esp32p4.c:113-116 |
| 解析用 I2C マスタ | bootloader がクロックを有効にしたまま残す | bootloader_esp32p4.c:98-99 |

- v1.0 は `ESP_CHIP_REV_ABOVE(rev, 1)` が真になるので、ECO0 向けの PLL 引き下げ処理（:102-107）は通らず、`bootloader_init_mspi_clock()` が実行される（推測。v1.0 = 100 として計算）。
- doc/14 で、実機が t5emu の 40MHz 換算より約 1.9 倍速かった。CPU 90MHz で XIP がボトルネックになっている、と考えれば説明がつく（推測）。

### 1.2 ESP-IDF の app が 360MHz にする手順
- 起動順は `sys_rtc_init`（`esp_rtc_init` → `pmu_init`、PVT を含む）→ `mspi_init`（PSRAM の初期化とチューニング）→ … → `esp_clk_init()` の順 [esp_system/port/cpu_start.c:547-571,591-636,781]。**PSRAM は CPU 90MHz のまま初期化され**、そのあとで 360MHz に上げている。
- `esp_clk_init` → `rtc_clk_cpu_freq_mhz_to_config(360)` → `rtc_clk_cpu_freq_set_config` [esp_system/port/soc/esp32p4/clk.c:135-154]。app 側の `s_cur_cpll_freq` は 0 なので、いったん XTAL に切り替えて CPLL を 360MHz に再較正してから戻る [rtc_clk.c:414-419]。bootloader がすでに 360MHz にしているので、カーネルでは **再較正は不要**（推測。§1.3 の確認手順で検証する）。
- 分周の変更 `rtc_clk_cpu_freq_to_cpll_mhz(360)` [rtc_clk.c:223-317]
  - 制約: MEM_CLK ≤ 200MHz、APB_CLK ≤ 100MHz。360MHz のときは CPU/1、MEM/2 = 180、SYS/1 = 180、APB/2 = 90 [rtc_clk.c:225-245]。
  - 上げるときは **APB → SYS → MEM → CPU の順** に、1 つ書くたびに `clk_ll_bus_update()` を呼ぶ。最後にソースを CPLL にする（今も CPLL なので実質何もしない）[rtc_clk.c:284-316]。
  - 最後に ROM の tick を 360 に更新する（`esp_rom_set_cpu_ticks_per_us`）[rtc_clk.c:316]。`ets_delay_us` を使うなら必要で、ROM の `ets_update_cpu_frequency` = 0x4FC0_0044 [esp_rom/esp32p4/ld/esp32p4.rom.ld:32]。
- 電圧: app の `pmu_init` の HP active 設定も `dcm_vset = 27` で、bootloader と同じ [port/esp32p4/pmu_param.c:145-158]。違いは PVT（電圧の自動調整）だけ。PVT は「量産では必須」とされ [esp_hw_support/Kconfig:288-295]、eFuse ブロック版 ≥ 2 のときに働く [port/esp32p4/pmu_pvt.c:77-80]。Tab5 は eFuse ブロック v0.3 なので対象になる [TCC/logs/boot.log:16]。**PVT なしの 360MHz は bootloader の電圧で動く見込みだが、温度などのマージンは保証されない**（推測）。M4a では PVT は入れず、余裕ができたら移植する（判断）。

### 1.3 レジスタ手順（最小）
`HP_SYS_CLKRST` = 0x500E_6000、`LP_CLKRST` = 0x5011_1000 [soc/esp32p4/register/hw_ver1/soc/reg_base.h:10-11,120,130]

| レジスタ | アドレス | フィールド | 根拠 |
|---|---|---|---|
| ROOT_CLK_CTRL0 | 0x500E_6004 | bit4 `SOC_CLK_DIV_UPDATE`（1 を書き、0 に戻るまで待つ）、bit[12:5] `CPU_CLK_DIV_NUM`（= 分周 − 1）、bit[20:13] 分子、bit[28:21] 分母 | hp_sys_clkrst_reg.h:29,43,49-64; hal/esp32p4/include/hal/clk_tree_ll.h:527-531,584-592 |
| ROOT_CLK_CTRL1 | 0x500E_6008 | bit[7:0] `MEM_CLK_DIV_NUM`、bit[31:24] `SYS_CLK_DIV_NUM` | hp_sys_clkrst_reg.h:69-97; clk_tree_ll.h:618-647 |
| ROOT_CLK_CTRL2 | 0x500E_600C | bit[23:16] `APB_CLK_DIV_NUM` | hp_sys_clkrst_reg.h:102-123; clk_tree_ll.h:671-675 |
| LP_CLKRST_HP_CLK_CTRL | 0x5011_1040 | bit[1:0] `HP_ROOT_CLK_SRC_SEL`（0 = XTAL、1 = CPLL、2 = RC_FAST） | lp_clkrst_reg.h:662-669; clk_tree_ll.h:538-556 |

```
// 前提: 割り込み禁止、RAM（L2MEM）上で実行する（判断。MEM_CLK が変わる間 L1/L2 を通る XIP を避ける）
assert(HP_ROOT_CLK_SRC_SEL == 1 && CPU_DIV_NUM == 3 && MEM/SYS/APB_DIV_NUM == 0)   // 90MHz の状態を確認する
APB_DIV_NUM = 1; update()     // APB 45MHz（途中の状態）
SYS_DIV_NUM = 0; update()
MEM_DIV_NUM = 1; update()     // MEM 45MHz（途中の状態）
CPU_DIV_NUM = 0; update()     // CPU 360 / MEM 180 / SYS 180 / APB 90
ets_update_cpu_frequency(360) // ROM の delay を使う場合だけ
```
- 確認: 一定時間の SYSTIMER の刻み数と `mcycle` の増分の比をとり、4.0 倍（90 → 360）になったか見る。P4 には `SOC_CPU_HAS_CSR_PC` がないので、サイクルカウンタは標準の `cycle` CSR [riscv/include/riscv/rv_utils.h:105-108]。
- ウォームリセット（SW リセット）では、bootloader は APB ≥ 90MHz ならクロック設定を飛ばす [bootloader_clock_init.c:51; soc/esp32p4/include/soc/soc.h:137]。そのため 360MHz のまま再起動してくることがある。**手順は今の分周値を読んでから決め、何度実行しても同じ結果になるようにする**（判断）。
- **影響を受けないもの**: SYSTIMER は XTAL 40MHz の 2.5 分周で 16MHz 固定 [port/esp32p4/systimer.c:10-11; hal/esp32p4/include/hal/systimer_ll.h:29-33]。USJ は APB クロック（90MHz のまま）と 48MHz の PHY クロックで動き、CPU クロックは関係しない [hal/esp32p4/include/hal/usb_serial_jtag_ll.h:336-341]。フラッシュは SPLL 系（§1.1）。

## 2. PSRAM の初期化

### 2.1 担当は app（bootloader はやらない）
- bootloader は PSRAM 上のセグメントを「app でマップする」として扱い、自分ではロードしない [bootloader_support/src/bootloader_utility.c:905-907]。PSRAM MMU のエントリはすべて無効にしてから渡す [hal/mmu_hal.c:40-50; bootloader_utility.c:1063]。
- app 側では `mspi_init()` → `esp_psram_chip_init()` → `esp_psram_impl_enable()`、続いて `esp_psram_init()` → `s_psram_mapping()` [cpu_start.c:591-636; esp_psram/system_layer/esp_psram.c:186-205,396-433]。実機ログでも bootloader の後に `hex_psram` と「Enter psram timing tuning」が出て、チューニングに約 170ms かかっている [TCC/logs/boot.log:39-56]。

### 2.2 電源（LDO VO2 = 1.8V）
- ESP-IDF は `periph_rtc_mpll_acquire()` の中で、**LDO チャネル 2 を 1.8V にして有効にする**。このチャネルは「PSRAM と MPLL の電源」[esp_hw_support/clk_ctrl_os.c:143-161; port/esp32p4/Kconfig.ldo:36-65]。Tab5 の IDF 設定も同じ（`CONFIG_ESP_LDO_CHAN_PSRAM_DOMAIN=2`、1800mV）[TCC/sdkconfig:2194-2200]。
- チャネル 2 は unit 1 で、PMU の `ext_ldo[3]` = `PMU_EXT_LDO_P1_0P1A`（0x5011_51D0）と `_ANA`（0x5011_51D4）[hal/esp32p4/include/hal/ldo_ll.h:155-216; pmu_reg.h:4339-4440; reg_base.h:135]。
  - 書く内容: `ANA.dref`（bit[31:28]）と `ANA.mul`（bit[25:23]）を設定する。1.8V のときは eFuse の `ldo_vo2_dref` と `ldo_vo2_mul` があればそれを使う。`tieh`（bit14）= 0、`force_tieh_sel`（bit7）= 1（ソフトウェア制御）、`xpd`（bit8）= 1 [ldo_ll.h:70-143,186-216; esp_hw_support/ldo/esp_ldo_regulator.c:86-98]。
  - 計算式: Vref = dref < 9 ? 0.5 + 0.05·dref : 1 + 0.1·(dref − 9)、Vout ≈ Vref × (1 + 0.25·mul) [ldo_ll.h:58-61]。esp-hal は VO2 に `0x5700_0000`（dref 5、mul 6 → 0.75 × 2.5 ≈ 1.875V）を書いている [ESPHAL/src/psram/esp32p4.rs:216-220]。
- Tab5 で VO2 が実際に PSRAM の VDD につながっているかは回路図で確かめていない（推測。IDF の既定と TCC の設定から、そう判断している）。**まず実機で P1_0P1A を読み、bootloader の時点で xpd が立っているかを見る**。
- **注意**: esp-hal は VO1（`P0_0P1A`）にも `0xB100_0000`（約 1.8V）を書いている [ESPHAL/src/psram/esp32p4.rs:193-199]。IDF では VO1 はフラッシュ用の 3.3V [Kconfig.ldo:4-34]。**VO1 から VO4 をまとめて書く esp-hal の処理は写さない**（判断）。

### 2.3 クロック（MPLL 400MHz → MSPI 200MHz）
1. MPLL の電源を入れる: `PMU_RF_PWC_REG`（0x5011_515C）の bit24 `MSPI_PHY_XPD` と、`LP_CLKRST_HP_CLK_CTRL` の bit28 `HP_MPLL_500M_CLK_EN` を立てる [clk_tree_ll.h:155-159; pmu_reg.h:2563-2570; lp_clkrst_reg.h:858]。
2. 較正: `HP_SYS_CLKRST_ANA_PLL_CTRL0`（0x500E_60BC）の bit9 `MSPI_CAL_STOP` を下ろす → regi2c で MPLL（block 0x63）の DHREF と IR_CAL_RSTB を操作し、`DIV_REG`（addr 2）に `(div<<3)|ref_div` を書く（400MHz なら div = 400/20 − 1 = 19、ref_div = 1）→ bit8 `MSPI_CAL_END` を待つ → `CAL_STOP` を立てる [port/esp32p4/rtc_clk.c:656-678; clk_tree_ll.h:450-468; hal/esp32p4/include/hal/regi2c_ctrl_ll.h:97-118; soc/esp32p4/include/soc/regi2c_mpll.h:17-33]。
   - **regi2c は ROM になく**、IDF は `esp_rom/patches/esp_rom_regi2c_esp32p4.c`（193 行。実質は :88-171 の約 80 行）で実装している。esp-hal にも `src/soc/esp32p4/regi2c.rs`（184 行）がある。
3. MSPI 側 [esp_psram/device/esp_psram_impl_ap_hex.c:419-439; esp_hal_mspi/esp32p4/include/hal/psram_ctrlr_ll.h:339-494]
   - `SOC_CLK_CTRL0`（0x500E_6014）の bit31 `PSRAM_SYS_CLK_EN` と、`PERI_CLK_CTRL00`（0x500E_6030）の bit14 `PSRAM_PLL_CLK_EN` を立てる。
   - `HP_RST_EN0`（0x500E_60C0）で bit23 `DUAL_MSPI_AXI` と bit25 `DUAL_MSPI_APB` をパルスにする。**bit22 と bit24 はフラッシュ側の MSPI なので触らない** [hp_sys_clkrst_reg.h:3225-3246]。
   - `PERI_CLK_CTRL00` の bit[13:12] `PSRAM_CLK_SRC_SEL` = 1（MPLL）。コアクロックは、既定値で EN = 1、DIV = 0（= 400MHz）[hp_sys_clkrst_reg.h:1085-1113]。
   - バス分周 2（200MHz）: `SPI_MEM_S_SRAM_CLK`（0x5008_E050）と `SPI1_MEM_S_CLOCK`（0x5008_F014）に N = 1、H = 0、L = 1 を書く [psram_ctrlr_ll.h:458-474; spi_mem_s_reg.h:924; spi1_mem_s_reg.h:354]。DLL を有効にする（`SMEM_TIMING_CALI` 0x5008_E190 と `TIMING_CALI` 0x5008_E180）[psram_ctrlr_ll.h:484-494]。
- MPLL を使わない案: PSRAM のクロック源には XTAL / MPLL / SPLL / CPLL を選べる [psram_ctrlr_ll.h:382-411]。CPLL 360MHz の 2 分周（180MHz）なら regi2c なしで動くかもしれない。ただし IDF に前例がなく、MPLL は VO2 系の電源でもあるので、採らない（推測／判断）。

### 2.4 チップの初期化（ap_hex、200MHz の設定値）
| 定数 | 値 | 根拠 |
|---|---|---|
| コマンド | SYNC_READ 0x0000、SYNC_WRITE 0x8080、REG_READ 0x4040、REG_WRITE 0xC0C0（16bit）、アドレス 32bit | esp_psram_impl_ap_hex.c:21-29 |
| ダミー（200MHz） | データ読み出し 26bit（2×(14−1)）、レジスタ読み出し 12bit、書き込み 12bit | :37-42 |
| MR の設定値 | MR0: LT = 1（固定レイテンシ）、RL = 4、drive = 0。MR4: WL = 1。MR8: BL = 3（2KB）、BT = 0、RBX = 1、X16 = 1 | :441-449 |
| CS | setup 4、hold 4、hold_delay 3、split_trans、page 2048 | :53-56,365-373,429-431 |

手順 [esp_psram_impl_ap_hex.c:410-478]
1. パッド: PSRAM の全ピン（D、Q、WP、HOLD、DQ4〜DQ15、DQS0/1、CK、CS）の DRV を 2 にし、DQS0/1 の XPD を立てる。`IOMUX_MSPI_PIN` = 0x500E_1200（PSRAM_D_PIN0 が +0x1C、DQS_0 が +0x3C、DQS_1 が +0x68）[esp_hal_mspi/esp32p4/include/hal/mspi_ll.h:231-270; iomux_mspi_pin_reg.h:270-319,702-772,1317]。
2. CS のタイミング、split、ページサイズを MSPI2（0x5008_E000、`SMEM_AC` +0x1A0 ほか）に設定する。
3. **MSPI3（0x5008_F000）の直接コマンドでモードレジスタを設定する**: MR0/1 と MR4/8 を読む → 書き換えて書き戻す → MR8 を読む → 書き換えて書き戻す [:147-225]。
4. 接続を確認する: アドレス 0 に 0x5A6B_7C8D を SYNC_WRITE で書き、SYNC_READ で読み戻して比べる [:272-302]。
5. MR0〜MR8 を読み、ベンダ（MR1 = 0x0D が AP）と容量（MR2.density 0x7 = 32MB）を確認する [:227-267,460-470]。実機の値: vendor 0x0d、density 0x07、KGD Pass [TCC/logs/boot.log:39-50]。
6. AXI（キャッシュ側）の設定: MSPI2 に、読み書きのコマンド、32bit の 4 バイトアドレス、ダミー、可変ダミー、DDR、OCT と HEX のデータ線、AXI と splice を設定する [:329-363]。
7. タイミングチューニング（§2.5）。最後に MSPI2 と MSPI3 の可変ダミーを有効にする [:473-475]。

- 直接コマンドは `psram_ctrlr_ll_common_transaction` → ROM の `esp_rom_spi_set_op_mode(3, OPI_DTR=7)`、`esp_rom_spi_cmd_config(3, &cmd)`、`esp_rom_spi_cmd_start(3, rx, len, cs_mask=1<<1, false)` [psram_ctrlr_ll.h:768-817; esp_rom/esp32p4/include/esp32p4/rom/opi_flash.h:53-63,163-186; esp_rom/include/esp_rom_spiflash.h:18-27]。v1 の ROM アドレスは 0x4FC0_0110 / 0x4FC0_0108 / 0x4FC0_010C で、eco5 と同じ [esp32p4.rom.ld:99-101]。FIFO は 64 バイトまで [psram_ctrlr_ll.h:41]。
- IDF はこの部分を IRAM に置いている（`esp_psram_impl_ap_hex (noflash)`、チューニングも noflash）[esp_psram/linker.lf:16-17; esp_hw_support/mspi_timing_tuning/linker.lf:6-15]。PSRAM とフラッシュは別の MSPI なので、XIP のままでも動くかもしれない（推測）。それでも **L2MEM に置いて実行する**（判断。L2 キャッシュを共有しているため）。

### 2.5 タイミングチューニング
- 200MHz では必須とされている（`MSPI_TIMING_PSRAM_NEEDS_TUNING 1`、MPLL 400）[esp_hw_support/mspi_timing_tuning/port/esp32p4/mspi_timing_tuning_configs.h:18-23]。P4 は DQS 方式 [mspi_timing_tuning/mspi_timing_tuning.c:437-468]。
- 流れ [mspi_timing_tuning.c:400-471]
  1. 低速モード: PSRAM を 20MHz（分周 20）にし、DQS 位相と遅延線を 0 にする [:484-501; mspi_timing_by_dqs.c:199-207]。P4 はフラッシュのクロック源を切り替えない（`MSPI_TIMING_LL_FLASH_CLK_SRC_CHANGEABLE` は H2/H21 だけ）。フラッシュ 80MHz ではフラッシュのチューニングも空になる [mspi_timing_tuning_configs.h:44; port/esp32p4/mspi_timing_config.c:38-60]。
  2. 128 バイトの参照データを、MSPI3 で PSRAM の 0x80 に書く [mspi_timing_by_dqs.c:49-52,74-91]。
  3. 200MHz に戻し（`mspi_timing_psram_init`）、**DQS 位相 4 通り**（67.5°、78.75°、90°、101.25°）を 1 回ずつ読んで比べる → 連続して成功した範囲の右端寄りを選ぶ [mspi_timing_by_dqs.c:61-65,112-153; mspi_timing_tuning.c:217-250,317-343]。
  4. その位相で、**遅延線 31 通り**（DQS 側 15→0、データ側 0→15）をそれぞれ 100 回読む → 連続成功区間の中央を選ぶ [mspi_timing_by_dqs.c:53-57,161-197; mspi_timing_tuning_configs.h:14]。
  5. 高速モード: 200MHz にして、選んだ位相と遅延線を書く [mspi_timing_tuning.c:520-537; mspi_timing_by_dqs.c:209-221]。
- レジスタ: 位相は `PSRAM_DQS_0/1_PIN0` の bit[2:1]、遅延は DQS の `DELAY_90`（bit[10:7]）と `DELAY_270`（bit[20:17]）、データピンは各 `PINx` の `DLC`（bit[7:4]）[mspi_ll.h:166-224; iomux_mspi_pin_reg.h:284,716-772]。
- 規模: 約 3,100 回の 128 バイト読み出し。実機で約 170ms [TCC/logs/boot.log:56-57]。ロジックは 150 行程度（推測）。
- **固定値で済ませる案**: esp-hal（v3 用）はチューニングをせず、既定値（位相 0、遅延 0）で動かしている [ESPHAL/src/psram/esp32p4.rs:143-185; ESPHAL/src/psram/oct_hex.rs 全体に tuning なし]。IDF には温度でサンプル点をずらすオプションまであり、マージンはそれほど広くない、と読める（`CONFIG_SPIRAM_TIMING_TUNING_POINT_VIA_TEMPERATURE_SENSOR`）[mspi_timing_tuning.c:307]（推測）。**M4a はチューニングを移植し、選ばれた位相と遅延線、成功区間の幅をイベントログに出す**（判断）。区間が常に広いなら、あとで固定値とスキップを検討する。

### 2.6 ROM にあるもの・自前で書くもの
| 処理 | ROM | 自前 |
|---|---|---|
| SPI 直接コマンド | `esp_rom_spi_set_op_mode` / `_cmd_config` / `_cmd_start`（0x4FC0_0110 / 0108 / 010C） | 呼び出しの包み、64 バイトずつの分割 |
| キャッシュ | `Cache_WriteBack_Addr` 0x4FC0_03F4、`Cache_Invalidate_Addr` 0x4FC0_03E4、`Cache_WriteBack_All` 0x4FC0_0414 [esp32p4.rom.ld:186-198] | – |
| 遅延 | `ets_delay_us` 0x4FC0_003C [esp32p4.rom.ld:30] | – |
| regi2c（MPLL） | なし（IDF も patches） | 約 80 行 |
| LDO、クロック、リセット、パッド、MSPI2 の設定、MR、チューニング、MMU | なし | 300〜400 行 |
- `Cache_PSRAM_MMU_Set`（0x4FC0_0520）もあるが、フラッシュのとき（m3-flash-access §1.4）と同じく、IDF は使っていない。レジスタ 2 本を書けば済む（判断）。

### 2.7 リビジョン差・errata
- `esp_psram_p4_rev3_workaround`（ダミー読み出し 2 回とコントローラのリセット）は **v3.0 だけ**（`CONFIG_ESP32P4_REV_MIN_FULL == 300`）。v1.0 では不要 [esp_psram.c:157-184,407-409]。
- 250MHz は v3 だけ。v1 は 200MHz まで [esp_psram/esp32p4/Kconfig.spiram:29-38]。
- IDF の esp_psram、esp_mm、cache_ll、mmu_ll、mspi_ll には、v1 用の分岐も errata の記述もなかった（grep した結果）。errata の PDF は IDF のツリーに入っていない。既知のものは doc/02 §5 の APM-560（不正な AHB アクセスで PSRAM とフラッシュが止まる）。U モードのアプリを PSRAM で動かす M4a で、そのまま関係する。

## 3. マップとキャッシュ
- MMU id 1 にエントリを書く: index = (vaddr − 0x4800_0000) >> 16、値は `page | BIT10(ACCESS_PSRAM) | BIT11(VALID)` [hal/esp32p4/include/hal/mmu_ll.h:265-300; soc/esp32p4/include/soc/ext_mem_defs.h:59-66,78,96]。32MB は 512 エントリ。P4 では `cache_ll_l1_enable_bus` の中身が空なので、バスを有効にする処理は要らない [esp_psram.c:297-301; cache_ll.h:1020-1024]。
- **起動時の線形マップ**では、IDF は L2 を止めず、invalidate もせずに書いている（`s_psram_mapping` → `mmu_hal_map_region` だけ）[esp_psram.c:263-302]。まだ一度もアクセスしていない窓なので、キャッシュに古い行がないためと考えられる（推測）。doc/07 の方針（起動時に一度だけ線形マップし、張り替えない）なら、**L2 を止める処理は要らない**（判断）。念のため、マップしたあとに窓全体を `Cache_Invalidate_Addr(L1I|L1D|L2)` する。
- 実行中に張り替える場合は、フラッシュと同じく `esp_mmu_map` の手順（L2 を suspend、書き込み、invalidate、resume）[m3-flash-access §1.1]。PSRAM を使う構成では、**L2 を suspend する前に L1 D の内部メモリ分を writeback する**（L1 の自動 writeback で止まるのを防ぐため）[esp_mm/esp_cache_utils.c:27-37]。M3 の `map_flash_linear` にも、PSRAM を有効にしたらこの writeback を足す（判断）。
- **コードを PSRAM にコピーしたあと**: IDF の PSRAM XIP は `memcpy` → `cache_hal_writeback_addr`（L1D と L2 を PSRAM へ）→ `cache_hal_invalidate_addr` の順 [esp_psram/xip_impl/mmu_psram_flash_v2.c:82-96]。PSRAM の窓は level 2 と判定される。writeback は `Cache_WriteBack_Addr(L1_DCACHE=0x10)` のあと `(L2_CACHE=0x20)`、invalidate は L1I（0x03）、L1D、L2 [cache_ll.h:644-693,536-557,1072-1093]。
  - L1 I のミスは L2 に取りにいくので、理屈の上では「L1D の writeback と L1I の invalidate」で足りる（推測）。M4a では IDF と同じ全段の手順にする（判断）。`fence.i` だけで足りるかは、doc/02 の未確認事項のまま。
- 非キャッシュのエイリアス 0x8800_0000 から PSRAM を直接読める（doc/02 §2。v3 の workaround も 0x8800_0000 を読んでいる [esp_psram.c:176-177]）。**メモリテストでキャッシュを通した書き込みを、非キャッシュ側から読み比べる** と、writeback の確認にもなる（判断）。

## 4. Rust のエコシステム（手元の crate。ネットワークは確認していない）
- `esp-hal 1.2.2` には P4 の PSRAM（`src/psram/esp32p4.rs` 322 行 + `oct_hex.rs` 460 行）とクロック（`src/soc/esp32p4/clocks.rs` 677 行）がある。ただし README には「v3.0 未満は非対応」とあり [ESPHAL/README.md:35]、クロックは「v3.x / eco5 用、360MHz の preset は TODO」[ESPHAL/src/soc/esp32p4/clocks.rs:1-8,70]。PSRAM は既定が 250MHz で、チューニングはない。1.1.2 には P4 の PSRAM がない。
- PAC `esp32p4 0.3.0` が手元にある（`hp_sys_clkrst`、`memspi2/3`、`iomux_mspi_pin`、`pmu`、`pvt` などを含む）。esp-hal が eco5 用なので、PAC も v3 の SVD から作られていると見られる（推測）。doc/06 の方針どおり、**レジスタの定義は IDF の `hw_ver1` ヘッダを一次資料にし**、esp-hal は手順の参考（MR の読み書き、regi2c）にとどめる（判断）。

## 5. M4a の進め方（案）
すべて RAM とレジスタの変更だけなので、文鎮化はしない。固まったときは、カーネルがすでに扱っている RWDT（LP_WDT）でリセットする。各ステップの間は RWDT を短めに張り、成功したら餌やりに戻す（判断）。

| 段階 | 内容 | 実機で確認すること |
|---|---|---|
| 0 | 読むだけ: ROOT_CLK_CTRL0/1/2、HP_CLK_CTRL、PMU_EXT_LDO_P1_0P1A/ANA、PMU_RF_PWC、PERI_CLK_CTRL00、SOC_CLK_CTRL0 をイベントログに出す | CPU 分周 4 / CPLL、VO2 の xpd、MPLL の状態（推測を確かめる） |
| 1 | 360MHz 化（§1.3）。L2MEM の関数で実行する | `mcycle` と SYSTIMER の比が 22.5 cycle/tick になる。USJ（RTT）の出力が続く。M3 のベンチ（88.7ms）を測り直す |
| 2 | VO2 の設定 → MPLL 400 → MSPI2/3 のクロック、リセット、パッド → MR の初期化 → ID を読む（§2.2〜2.4 の 1〜5） | MR1 = 0x0D、MR2.density = 7、KGD = 6。0x5A6B7C8D を読み戻せる |
| 3 | AXI の設定 → チューニング → 32MB を 0x4800_0000 に線形マップ → メモリテスト（アドレス書き込み、乱数、0x8800_0000 との比較） | 位相、遅延線、成功区間の幅をログに出す。全域でエラー 0。キャッシュ経由と非キャッシュの帯域を測る |
| 4 | APP_POOL を PSRAM に移す。コピー → writeback → invalidate → PMP で U に開ける | 既存のアプリ（M3 のテスト一式）が PSRAM 上で動く。命令の一貫性（fence.i だけで足りるか）を試す |

**リスクと未確認事項**
- VO2 が PSRAM の電源か（回路図で未確認）。**VO1 には触らない**（フラッシュが落ちる）。
- PVT なしで 360MHz を常用してよいか（電圧マージン）。
- チューニングの区間が狭い場合、温度で化ける（IDF に温度補正のオプションがある）。メモリテストを常設し、起動のたびに短いテストを走らせる案もある（判断）。
- MPLL と regi2c の自前実装で、較正完了（`MSPI_CAL_END`）を待つループが終わらない場合に備え、タイムアウトを付ける。
- APM-560: U モードのアプリが PSRAM の外を叩いたとき、PSRAM とフラッシュの両方が止まる可能性（doc/02 §5、doc/07 §8）。
- 2 コア目は止めたままにする前提（L2 を suspend している間に他コアが外部メモリを触ると固まる [mspi_timing_tuning.c:543-552]）。

## 6. 調査の後の判断（2026-10-30）
- **PSRAM の電源は LDO チャネル 2（VO2）で確定とする**。この Tab5 で動いていた元のファーム（~/repos/tab5_claude_client）の設定が `CONFIG_ESP_LDO_CHAN_PSRAM_DOMAIN=2`、1800mV である（§2.1 の引用と同じ箇所）。回路図での確認ではないが、実機で動いていた設定なので、これに従う（判断）。VO1 には触らない。
- **元のファームは `CONFIG_ESP_ENABLE_PVT=y` で 360MHz を使っていた** [TCC/sdkconfig:2177]。PVT を移植していない今、360MHz にするのは電圧のマージンが分からないので、**§5 の段階 1（360MHz 化）は後回しにする**。ESP-IDF も PSRAM の初期化は 90MHz のうちに行う（§2）ので、段階 0 → 2 → 3 → 4 の順に進め、360MHz は PVT を調べてから行う（判断）。

## 7. 段階 0 の結果（実機、2026-10-30）
カーネルの起動時に読んだ値（`boot.clock_reg`、`boot.clock`。書き込みはしていない）。

| レジスタ | 値 | 読み取れること |
|---|---|---|
| ROOT_CLK_CTRL0 | 0x0000_0060 | `CPU_CLK_DIV_NUM` = 3 → CPLL 360MHz の 4 分周（§1 の推測どおり） |
| ROOT_CLK_CTRL1 / 2 | 0 / 0 | MEM、SYS、APB は分周 1（CPU クロックから） |
| SOC_CLK_CTRL0 | 0xE6E0_7FEF | （未解読） |
| PERI_CLK_CTRL00 | 0x0000_C05D | （未解読。`PSRAM_CLK_SRC_SEL` は bit[13:12] = 0） |
| ANA_PLL_CTRL0 | 0x0000_004C | （未解読） |
| LP HP_CLK_CTRL | 0x1FFF_FFFD | bit28 `HP_MPLL_500M_CLK_EN` は立っている |
| PMU_RF_PWC | 0x0C00_0000 | bit24 `MSPI_PHY_XPD` = 0 → **MPLL は電源が入っていない** |
| PMU_EXT_LDO_P1_0P1A / _ANA | 0x4020_0000 / 0xA000_0000 | VO2 の現在の設定（フィールドの解読は段階 2 で行う） |

- **mcycle と SYSTIMER で測った CPU クロックは 90.004MHz**。§1 のとおり、ブートローダは 90MHz のまま渡している。

## 8. 段階 2・3 の結果（実機、2026-10-30）: PSRAM が動いた
`kernel/src/platform/esp32p4v1/psram.rs`（§2 の手順をそのまま実装）を、制御コマンド `psram` で実行した。ログは [../logs/m4a-psram-bringup-tab5-20261030.txt](../logs/m4a-psram-bringup-tab5-20261030.txt)。**1 回目で、32MiB 全体が誤りなく読み書きできた**。

| 段 | 結果 |
|---|---|
| VO2 | eFuse の較正値 dref = 10、mul = 3 を使った（`efuse=true`）。較正なしの式で計算すると 1.925V になるが、eFuse の値は 1.8V に合わせて較正したもので、IDF もそのまま使う（ldo_ll.h の `ldo_ll_voltage_to_dref_mul`）。カーネルは、計算値が 1.70〜1.95V の外なら書かないようにしている（判断） |
| MPLL | 400MHz。較正は 9µs で終わった。DIV レジスタの読み戻しは 153 = (19 << 3) \| 1 |
| チップ | MR1 のベンダ 0x0D（AP Memory）、MR2 の容量 7（32MiB）、KGD 6（合格）。0x5A6B7C8D の書き込みと読み戻しが一致した |
| チューニング | DQS 位相は 4 通りすべて成功（→ IDF の規則で 67.5°）。遅延線は 31 通りのうち id 5〜29 の 25 通りが成功し、中央の id 17（データ 2、DQS 0）を選んだ。**成功区間は広い**。所要 166ms |
| マップとテスト | MMU（0x5008_E380/E37C）でエントリ 0〜511 を 0x4800_0000 に対応付けた。32MiB にキャッシュ経由で書き、L1D と L2 を書き戻して読み比べた → 誤り 0。非キャッシュの 0x8800_0000 から 4KiB ごとに読んでも一致した |
| 速さ（参考） | CPU 90MHz の 1 ワードずつのループで、書き込み 30MB/s、読み出し 24MB/s。CPU の命令の速さで頭打ちになっている（推測） |

- 全体で 2.6 秒（大半は 32MiB のテスト）。起動のたびに行うには長いので、起動時は短いテストにする（判断）。

## 9. 段階 4 の結果（2026-10-30）: アプリを PSRAM で動かす
### 9.1 実装したもの
- **起動時に PSRAM を初期化する**（`Platform::psram_init`）。起動時のメモリテストは 16KiB ごとに 1 ワードにした（全体で 173ms。大半はチューニングの 166ms）。全域のテストは制御コマンド `psram` で行う（PSRAM の中身を壊すので、プロセスがないときだけ）。
- **アプリプール**: PSRAM があれば 0x4800_0000〜（32MiB）を 4 スロット × 8MiB に分けて使う。なければ従来の L2MEM のプール（`boot.pool` に出す）。
- **コードの同期**（`sync_code`）: PSRAM の範囲なら、L1 D と L2 を書き戻してから L1 I を無効化する（IDF の PSRAM XIP と同じ順）。L2MEM は従来どおり L1 D だけ。
- **L2 を止める前に L1 D を全部書き戻す**（ROM の `Cache_WriteBack_All` 0x4FC0_0414）。`map_flash_linear` と `flash_op` の両方。PSRAM の汚れた行が、止めた L2 へ自動で書き戻されて固まるのを防ぐ（IDF の `esp_cache_suspend_ext_mem_cache`）。
- **t5emu**: PSRAM を模した（`emu/src/psram.rs`）。ROM の `esp_rom_spi_cmd_config` / `_cmd_start` / `_set_op_mode` で、モードレジスタと同期読み書きを処理する（タイミングは常に合うので、チューニングは全点成功する）。PSRAM の MMU、窓 0x4800_0000 と非キャッシュの 0x8800_0000、L1 の不整合のモデル（L2MEM と同じ）、`Cache_WriteBack_All`、MPLL の較正完了ビット。L2 を止めている間に PSRAM の窓に触れたら、フラッシュと同じく止める。
  - ついでに直した誤り: ROM 呼び出しの時間を、どの関数でも a2 を「長さ」とみなして数えていた。SPI の関数では a2 がたまたま大きなアドレスで、1 回に数百万サイクル進み、RWDT が切れた。範囲を取るキャッシュ関数だけ数えるようにした。

### 9.2 実機（Tab5）で確かめたこと（[../logs/m4a-apps-in-psram-tab5-20261030.txt](../logs/m4a-apps-in-psram-tab5-20261030.txt)）
- **自己試験 21 件がすべて、PSRAM 上のアプリで通った**（`proc.start ... base=0x48000000`）。保護の試験（M1）、署名の検証、M2 のプリエンプションと FPU、M3d のタイマ（20ms 間隔のずれ ±40µs 以内）を含む。
- 常駐の後、`install` → `run greeter` → `remove` も PSRAM 上で動いた。
- `run timers 5 40` は、確認 10 で終わった（コード 10）。同時に行ったインストールで、アプリが約 250ms 止まり、全体の締め切り（210ms のワンショット）が先に満了したため。PSRAM の問題ではなく、doc/14 §11.4 の既知の制限（インストール中に止まる）の現れ。
- **命令の一貫性**: L1 D と L2 の書き戻し + L1 I の無効化で、PSRAM に写したコードは正しく動いた。`fence.i` だけで足りるかは、まだ試していない。

### 9.3 残っていること
- 360MHz（PVT を調べてから。§6）。
- M4b（XIP: text をフラッシュから実行し、data を PSRAM に置く）。
- スロットの固定割り当て（8MiB × 4）をやめて、必要な大きさだけ割り当てる。
