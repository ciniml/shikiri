# M7 調査: microSD を読み書きする（SDMMC slot 0、ESP32-P4 v1.0）

対象は Tab5 の ESP32-P4 rev v1.0。カーネルは IDF なしでレジスタを直接書く。CPU は 360MHz、PSRAM は M4 で初期化済み。ここではボード（ピンと電源）、SDMMC ホスト（Synopsys DesignWare MMC）、カードの初期化手順、DMA、ファイルシステムの選択肢を扱う。
出典の表記: パスは `~/esp-idf/6.0/components/` からの相対パス。`R1/` は `soc/esp32p4/register/hw_ver1/soc/`、`LL` は `hal/esp32p4/include/hal/sdmmc_ll.h`、`HOST` は `esp_driver_sdmmc/src/sd_host_sdmmc.c`、`TRANS` は `esp_driver_sdmmc/src/sd_trans_sdmmc.c`、`SDM/` は `sdmmc/`（プロトコル層）。`UD/` は `~/repos/M5Tab5-UserDemo/platforms/tab5/components/m5stack_tab5/`（M5Stack の BSP）。`SCH` は公式回路図 PDF（doc/06 の 8 章の URL、作成日 2025-06-09）を pdftoppm で画像にして読んだもの。（推測）は推測、（判断）は判断を表す。

## 0. 結論
- **ピン**: CLK=GPIO43、CMD=GPIO44、D0〜D3=GPIO39〜42。4bit で配線されている。IO_MUX の機能 0（`SD1_*`）で直結し、GPIO マトリクスは通らない（§1.1）。
- **カード検出はない**。ソケットの Detect（9 番ピン）は未接続 [SCH p.2 J2]。BSP も `GPIO_SDMMC_DET = NC` [UD/m5stack_tab5.c:542]。
- **カードの電源は切り替えられない**。カードの VDD もパッド電源 VDDPST_5 も `SOC_3.3V`（SoC 本体と同じ 3.3V）に直結している [SCH p.1 C23/C24、p.2 J2 4 番ピン]。**LDO VO1 にも IO エクスパンダ #2（0x44）にも依存しない**。カードのために書くべき電源レジスタはない。
- BSP は LDO **VO4** を 3.3V で有効にしている [UD/m5stack_tab5.c:538-539,572-583]。しかし回路図では VO4（ネット `ESP_LDO4`）はコンデンサ C29 につながるだけで、ほかのどこにも出ていない [SCH p.1 ピン 74]。Tab5 では意味がない。**カーネルは VO4 に触らない**（判断）。
- **ホスト**: SDMMC は 0x5008_3000 [R1/reg_base.h:44; soc/esp32p4/ld/esp32p4.peripherals.ld:41]。バスクロックは HP_SYS_CLKRST の `SOC_CLK_CTRL1` bit14、リセットは **LP 側**の `LP_AON_CLKRST`（0x5011_104C）bit28 [LL:140-163]。
- **クロック**: ソースは PLL_F160M（SPLL 480MHz / 3）。MPLL（PSRAM）とは無関係。1 段目（HP_SYS_CLKRST の `PERI_CLK_CTRL02` の edge 設定）で 160MHz を分周し、2 段目（ホストの `CLKDIV`）でさらに 2×M 分周する。400kHz = 160/10/(2×20)、20MHz = 160/8、40MHz = 160/4 [HOST:996-1060]。
- **v1 固有**: レジスタの配置は v1 と v3 で同じ（ヘッダの違いは名前だけ）。SDR104 は v3 未満では使えない [HOST:205-212]。3.3V の HS（40MHz）までしか使わないので影響はない。
- **DMA**: IDF は常に内部 DMA（IDMAC）を使い、FIFO のポーリング（PIO）は使わない。PIO は IDF に実績がないので（推測）、最初の 1 セクタ読みで試し、だめなら DMA に切り替える。IDMAC の記述子は 16 バイト（IDF は 64 バイトに詰めてキャッシュ行に合わせる）、1 記述子あたり最大 4096 バイト [R1/sdmmc_struct.h:14-46]。PSRAM のバッファも DMA できる [soc_caps.h:502]。CPU 側では L1 D と L2 の書き戻しと無効化が要る（§5.3）。
- **ファイルシステム**: `embedded-sdmmc` 0.10.0（MIT/Apache-2.0、no_std、alloc 不要、FAT16/32、8.3 名で作成、LFN は読み出しと LFN で開くだけ）を U モードのサービスで使うのが一番早い（§7）。カーネルはブロックデバイス（512 バイトの読み書き）だけを提供する（判断）。**exFAT は読めない**ので、64GB 以上のカードは FAT32 に初期化し直す必要がある。

## 1. ボード

### 1.1 ピン
| 信号 | GPIO | IO_MUX 機能 0 | SCH のネット | 出典 |
|---|---|---|---|---|
| CLK | 43 | `SD1_CCLK_PAD` | `SDIO1_SCK` | R1/io_mux_reg.h:417; soc/esp32p4/include/soc/sdmmc_pins.h:9 |
| CMD | 44 | `SD1_CCMD_PAD` | `SDIO1_CMD` | io_mux_reg.h:424; sdmmc_pins.h:10 |
| D0 | 39 | `SD1_CDATA0_PAD` | `SDIO1_D0` | io_mux_reg.h:389; sdmmc_pins.h:11 |
| D1 | 40 | `SD1_CDATA1_PAD` | `SDIO1_D1` | io_mux_reg.h:396; sdmmc_pins.h:12 |
| D2 | 41 | `SD1_CDATA2_PAD` | `SDIO1_D2` | io_mux_reg.h:403; sdmmc_pins.h:13 |
| D3 | 42 | `SD1_CDATA3_PAD` | `SDIO1_D3` | io_mux_reg.h:410; sdmmc_pins.h:14 |

- BSP も同じピンで 4bit を使う [UD/include/bsp/m5stack_tab5.h:72-78; UD/m5stack_tab5.c:541-549,592-598]。UserDemo は起動時に `bsp_sdcard_init("/sd", 25)` を呼ぶので、この設定で実際に動いている [M5Tab5-UserDemo/platforms/tab5/main/hal/hal_esp32.cpp:283]。
- slot 0 は IO_MUX 専用で、GPIO マトリクスでは出せない（`SDMMC_LL_SLOT_SUPPORT_GPIO_MATRIX(0) = 0`）[LL:89-90; soc/esp32p4/sdmmc_periph.c:50-62]。
- SPI モードの割り当て（CLK=43、MOSI=44、MISO=39、CS=42）もある [TCC/components/M5Unified/src/M5Unified.cpp:173]。SDMMC が動かないときの逃げ道になる（判断）。
- ESD 保護（ESD0524P × 2）が入っている。CMD、D0〜D3 は 5.1kΩ（R21、R22 のアレイ）で SOC_3.3V にプルアップされている。CLK には 22Ω の直列抵抗 R23 がある [SCH p.2]。CLK のカード側にも R21 の 1 素子がつながっているように見える（推測、図の読み取り）。
- 外付けのプルアップがあるので、内部プルアップは要らない。BSP も `SDMMC_SLOT_FLAG_INTERNAL_PULLUP` をコメントアウトしている [UD/m5stack_tab5.c:600]。

### 1.2 電源（最重要）
- カードの VDD（J2 の 4 番ピン）は `SOC_3.3V` に直結。スイッチも LDO もない [SCH p.2 J2、C46]。
- P4 の VDDPST_5（ピン 85）も `SOC_3.3V` に直結（C23 1µF、C24 100nF）[SCH p.1]。ピン 85 は GPIO43 と GPIO44 の間にあり、GPIO39〜48 のパッド電源と思われる（推測。データシートで未確認）。
- `SOC_3.3V` は SoC の VDDPST_4、VDDPST_LDO、VDDPST_DCDC にもつながっている [SCH p.1 ピン 62、75、77]。CPU が動いている間は必ずオン。
- VO1（ネット `ESP_LDO1`）は VDDPST_3 [FLASH_PWR] とフラッシュにつながる [SCH p.1 ピン 71]。**SD の経路には出てこない**。
- IO エクスパンダ #1/#2 のビット表（doc/06 §1.3）にも SD の電源ビットはない。**#2 を書く必要はない**。
- 結果として、カードの電源を入れ直す手段がない。カードが応答しなくなったら CMD0 で戻すしかない。1.8V への切り替え（CMD11）をすると、電源を切るまで 3.3V に戻せない。**CMD11 は絶対に送らない**（判断。SD 物理層仕様 §4.2.4 の電圧切替は、電源の再投入でしか戻らない）。
- `SOC_SDMMC_IO_POWER_EXTERNAL = 1`（IO 電源は外部から）[soc/esp32p4/include/soc/soc_caps.h:501]。IDF のこの仕組みは LDO を通して IO 電圧を変えるためのもの [SDM/sd_pwr_ctrl/sd_pwr_ctrl_by_on_chip_ldo.c:29-83]。Tab5 のパッド電圧は 3.3V に固定なので、パッドの電圧設定レジスタも要らない（推測）。
- BSP の VO4 のコメント「`LDO_VO4` is used as the SDMMC IO power」[UD/m5stack_tab5.c:573] は、ESP32-P4 Function EV Board 向けの BSP からの写しと思われる（推測）。

## 2. SDMMC ホスト（0x5008_3000）

### 2.1 クロックとリセット
| レジスタ | アドレス | フィールド | 設定 | 出典 |
|---|---|---|---|---|
| SOC_CLK_CTRL1 | 0x500E_6018 | bit14 `SDMMC_SYS_CLK_EN` | 1（RMW。bit12/5 は DSI と GDMA） | R1/hp_sys_clkrst_reg.h:376,475-481; LL:140-144 |
| HP_SDMMC_EMAC_RST_CTRL | 0x5011_104C | bit28 `RST_EN_SDMMC`（bit29 `FORCE_NORST_SDMMC`、bit30/31 は EMAC） | 1 → 0 のパルス（RMW） | R1/lp_clkrst_reg.h:989-999; peripherals.ld:76; LL:158-163 |
| REF_CLK_CTRL0 | 0x500E_6024 | bit[31:24] `REF_160M_CLK_DIV_NUM`（既定 2 → 480/3） | 確認だけ | hp_sys_clkrst_reg.h:867,889 |
| REF_CLK_CTRL2 | 0x500E_602C | bit0 `REF_160M_CLK_EN`（既定 1） | 確認だけ | hp_sys_clkrst_reg.h:982-983 |
| PERI_CLK_CTRL01 | 0x500E_6034 | bit22 `SDIO_HS_MODE`、bit23 `SDIO_LS_CLK_SRC_SEL`（0 = PLL_F160M、1 = SDIO PLL 200MHz）、bit24 `SDIO_LS_CLK_EN` | bit22 = 0、bit23 = 0、bit24 = 1（下位は EMAC なので RMW） | hp_sys_clkrst_reg.h:1153,1210-1230; LL:212-229 |
| PERI_CLK_CTRL02 | 0x500E_6038 | bit8 `CFG_UPDATE`、bit[12:9] `EDGE_L`、bit[16:13] `EDGE_H`、bit[20:17] `EDGE_N`、bit[22:21] SLF、bit[24:23] DRV、bit[26:25] SAM の edge 選択、bit27/28/29 SLF/DRV/SAM の clk_en、**bit[31:30] は DSI D-PHY のソース** | 下記 | hp_sys_clkrst_reg.h:1235-1313; LL:244-315 |

- 1 段目の分周（`sdmmc_ll_set_clock_div`、div > 1 のとき）: `EDGE_H = div/2 − 1`、`EDGE_N = div − 1`、`EDGE_L = div − 1` を書き、`CFG_UPDATE` を 1 → 0 [LL:244-258]。div = 1 は `SDIO_HS_MODE = 1` と edge 全部 0（SDR104 用。使わない）。
- 位相の初期化（`sdmmc_ll_init_phase_delay`）: DRV/SAM/SLF の clk_en = 1、DRV の edge 選択 = 1、SAM = 0、SLF = 0、`CFG_UPDATE` を 1 → 0 [LL:303-315]。
- IDF の順: `set_clock_div` → `select_clk_source`（src = 0、ls_clk_en = 1）→ `init_phase_delay` → 10µs 待つ [HOST:978-994]。
- **PERI_CLK_CTRL02 の bit[31:30] は M6 で DSI が 0 にしている**。同じ値なので壊さないが、必ず RMW にする（doc/research/m6-dsi.md §2）。
- 分周の組（160MHz 基準）[HOST:996-1060]:

| 用途 | 1 段目 div | EDGE_H/L/N | 2 段目 `CLKDIV.clk_divider0` | カードのクロック |
|---|---|---|---|---|
| 初期化時の既定 | 2 | 0 / 1 / 1 | — | （80MHz、コマンド前に変える）[HOST:107] |
| 識別 | 10 | 4 / 9 / 9 | 20（2×20 で割る） | 400kHz |
| 既定速度 | 8 | 3 / 7 / 7 | 0（バイパス） | 20MHz |
| HS | 4 | 1 / 3 / 3 | 0 | 40MHz |

- IDF は起動時に `SDHOST_CLK_EDGE_SEL`（+0x800）の bit23 `CCLK_EN`（既定 1）を 0 にする [esp_system/port/soc/esp32p4/clk.c:297]。戻す処理は見当たらない。SDMMC のリセットで既定値に戻ると思われる（推測）。**カーネルはリセット後に +0x800 を読んで記録する**（判断）。+0x800 の edge 系フィールドは IDF が使っていない（v1 の S3 型の名残。推測）。

### 2.2 ホストのレジスタ（使うもの）
[R1/sdmmc_reg.h の行番号]

| オフセット | 名前 | 使うフィールド | 行 |
|---|---|---|---|
| +0x00 | CTRL | bit0 controller_reset、bit1 fifo_reset、bit2 dma_reset（自動で 0 に戻る）、bit4 int_enable、bit5 dma_enable、bit25 use_internal_dma | 17-110; R1/sdmmc_struct.h:78,133 |
| +0x08 | CLKDIV | bit[7:0] clk_divider0（slot 0 用） | 124-125 |
| +0x0C | CLKSRC | bit[1:0] slot 0 の分周器選択 = 0 | 165; LL:431-442 |
| +0x10 | CLKENA | bit0 cclk_enable、bit16 lp_enable（アイドル時にクロック停止） | 184-195 |
| +0x14 | TMOUT | bit[7:0] 応答タイムアウト（IDF は 255）、bit[31:8] データタイムアウト（IDF は 100ms 分のクロック数） | 208-217; HOST:529-533 |
| +0x18 | CTYPE | bit0 slot 0 の 4bit、bit16 8bit | 233-244 |
| +0x1C | BLKSIZ | bit[15:0]（既定 512） | 258-259 |
| +0x20 | BYTCNT | 転送する総バイト数（既定 512） | 270-271 |
| +0x24 | INTMASK | 割り込みのマスク（ポーリングでは 0） | 285 |
| +0x28 | CMDARG | 引数 | 323 |
| +0x2C | CMD | §3.1 | 335-495 |
| +0x30〜0x3C | RESP0〜3 | 応答。R2 は RESP0 が bit[31:0]（推測。IDF は 4 語をそのまま memcpy する [TRANS:240-244]） | 510-547 |
| +0x44 | RINTSTS | 生の割り込み状態。1 を書いて消す | 596; LL:36-54 |
| +0x48 | STATUS | bit2 fifo_empty、bit3 fifo_full、bit[7:4] コマンド FSM、bit9 data_busy（D0 が Low）、bit[29:17] fifo_count | 636-720 |
| +0x4C | FIFOTH | TX_WMARK [11:0]、RX_WMARK [26:16]、DMA の MSIZE [30:28]。IDF は既定のまま | 731-764 |
| +0x50 | CDETECT | bit0 card_detect_n（0 = あり） | 784; LL:592-595 |
| +0x54 | WRTPRT | bit0 書き込み禁止 | 797 |
| +0x6C / +0x70 | VERID / HCON | 既定 0x5432_270A（1412572938）/ バス 32bit、クロック分周器 4 個など | 861-933 |
| +0x74 | UHS | bit[1:0] volt（1.8V。**0 のまま**）、bit[17:16] DDR | 941; LL:618-643 |
| +0x78 | RST_N | カードのリセット（eMMC 用。既定 1） | 955 |
| +0x80〜+0x98 | BMOD、PLDMND、DBADDR、IDSTS、IDINTEN、DSCADDR、BUFADDR | IDMAC（§5） | 972-1237 |
| +0x200 | BUFFIFO | データ FIFO の窓 | 1330 |

- RINTSTS のビット [LL:36-54]: 0 CD、1 RE（応答エラー）、2 CMD_DONE、3 DTO（データ転送完了）、4 TXDR、5 RXDR、6 RCRC、7 DCRC、8 RTO（応答タイムアウト）、9 DRTO（データ読みタイムアウト）、10 HTO、11 FRUN、12 HLE（ハードウェアロックエラー）、13 SBE/BCI、14 ACD（自動 STOP の完了）、15 EBE。
- IDF のエラーのまとめ方 [TRANS:30-42,238-268,306-328]: コマンドのエラー = RTO | RCRC | RE。データのエラー = DRTO | DCRC | HTO | SBE | EBE。データエラーのときは `fifo_reset` を立てる。

### 2.3 IO の設定
- IO_MUX の基底は 0x500E_1000、GPIOn のレジスタは +0x04 + 4n（GPIO39 = 0x500E_10A0、GPIO44 = 0x500E_10B4）[R1/reg_base.h:10,114; io_mux_reg.h:384]。フィールド: bit7 FUN_PD、bit8 FUN_PU、bit9 FUN_IE、bit[11:10] FUN_DRV、bit[14:12] MCU_SEL、bit15 FILTER_EN [io_mux_reg.h:42-70]。
- IDF の `configure_pin_iomux`: プルダウン解除、入力有効、`gpio_iomux_output(func 0)`、**駆動能力 3** [HOST:1075-1087]。`gpio_iomux_output` は GPIO マトリクスの `FUNCn_OUT_SEL_CFG` の `OEN_SEL`（bit10）を 0 にし（出力有効は周辺が決める）、MCU_SEL を書く [hal/gpio_hal.c:43-47; hal/esp32p4/include/hal/gpio_ll.h:645-685]。`FUNCn_OUT_SEL_CFG` は 0x500E_0558 + 4n（GPIO39 = 0x500E_05F4）[R1/gpio_reg.h:9764,11165-11200]。
- よって GPIO39〜44 の IO_MUX には `MCU_SEL = 0、FUN_DRV = 3、FUN_IE = 1、FUN_PD = 0、FUN_PU = 0` = **0x0000_0E00** を書く。`OUT_SEL_CFG` は bit10 を 0 にする（判断。値は上の出典からの計算）。
- **D3 の扱い**: IDF は 4bit 指定でも、ACMD6 で幅を変えるまで D3 を GPIO の出力 High にしておく（カードを SD モードに入れるため）。幅を変えたあとで IO_MUX の機能 0 に戻す [HOST:1276-1293,572-578]。Tab5 は D3 に 5.1kΩ のプルアップがあるので、カーネルは **ACMD6 までは D3 を GPIO 入力（MCU_SEL = 1、出力無効）にしておけばよい**（推測）。
- **カード検出と書き込み禁止の入力**: slot 0 の CD/WP/INT は GPIO マトリクスの入力信号 126/130/128（`SD_CARD_DETECT_N_1`、`SD_CARD_WRITE_PRT_1`、`SD_CARD_INT_N_1`）[soc/esp32p4/include/soc/gpio_sig_map.h:234-242; sdmmc_periph.c:8-14]。IDF はピンがなければ CD に定数 0（カードあり）、WP に定数 1 を反転して入れ（= 0、禁止でない）、INT に定数 1 を入れる [HOST:1295-1334]。
  - 入力の設定レジスタは `GPIO_FUNCn_IN_SEL_CFG` = 0x500E_0158 + 4n。bit[5:0] IN_SEL（0x3E = 常に Low、0x3F = 常に High。**既定は 0x3F**）、bit6 反転、bit7 SIG_IN_SEL（1 = マトリクスを通す）[R1/gpio_reg.h:3716-3735,6551,6605,6659; soc/esp32p4/include/soc/gpio_pins.h:13-14]。
  - 書く値: 0x500E_0350（信号 126、CD）= 0xBE、0x500E_0360（130、WP）= 0xBE、0x500E_0358（128、INT）= 0xBF（判断。上の出典からの計算）。
  - **既定のままだと CD は 1（カードなし）に見える**。IDF はカードなしならコマンドを出さない [HOST:851-853]。ハードウェア自体がコマンドを止めるかは不明（推測）。どちらにせよ設定しておく。

## 3. コマンド（ポーリング）

### 3.1 CMD レジスタの組み立て
- ビット [R1/sdmmc_struct.h の cmd レジスタ、sdmmc_reg.h:335-495]: [5:0] index、6 response_expect、7 response_long、8 check_response_crc、9 data_expected、10 rw（1 = 書き込み）、11 transfer_mode、12 send_auto_stop、13 wait_prvdata_complete、14 stop_abort_cmd、15 send_initialization（80 クロックを先に出す）、[20:16] card_number、21 update_clock_registers_only、28 volt_switch、29 use_hold_reg、31 start_cmd。
- IDF の規則 [TRANS:190-229; HOST:857-892]: 常に `use_hold_reg = 1`。CMD0 は `send_init`、CMD12 は `stop_abort`、それ以外は `wait_prvdata_complete`。応答があれば `response_expect`、R2 は `response_long`、CRC 付きの応答は `check_response_crc`。データ付きは `data_expected`、書き込みは `rw`、CMD18/25 は `send_auto_stop`。
- 発行の手順: `CMD.start_cmd` が 0 になるのを待つ → CMDARG → CMD を書く → `start_cmd` が 0 に戻るのを待つ（上限 1 秒 [esp_driver_sdmmc/include/esp_private/sd_host_private.h:61]）→ RINTSTS の CMD_DONE とエラーを待つ → RINTSTS を書いて消す。
- 計算した CMD の値（slot 0、use_hold 付き。判断）:

| コマンド | 応答 | CMD の値 | 引数 |
|---|---|---|---|
| クロック更新（コマンドではない） | — | 0xA020_2000 | 0 |
| CMD0 GO_IDLE | なし | 0xA000_8000 | 0 |
| CMD8 SEND_IF_COND | R7 | 0xA000_2148 | 0x1AA |
| CMD55 APP_CMD | R1 | 0xA000_2177 | RCA << 16（最初は 0） |
| ACMD41 SD_SEND_OP_COND | R3（CRC なし） | 0xA000_2069 | 0x40FF_8000（HCS + 2.7〜3.6V） |
| CMD2 ALL_SEND_CID | R2 | 0xA000_21C2 | 0 |
| CMD3 SEND_RELATIVE_ADDR | R6 | 0xA000_2143 | 0 |
| CMD9 SEND_CSD | R2 | 0xA000_21C9 | RCA << 16 |
| CMD7 SELECT_CARD | R1b | 0xA000_2147 | RCA << 16 |
| CMD13 SEND_STATUS | R1 | 0xA000_214D | RCA << 16 |
| CMD16 SET_BLOCKLEN | R1 | 0xA000_2150 | 512（SDSC のみ） |
| ACMD51 SEND_SCR | R1 + 8 バイト | 0xA000_2373 | 0 |
| CMD6 SWITCH_FUNC（読み 64 バイト） | R1 + 64 バイト | 0xA000_2346 | 0x00FF_FFF1（確認）/ 0x80FF_FFF1（切替） |
| ACMD6 SET_BUS_WIDTH | R1 | 0xA000_2146 | 2（4bit） |
| CMD17 READ_SINGLE | R1 + 512 | 0xA000_2351 | ブロック番号（SDSC はバイト位置） |
| CMD24 WRITE_SINGLE | R1 + 512 | 0xA000_2758 | 同上 |
| CMD18 READ_MULTIPLE | R1 + n×512 | 0xA000_3352 | 同上 |
| CMD25 WRITE_MULTIPLE | R1 + n×512 | 0xA000_3759 | 同上 |
| CMD12 STOP | R1b | 0xA000_414C | 0 |

- ACMD41 の引数: IDF は電圧マスクを常に `0xFF8000`（bit23:15）とし、CMD8 が通れば HCS（bit30）を足す [SDM/include/esp_private/sdmmc_common.h:175-181; SDM/sdmmc_sd.c:28-48; SDM/sdmmc_common.c:25-58]。**S18R（bit24）と XPC は立てない**（立てるのは UHS-I の slot だけ [sdmmc_common.c:47-54]）。
- 応答の確認: CMD8 は応答の下位 8bit が 0xAA [SDM/sdmmc_cmd.c:87-105]。ACMD41 は R3 の bit31（電源投入完了）が立つまで 10ms ごとに最大 300 回 [sdmmc_cmd.c:107-166; sdmmc_common.h:56-57]。bit30（CCS）が 1 なら SDHC/SDXC でブロック単位のアドレス、0 なら SDSC でバイト単位 [sdmmc_cmd.c:514-518,649]。
- CMD0 のあと IDF は 20ms 待つ [sdmmc_cmd.c:80-82; sdmmc_common.h:36]。
- ACMD41 の間、IDF はカードのクロックを止めない（`lp_enable = 0`）[sdmmc_cmd.c:111-122; HOST:270-283]。カーネルは最初から `lp_enable = 0` にしておく（判断）。

### 3.2 カードのクロックの設定（IDF `sd_host_slot_set_card_clk`）
[HOST:485-536]
1. CLKENA の bit0 を 0 → クロック更新コマンド。
2. CLKDIV.clk_divider0 と CLKSRC を書き、1 段目の分周（§2.1）を設定 → クロック更新コマンド。
3. CLKENA の bit0 を 1（IDF は bit16 も 1）→ クロック更新コマンド。
4. TMOUT: データタイムアウト = 100 × kHz（400kHz なら 40,000、40MHz なら 4,000,000。上限 0xFF_FFFF）、応答タイムアウト = 255。
- クロック更新コマンドが受け付けられないときは RINTSTS の HLE（bit12）が立つ（推測。DesignWare の通常の動き）。

### 3.3 データの読み書き（PIO、DMA なし）
IDF に実績がないので推測を含む。DesignWare MMC の一般的な手順に従う。
- 準備: CTRL の dma_enable（bit5）と use_internal_dma（bit25）を 0、BMOD を 0。BLKSIZ と BYTCNT を書いてからコマンドを出す。
- **読み（CMD17、ACMD51、CMD6）**: CMD_DONE とエラーを確かめたあと、STATUS の fifo_count（bit[29:17]）が 0 でない間 +0x200 から 32bit ずつ読む。RINTSTS の DTO が立ったら残りを読み切る。DRTO/DCRC/SBE/EBE ならエラー。
- **書き（CMD24）**: STATUS の fifo_full（bit3）が 0 の間 +0x200 に書く。DTO を待ったあと、STATUS の data_busy（bit9）が 0 になるまで待つ（カードのプログラム中は D0 が Low）。IDF はさらに CMD13 で `READY_FOR_DATA` かつ TRAN 状態を待つ [sdmmc_cmd.c:520-556; sdmmc_common.h:183-186]。書き込みの上限は 5 秒 [sdmmc_common.h:48]。
- ウォーターマーク（FIFOTH）は既定 0 のままでよい。fifo_count を直接見るので深さに依存しない（判断）。FIFO の深さは未確認。
- 400kHz での 512 バイトは約 1.3ms、40MHz・4bit なら約 13µs（計算）。PIO でも 2MB/s 程度は出るはず（推測）。

## 4. IDF の初期化の順序
`sdmmc_card_init` [SDM/sdmmc_init.c:45-190] と、その前のホスト側の処理。

**ホスト側**（`sd_host_create_sdmmc_controller` [HOST:70-135]、`sdmmc_slot_io_config` [HOST:1149-1336]）
1. バスクロック有効 → LP 側のリセットパルス [HOST:743-767]。
2. 1 段目の分周を div 2 で設定 [HOST:107]。
3. CTRL の controller/dma/fifo_reset を立て、自動で 0 になるのを待つ（上限 5 秒）[HOST:915-951; sd_host_private.h:62]。
4. RINTSTS を全部消し、割り込みを設定、`init_dma`（dma_enable = 1、BMOD のソフトリセット）[HOST:118-123; LL:796-804]。
5. ピンの設定。D3 は GPIO High。CD/WP/INT に定数を入れる。

**カード側**（BSP の設定: `max_freq_khz = 40000`、4bit、io_voltage 3.3）[UD/m5stack_tab5.c:568-598; esp_driver_sdmmc/legacy/include/driver/sdmmc_default_configs.h:25-33]
1. `sd_pwr_ctrl_set_io_voltage(3300)`（BSP では VO4。Tab5 では無意味）[sdmmc_init.c:60-66]。
2. 最初のコマンドの前に 400kHz・1bit に設定 [TRANS:491-505]。
3. CMD0（send_init 付き）→ 20ms 待つ [sdmmc_init.c:84]。
4. CMD8: 応答があれば SDHC/SDXC 候補（HCS を立てる）、タイムアウトなら v1 カード [sdmmc_init.c:87; sdmmc_sd.c:28-48]。
5. ACMD41 を繰り返す。タイムアウトなら MMC として CMD1 を試す [sdmmc_init.c:101; sdmmc_common.c:57-66]。
6. UHS-I（CMD11 で 1.8V）は、slot が UHS-I 指定のときだけ [sdmmc_init.c:110-112]。BSP は指定しない。**カーネルは実装しない**。
7. CMD2（CID）→ CMD3（RCA）→ CMD9（CSD）→ CMD7（選択）[sdmmc_init.c:115-130]。
8. SDSC だけ CMD16 = 512 [sdmmc_init.c:137; sdmmc_sd.c:50-64]。
9. ACMD51 で SCR を読む（D0 を使う最初のコマンド。ここで失敗するならプルアップか配線の問題 [sdmmc_sd.c:66-73]）→ CMD13 で READY_FOR_DATA を待つ（上限 5 秒）[sdmmc_init.c:138-139; sdmmc_sd.c:147-174]。
10. HS への切替: SCR の版が 1.10 以上で CSD の CCC に SWITCH があれば、CMD6（mode 0）で対応を確認し、CMD6（mode 1、group 1 = 1 = SDR25/HS）で切り替える。そのあと CMD7(0) で選択を外し、CMD9 で CSD を読み直し、CMD7(RCA) で選び直す [sdmmc_common.c:178-197; sdmmc_sd.c:176-315,517-562]。非対応なら 20MHz のまま。
11. ACMD6 で 4bit → ホストの CTYPE を 4bit、D3 を IO_MUX に戻す [sdmmc_init.c:157-162; sdmmc_sd.c:131-145; HOST:564-588]。
12. ACMD13 で SD Status を読む [sdmmc_init.c:172]。
13. ホストのクロックを 40MHz（HS）または 20MHz に上げる [sdmmc_init.c:175; sdmmc_common.c:241-256]。
14. ACMD51 で SCR をもう一度読み、同じ値か確かめる（速度と幅を変えたあとの確認）[sdmmc_init.c:181]。

- 40MHz（HS）の位相: IDF は既定で位相 0（SAM の edge 選択 0）のまま [HOST:610-682]。BSP も位相を指定していない。

## 5. 内部 DMA（IDMAC）

### 5.1 記述子
[R1/sdmmc_struct.h:14-46]
| 語 | 内容 |
|---|---|
| DES0 | bit1 DIC（完了割り込みを出さない）、bit2 LD（最後）、bit3 FS（最初）、bit4 CH（DES3 が次の記述子）、bit5 ER（リングの最後）、bit30 CES（カードエラー）、bit31 OWN（1 = DMA が所有） |
| DES1 | bit[12:0] バッファ 1 の長さ、bit[25:13] バッファ 2 の長さ |
| DES2 | バッファ 1 のアドレス |
| DES3 | 次の記述子のアドレス（CH = 1 のとき） |
| 残り 48 バイト | IDF がキャッシュ行（64 バイト）に合わせるための詰め物 |

- IDF の 1 記述子の最大は 4096 バイト（`SDMMC_DMA_MAX_BUF_LEN`）。長さは 4 の倍数に切り上げる [sdmmc_struct.h:43; TRANS:107-138]。
- IDF の組み方: 全記述子を 0 にし、先頭に FS、各記述子に CH = 1・OWN = 1、最後に LD [TRANS:153-174]。記述子は既定 4 個で、足りない分は割り込みの中で詰め直す [sd_host_private.h:68; TRANS:79-105]。カーネルは転送の最大長に合わせて記述子を用意し、詰め直しはしない（判断。64KB なら 16 個）。
- アドレスは CPU と同じアドレス（L2MEM の 0x4FFx_xxxx、PSRAM の 0x48xx_xxxx）をそのまま書く。IDF はポインタをそのまま DES2 と DBADDR に入れている [TRANS:124,171]。

### 5.2 転送の手順
1. 初期化で一度: CTRL.dma_enable = 1、BMOD = 0、BMOD.SWR（bit0）= 1、IDINTEN の TI/RI/NI = 1（ポーリングでも IDSTS は立つ）[LL:796-804]。
2. 転送ごと: 記述子を書く → BYTCNT、BLKSIZ → DBADDR（+0x88）→ CTRL.dma_enable = 1・use_internal_dma = 1、BMOD.DE（bit7）= 1・FB（bit1）= 1 → PLDMND（+0x84）に 1 → コマンド [TRANS:153-174; LL:673-686,812-818]。
3. 完了: IDSTS（+0x8C）の TI（bit0、送信完了）か RI（bit1、受信完了）→ RINTSTS の DTO。IDSTS は 1 を書いて消す [TRANS:35-37,384-405; LL:906-920]。FBE（bit2、バスエラー）、DU（bit4、記述子が使えない）、CES（bit5）はエラー。
4. エラーやタイムアウトのとき: use_internal_dma = 0、dma_reset = 1、BMOD.FB = 0、DE = 0 [LL:825-831]。IDF はここで dma_reset の完了を待っていない（コメントで問題の可能性を認めている）。カーネルは CTRL bit2 が 0 に戻るまで待つ（判断）。
5. CMD18/25 は `send_auto_stop` で CMD12 をホストが出す。完了は RINTSTS の ACD（bit14）でもわかる [TRANS:180-188]。書き込みの失敗時、IDF は ACMD22 で書けたブロック数を聞く [sdmmc_cmd.c:524-536]。

### 5.3 キャッシュ
- P4 は内部 SRAM も L1 キャッシュを通る（`SOC_CACHE_INTERNAL_MEM_VIA_L1CACHE`）[soc_caps.h:183]。PSRAM は L1 と L2 を通る。L1/L2 の行は 64 バイト [TCC/sdkconfig:2458-2464]。
- IDF は転送の前にバッファを書き戻し（C2M）、読みのあとで無効化（M2C）する。記述子も書いたあとに書き戻し、DMA が書き換えた記述子を読む前に無効化する [TRANS:91,136,540-546,575-581]。
- バッファの条件: キャッシュ行の境界（64 バイト）に合わせ、長さも 64 の倍数 [HOST:455-483]。読みのあとの無効化で隣のデータを消さないため。
- カーネルでの対応（判断）:
  - 書き込み前: `cache_writeback(buf, len, l2)`（ROM `Cache_WriteBack_Addr` 0x4FC0_03F4、L1 D → L2 の順）[kernel/src/platform/esp32p4v1.rs:261-269; doc/research/m4-psram-and-clock.md §2.6・§3]。
  - 読みの前: 書き戻し（汚れた行があとで DMA の結果を上書きしないように）。読みのあと: `cache_invalidate(buf, len, l2)`（ROM `Cache_Invalidate_Addr` 0x4FC0_03E4）[esp32p4v1.rs:248-257]。I キャッシュの無効化は要らない（データだけ）。
  - 記述子: L2MEM に置き、非キャッシュの別名（+0x4000_0000）から書く（M6 の LLI と同じ方法 [doc/research/m6-dsi.md §5]）。DES2/DES3/DBADDR にはキャッシュ側のアドレスを書く（推測。DMA が 0x8FFx 側を扱えるかは未確認）。
- APM（doc/02 §5）が SDMMC の DMA から PSRAM への読み書きを許しているかは未確認（推測で既定のまま通る。M6 の DW-GDMA と同じ扱い）。

## 6. ブロック 0 から FAT まで
- セクタ 0 は MBR。0x1FE〜0x1FF が 0x55 0xAA。パーティション表は 0x1BE から 16 バイト × 4。種類 0x0B/0x0C が FAT32（LBA）、0x06/0x0E が FAT16、0x07 が exFAT/NTFS、0xEE は GPT の保護 MBR（SD 協会の仕様の FAT 形式。https://www.sdcard.org/downloads/pls/ の Part 2 File System）。
- MBR がなくセクタ 0 が直接 BPB のカード（superfloppy）もある。先頭が 0xEB/0xE9 で、0x52 に "FAT32   " があれば BPB と判断する（判断）。
- SD 協会の規格では、32GB 以下（SDHC）は FAT32、それを超える SDXC は exFAT で出荷される（SD Physical Layer Simplified Spec と Part 2 の容量区分。上の URL）。

## 7. ファイルシステムの選択肢

| 候補 | 版（crates.io、2026-10-31 に照会） | ライセンス | no_std / alloc | FAT | 書き込み | 長いファイル名 | 備考 |
|---|---|---|---|---|---|---|---|
| `embedded-sdmmc` | 0.10.0（2026-07-24、MSRV 1.87、edition 2024） | MIT OR Apache-2.0 | no_std、**alloc 不要** | FAT16/32（FAT12 は非対応 [src/fat/bpb.rs:41]） | 可（作成、追記、削除、ディレクトリ作成） | 読み出し（`iterate_dir_lfn`）と LFN で開く（`open_long_name_file_in_dir`）だけ。作るのは 8.3 名 | `BlockDevice` は同期の `read(&self, &mut [Block], BlockIdx)`、`write`、`num_blocks`。Block は 512 バイト。2TiB まで。開けるファイル数などは const generics。内部は `RefCell` |
| `fatfs`（rafalh） | 0.3.6（2023-01-17） | MIT | 0.3.6 の no_std は `core_io`（古い nightly 向け）頼み。features は `alloc`/`std`/`chrono` | FAT12/16/32 | 可、フォーマットも可 | master の README では `lfn` feature で読み書き。0.3.6 には `lfn` feature がない | master は未リリース（crates.io の版と README が食い違う） |
| `fat32` | 0.2.0（2020） | MIT | — | FAT32 | — | — | 長く更新されていない。候補にしない |
| 自作 | — | — | — | FAT32 だけ | 読みだけなら 300〜500 行（推測） | 読みは容易 | 書き込み（FAT の 2 面、FSInfo、ディレクトリの更新）は手間が大きい |
| （参考）IDF の FatFs（ChaN） | IDF 6.0 同梱 [fatfs/] | 独自の BSD 系 | C | FAT/exFAT | 可 | 可 | Rust から使うには C のビルドが要る。採らない |

出典: https://crates.io/api/v1/crates/embedded-sdmmc 、https://docs.rs/embedded-sdmmc/0.10.0/embedded_sdmmc/ 、https://docs.rs/embedded-sdmmc/0.10.0/embedded_sdmmc/trait.BlockDevice.html 、embedded-sdmmc 0.10.0 の CHANGELOG.md:9-51（crate を展開して確認）、https://crates.io/api/v1/crates/fatfs と /0.3.6/dependencies、https://github.com/rafalh/rust-fatfs 、https://crates.io/api/v1/crates/fat32 。

- **推奨（判断）**: `embedded-sdmmc` を **U モードのサービス**（ストレージサーバ）で使う。カーネルは SDMMC ドライバとセクタ単位の read/write（ハンドル付きチャネル）だけを持つ。理由:
  - alloc なしで動き、FAT の処理は数千行あるので、カーネルの外に置くと失敗が隔離される。
  - `BlockDevice` は同期の 3 関数だけなので、チャネル越しのセクタ要求に包むのが簡単。
  - 依存（`heapless`、`embedded-io` 0.7、`embedded-hal` 1.0、`thiserror` 2 など）は no_std の範囲。サービスのビルドに入るかは確認が要る（推測）。
- **注意**: exFAT のカードは読めない。最初は 32GB 以下のカードを FAT32 で使う。作るファイルは 8.3 名に限られる。
- `embedded-sdmmc` に付属のドライバは SPI 用で、SDMMC ホストは自分で書く（docs.rs のトップページ）。

## 8. 推奨する立ち上げ順（カーネル）
ログは `event!` で出す（doc/12）。名前は案。

| # | 操作 | アドレス・値 | 待ち | ログ |
|---|---|---|---|---|
| 1 | 電源の確認だけ | 書くものはない（SOC_3.3V 直結）。**VO1、VO4、IO エクスパンダ #2 には触らない** | — | — |
| 2 | 160MHz の確認 | 0x500E_6024 bit[31:24] = 2、0x500E_602C bit0 = 1 | — | `sd.clk` ref160_div, ref160_en |
| 3 | バスクロック | RMW 0x500E_6018 bit14 = 1 | — | |
| 4 | リセット | RMW 0x5011_104C bit28 = 1 → 0 | 数 µs（推測） | |
| 5 | 1 段目の分周 | RMW 0x500E_6038（div 10: EDGE_H 4、L 9、N 9、CFG_UPDATE 1 → 0）、RMW 0x500E_6034（bit23 = 0、bit24 = 1）、0x500E_6038 の位相（clk_en 3 個 = 1、DRV edge = 1、CFG_UPDATE 1 → 0） | 10µs | |
| 6 | ホストのリセット | 0x5008_3000 に bit0/1/2 を立て、0 に戻るまで | 上限 5 秒 | `sd.host` verid, hcon, clk_edge_sel(+0x800), status |
| 7 | 割り込みを使わない | INTMASK = 0、CTRL.int_enable = 0、RINTSTS = 0xFFFF_FFFF、IDSTS = 0x3FF | — | |
| 8 | IO | GPIO39/40/41/43/44 の IO_MUX = 0x0E00、OUT_SEL_CFG bit10 = 0。GPIO42（D3）は MCU_SEL = 1・入力 | — | |
| 9 | CD/WP/INT | 0x500E_0350 = 0xBE、0x500E_0360 = 0xBE、0x500E_0358 = 0xBF | — | `sd.cd` cdetect, wrtprt |
| 10 | 400kHz | CLKENA = 0 → 更新、CLKDIV = 20・CLKSRC = 0 → 更新、CLKENA = 0x1 → 更新、TMOUT = (40000 << 8) \| 255、CTYPE = 0 | — | `sd.clk` khz=400 |
| 11 | 電源投入の待ち | SD の規格では電源が安定してから 1ms と 74 クロック以上（SD Physical Layer Simplified Spec §6.4.1）。電源はずっと入っているので、CMD0 の send_init（80 クロック）で足りる（推測） | 1ms | |
| 12 | CMD0 | 0xA000_8000 | 20ms | `sd.cmd` op=0, rints |
| 13 | CMD8 | 0x1AA | — | `sd.if_cond` resp（タイムアウトなら v1） |
| 14 | ACMD41 | CMD55(0) + ACMD41(0x40FF_8000) を 10ms ごとに | 最大 300 回（3 秒） | `sd.ocr` ocr, tries, ccs |
| 15 | CMD2、CMD3、CMD9 | — | — | `sd.cid` mid, oid, pnm、`sd.csd` ver, c_size, ccc, blocks, `sd.rca` |
| 16 | CMD7(RCA)、SDSC なら CMD16 | — | — | |
| 17 | **CMD17 でセクタ 0（PIO、1bit、400kHz）** | BLKSIZ = BYTCNT = 512 | — | `sd.read` lba=0, sig=0x55AA, part[0..3] type/start/len |
| 18 | ACMD51（SCR）、CMD13 | 8 バイト | — | `sd.scr` spec, widths |
| 19 | HS への切替（任意） | CMD6 mode 0 → mode 1 → CMD7(0)・CMD9・CMD7(RCA) | — | `sd.hs` supported, switched |
| 20 | 4bit | CMD55 + ACMD6(2) → CTYPE bit0 = 1 → GPIO42 を IO_MUX 0x0E00 | — | `sd.width` 4 |
| 21 | 速度 | まず 20MHz（div 8、CLKDIV 0）。HS が通れば 40MHz（div 4）。TMOUT を更新 | — | `sd.clk` khz |
| 22 | 確認 | ACMD51 をもう一度読み、18 と比べる。セクタ 0 を読み直して 17 と比べる | — | `sd.verify` ok |
| 23 | 書き込み | 予約した試験用のセクタ（パーティションの外か、専用ファイル）に CMD24 → CMD13 で TRAN を待つ → CMD17 で読み比べ | 上限 5 秒 | `sd.write` lba, ok, busy_us |
| 24 | DMA | CMD18/25 で 8〜128 セクタ。キャッシュは §5.3 | — | `sd.dma` n, us, idsts |
| 25 | FAT | U モードのサービスで `embedded-sdmmc` を使い、ルートディレクトリを列挙 | — | サービス側のログ |

- 失敗したときの記録: RINTSTS、STATUS（コマンド FSM の bit[7:4] を含む）、IDSTS、最後の CMD と引数を必ず出す（判断）。
- 再試行: カードの電源を切れないので、CMD0 からやり直す。リブートをまたいでカードが TRAN 状態のままでも、CMD0 で IDLE に戻る（SD 規格。推測で実機未確認）。
- 自己試験（t5emu）: エミュレータには SDMMC がない。カードのモデル（コマンドへの応答と 512 バイトのイメージ）を足すかは別に判断する（判断）。

## 9. 未確認事項
- **PIO（FIFO のポーリング）が P4 の SDMMC で動くか**。IDF は常に DMA を使う。FIFO の深さも未確認。
- VDDPST_5 が GPIO39〜48 のパッド電源かどうか（データシートの電源ドメイン表で確認する）。
- CDETECT が 1（カードなし）のときに、ハードウェアがコマンドを止めるか。
- `SDHOST_CLK_EDGE_SEL.CCLK_EN`（IDF が起動時に 0 にする）が SDMMC のリセットで 1 に戻るか。また、このビットが P4 で意味を持つか。
- ホスト側のリセット後の待ち時間（LP_AON のリセットパルスの幅）。
- IDMAC が L2MEM の非キャッシュ別名（0x8FFx_xxxx）のアドレスを扱えるか。カーネルはキャッシュ側のアドレスを渡す前提。
- APM が SDMMC の DMA から PSRAM / L2MEM への書き込みを許しているか。
- R2 応答の語の順（RESP0 = bit[31:0] と考えている）。CID/CSD の解釈で確かめる。
- 40MHz（HS）で位相 0 のまま安定するか（ESD 保護と 22Ω の直列抵抗がある）。だめなら 20MHz に留める。
- `embedded-sdmmc` 0.10.0 の依存が U モードのサービスのビルド（ターゲット、MSRV 1.87、edition 2024）で通るか。
- BSP の VO4 は Tab5 で本当にどこにもつながっていないか（回路図の 1 ページ目しか見ていない。ネット `ESP_LDO4` は PDF 全体で 1 か所だけ）。
