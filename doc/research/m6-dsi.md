# M6 調査: MIPI-DSI でフレームバッファを表示する（SoC 側、ESP32-P4 v1.0）

対象は ESP32-P4 rev v1.0（Tab5、`CONFIG_ESP32P4_SELECTS_REV_LESS_V3`、`REV_MIN_FULL=100` [TCC/sdkconfig:2040-2045]）。パネルは 720×1280、RGB565、DSI 2 レーン。フレームバッファは PSRAM（0x4800_0000〜、M4 で初期化済み）に置く。ここでは SoC 側（LDO、クロック、DSI ホスト、D-PHY、ブリッジ、DMA）だけを扱う。パネルの初期化コマンド表は別の担当なので、プレースホルダにしておく。
出典の表記: パスは `~/esp-idf/6.0/components/` からの相対パス。`R1/` は `soc/esp32p4/register/hw_ver1/soc/`、`R3/` は `soc/esp32p4/register/hw_ver3/soc/`、`LL/` は `esp_hal_lcd/esp32p4/include/hal/`、`DSI/` は `esp_lcd/dsi/`、`GDMA/` は `esp_hal_dma/esp32p4/include/hal/dw_gdma_ll.h`。`TCC/` と `M5G/` は doc/06 と同じ（`M5G/` = `TCC/components/M5GFX/src/`）。（推測）は推測、（判断）は判断を表す。

## 0. 結論
- **電源**: D-PHY の電源は LDO VO3 で、レジスタは PMU の `EXT_LDO_P0_0P2A`（0x5011_51C0）と `_ANA`（0x5011_51C4）。VO2（PSRAM）と同じ形式で、書き方も psram.rs の `ldo_vo2_on` と同じ。2500mV は較正なしなら dref = 9、mul = 6（1.0V × 2.5）。ただし VO3 には eFuse の補正係数（K、Vos、C）があり、IDF はそれを使って探索する（§1）。
- **クロック**: DPI の画素クロックは **SPLL 系の PLL_F240M を 3 分周して 80MHz**（IDF の既定）。**MPLL は使わない**ので、PSRAM の MPLL 400MHz とはぶつからない。D-PHY の PLL の基準は、**v1 では設定クロックと同じ PLL_F20M（SPLL/24 = 20MHz）に固定**される（v3 は XTAL 40MHz が既定）。1040Mbps なら M = 52、N = 1（§2、§4）。
- **ブリッジは自分では DMA しない**。フレームバッファは **DW-GDMA（AXI の汎用 DMA、0x5008_1000）** が PSRAM から読み、ブリッジの FIFO 窓 0x5010_5000 に書き込む。DMA2D は描画のコピーに使うだけで、表示には要らない。IDF は 1 フレームを LLI 1 個（64bit × 230,400 回）で送り、**フレームごとに DMA 完了割り込みで LLI を張り直して再起動**する（§5）。DW-GDMA の初期化も M6 の範囲に入る。
- **v1 固有の罠**: (1) PLL 基準クロックのセレクタと分周器が v1 にはない（§2）。(2) ブリッジにソフトリセットと VSYNC 割り込みがない。フレームの区切りは DMA の完了で知る（§6）。(3) ブリッジの出力色形式のフィールドがない（入力の `raw_type` に兼ねる）。DSI ホストのレジスタは v1 と v3 で同一。
- **キャッシュ**: DMA は L1/L2 キャッシュを通らない。CPU が描いたら、L1 D → L2 の順に書き戻す（ROM `Cache_WriteBack_Addr`）。
- **リブート**: DW-GDMA はソフトリセットをまたいで走り続ける。IDF はリセットの前に全チャネルを abort している。カーネルも同じことをする（§5.4）。
- 手順の全体は §7。レジスタの書き込みは 100 個程度で、Rust で 400〜600 行（DW-GDMA を含む。推測）。

## 1. 電源（LDO VO3 = 2500mV）

### 1.1 VO3 のレジスタ
- IDF の LDO は「チャネル（VO1〜VO4）」を「ユニット 0〜3」に、ユニットを PMU の `ext_ldo[]` の添字 `{0, 3, 1, 4}` に対応付ける [hal/esp32p4/include/hal/ldo_ll.h:35,157,188,208]。`ext_ldo[]` は PMU + 0x1B8 から 8 バイトずつ並ぶ [R1/pmu_struct.h:731-734,865; R1/pmu_reg.h:4036]。PMU の基底は 0x5011_5000 [R1/reg_base.h:11,135]。

| チャネル | 用途 | ext_ldo | レジスタ（LDO / ANA） | 扱い |
|---|---|---|---|---|
| VO1 | フラッシュ 3.3V | [0] `P0_0P1A` | 0x5011_51B8 / 0x5011_51BC | **触らない** |
| VO2 | PSRAM 1.8V | [3] `P1_0P1A` | 0x5011_51D0 / 0x5011_51D4 | psram.rs で設定済み |
| **VO3** | **D-PHY 2.5V** | **[1] `P0_0P2A`** | **0x5011_51C0 / 0x5011_51C4** | M6 で書く |
| VO4 | 未使用 | [4] `P1_0P2A` | 0x5011_51D8 / 0x5011_51DC | 触らない |

[R1/pmu_reg.h:4036,4104,4137,4205,4339,4407,4440,4508]
- VO1 だけは別の制御もある。rev ≥ v1.0 では `PMU.hp_sys[HP_ACTIVE].regulator0.xpd_0p1a` も VO1 の出力に効く [ldo_ll.h:209-212]。**VO3 の設定は 0x5011_51C0 と 0x5011_51C4 の 2 語だけで完結し、VO1 のレジスタにも `hp_sys` のレジスタにも触らない**。
- フィールド（P0_0P2A と P1_0P1A で配置は同じ）[R1/pmu_reg.h:4138-4233; R1/pmu_struct.h:703-728]
  - LDO 語（0x5011_51C0）: bit7 `FORCE_TIEH_SEL`（1 = ソフトウェア制御）、bit8 `XPD`（1 = 出力オン）、bit[11:9] `TIEH_SEL`（0 = tieh）、bit12/13 `TIEH_POS_EN/NEG_EN`（遅延。IDF は触らない）、bit14 `TIEH`（1 = 3.3V 直結。0 にする）、bit[22:15] `TARGET1`、bit[30:23] `TARGET0`、bit31。
  - ANA 語（0x5011_51C4）: bit[25:23] `MUL`、bit26 `EN_VDET`（リップル抑制）、bit27 `EN_CUR_LIM`、bit[31:28] `DREF`（既定 10）。
- IDF の書き方 `esp_ldo_acquire_channel()`: `ldo_ll_adjust_voltage`（tieh = 0、dref、mul）→ `ldo_ll_set_owner`（force_tieh_sel = 1、tieh_sel = 0）→ `en_vdet = 1` → `xpd = 1` [esp_hw_support/ldo/esp_ldo_regulator.c:86-98; ldo_ll.h:155-214,277-281]。psram.rs の `ldo_vo2_on` と同じ順なので、関数をチャネルについて一般化すればよい（判断）。M5GFX は `esp_ldo_acquire_channel(chan_id=3, 2500mV)` を DSI バスより先に呼ぶ [M5G/lgfx/v1/platforms/esp32p4/Bus_DSI.cpp:40-51; M5G/M5GFX.cpp:2243-2244]。

### 1.2 電圧の符号化と eFuse 補正
- 較正なしの式: Vref = dref < 9 ? 0.5 + 0.05·dref : 1.0 + 0.1·(dref − 9)、Vout = Vref × (1 + 0.25·mul) [ldo_ll.h:60-61]。2500mV にちょうど合うのは **dref = 9、mul = 6** だけ（dref ≤ 8 では最大 0.9 × 2.75 = 2.475V）。
- **eFuse の補正（VO3 と VO4 だけ）**: `efuse_hal_blk_version() ≥ 1` のとき、Vout = (Vref·K + Vos)·(1 + 0.25·mul·C) の K、Vos、C を eFuse から読み、dref 16 × mul 8 の全組を探索して最も近いものを選ぶ [ldo_ll.h:70-122]。
  - 固定小数点の式: `vref_20 = dref<9 ? 10+dref : 20+2(dref-9)`、`Vout_mV = (vref_20·K + 20·Vos)·(4000 + mul·C) / 80000`。K = efuse_k の bit7 が符号（`±(k&0x7F) + 975`）、Vos は bit5 が符号（`±(v&0x1F) − 3`）、C は bit5 が符号（`±(c&0x1F) + 990`）。値が 0 なら K = 1000、Vos = 0、C = 1000 [ldo_ll.h:76-101]。
  - VO3 の係数は `EFUSE_RD_MAC_SYS_3`（0x5012_D050）の bit[13:6] K、bit[19:14] Vos、bit[25:20] C [R1/efuse_struct.h:639-650; R1/efuse_reg.h:845; reg_base.h:12,159]。
  - ブロック版は `blk_version_major × 100 + minor` [hal/efuse_hal.c:28-31]。Tab5 は v0.3 なので 3 ≥ 1 で、**補正の探索を通る**（doc/research/m4 §1.2 の TCC/logs/boot.log:16 から。推測）。
  - VO1/VO2 の 1.8V のような「dref/mul そのものの eFuse 値」は VO3 にはない [ldo_ll.h:124-138]。
- **カーネルでは探索を移植する**（ループ 128 回）。結果の電圧が 2.4〜2.6V の外なら書かない（psram.rs の 1.70〜1.95V の判定と同じ考え方。判断）。最初に P0_0P2A と _ANA を読んでログに出し、bootloader が VO3 を有効にしているかも見る。

## 2. クロックとリセット（HP_SYS_CLKRST = 0x500E_6000）

| レジスタ | アドレス | フィールド | 設定 | 出典 |
|---|---|---|---|---|
| SOC_CLK_CTRL0 | 0x500E_6014 | bit13 `GDMA_CPU_CLK_EN` | 1（DW-GDMA） | R1/hp_sys_clkrst_reg.h:147,239; GDMA:90-95 |
| SOC_CLK_CTRL1 | 0x500E_6018 | bit12 `DSI_SYS_CLK_EN`（ホストとブリッジの APB）、bit5 `GDMA_SYS_CLK_EN` | 両方 1 | hp_sys_clkrst_reg.h:376,412,461; LL/mipi_dsi_ll.h:33-37 |
| REF_CLK_CTRL0 | 0x500E_6024 | bit[23:16] `REF_240M_CLK_DIV_NUM`（既定 1 → 480/2） | 既定のまま | hp_sys_clkrst_reg.h:867,882 |
| REF_CLK_CTRL1 | 0x500E_6028 | bit[23:16] `REF_20M_CLK_DIV_NUM`（既定 23 → 480/24）、bit30 `REF_240M_CLK_EN`（既定 1） | 確認だけ | hp_sys_clkrst_reg.h:900,915,964 |
| REF_CLK_CTRL2 | 0x500E_602C | bit8 `REF_20M_CLK_EN`（既定 1） | 確認だけ | hp_sys_clkrst_reg.h:982,1039 |
| PERI_CLK_CTRL02 | 0x500E_6038 | bit[31:30] `MIPI_DSI_DPHY_CLK_SRC_SEL`（0 = PLL_F20M、1 = RC_FAST、2 = PLL_F25M） | 0 | hp_sys_clkrst_reg.h:1235,1313-1319; LL/mipi_dsi_ll.h:162-178 |
| PERI_CLK_CTRL03 | 0x500E_603C | bit0 `DPHY_CFG_CLK_EN`、bit1 `DPHY_PLL_REFCLK_EN`、bit[6:5] `DPICLK_SRC_SEL`（0 = XTAL、1 = F240M、2 = F160M、3 = APLL）、bit7 `DPICLK_EN`、bit[15:8] `DPICLK_DIV_NUM`（= 分周 − 1） | bit0 = 1、bit1 = 1、SRC = 1、DIV = 2、EN = 1 | hp_sys_clkrst_reg.h:1324-1373; R1/hp_sys_clkrst_struct.h:868-904; LL/mipi_dsi_ll.h:71-128,143-147,193-197 |
| HP_RST_EN0 | 0x500E_60C0 | bit21 `RST_EN_GDMA`、bit26 `RST_EN_DSI_BRG` | 1 → 0 のパルス | hp_sys_clkrst_reg.h:3064,3212,3247-3253; LL/mipi_dsi_ll.h:51-56 |

- PERI_CLK_CTRL03 の bit[4:2] は CSI のフィールドなので、読み書きはビット単位で行う（RMW）。HP_RST_EN0 の bit22〜25 は MSPI（bit22/24 はフラッシュ側）なので触らない（doc/research/m4 §2.3）。
- **DPI の画素クロック**: IDF の既定ソースは **PLL_F240M**（SPLL 480MHz の 2 分周）[soc/esp32p4/include/soc/clk_tree_defs.h:46,170,430-440]。分周は `src_mhz / 目標` の切り捨て [esp_hal_lcd/mipi_dsi_hal.c:251-256; DSI/esp_lcd_panel_dpi.c:241-258]。80MHz → 分周 3（ちょうど 80MHz）。60MHz → 4、70MHz → 3（80MHz になる）。F160M でも 80MHz（分周 2）を作れる。**どちらも SPLL 系で MPLL（PSRAM の 400MHz）とは無関係**。SPLL はフラッシュの MSPI が使っているので、bootloader の時点で動いている（doc/research/m4 §1.1）。
- **D-PHY の設定クロック**: 既定は PLL_F20M [clk_tree_defs.h:443-451]。IDF は必ずこれを使う [DSI/esp_lcd_mipi_dsi_bus.c:50-58]。
- **D-PHY の PLL 基準クロック**:
  - v3（`CHIP_SUPPORT_MIN_REV ≥ 300`）では PERI_CLK_CTRL03 の bit[18:16] でソース（XTAL/APLL/CPLL/SPLL/MPLL）、bit[26:19] で分周を選べる。既定は XTAL [R3/hp_sys_clkrst_struct.h:872-916; LL/mipi_dsi_ll.h:210-266; clk_tree_defs.h:464-469]。
  - **v1 ではこの 2 つのフィールドがない**。基準クロックは設定クロックと同じセレクタ（PERI_CLK_CTRL02 bit[31:30]）で決まり、分周器もない（`set_phy_pll_ref_clock_div` は何もしない）[LL/mipi_dsi_ll.h:268-308; R1/hp_sys_clkrst_struct.h:868-904]。IDF も v1 では既定を `DEFAULT_LEGACY` = PLL_F20M にする [DSI/esp_lcd_mipi_dsi_bus.c:39-47; clk_tree_defs.h:461]。M5GFX も `MIPI_DSI_PHY_CLK_SRC_DEFAULT`（= `DEFAULT_LEGACY`）を渡す [M5G/lgfx/v1/platforms/esp32p4/Bus_DSI.cpp:37; clk_tree_defs.h:478]。**つまり v1 の基準は 20MHz**。
- PLL_F25M は MPLL 系（MPLL/20）[clk_tree_defs.h:45,165; hp_sys_clkrst_reg.h:875（REF_25M_CLK_DIV_NUM 既定 19）]。カーネルは MPLL を 400MHz にしているので、F25M は 20MHz になる。D-PHY のソースに F25M を選ばない限り関係ない（推測）。
- ブリッジ側にもクロックのゲートがある: `DSI_BRG_HOST_CTRL`（0x500A_0880）bit0 `DSI_CFG_REF_CLK_EN`（ホストの refclk と cfg_clk、既定 1）[R1/mipi_dsi_bridge_reg.h:541-548]。IDF は電源投入時の `esp_perip_clk_init` でこれを 0 にする [esp_system/port/soc/esp32p4/clk.c:242-246,294] が、そのあと戻す処理は見当たらない。DSI の APB クロックが切れている間の書き込みなので効いていないか、DSI_BRG のリセット（下記）で既定値 1 に戻ると思われる（推測）。**カーネルではリセット後に読んで 1 であることを確かめる**（判断）。
- DSI ホストだけのリセットビットは HP_SYS_CLKRST に見当たらない（`RST_EN_DSI_BRG` だけ）。IDF もこれ 1 つで済ませている [LL/mipi_dsi_ll.h:51-56]。

## 3. DSI ホスト（Synopsys DesignWare、0x500A_0000）

### 3.1 レジスタ（使うものだけ）
基底は 0x500A_0000 [soc/esp32p4/ld/esp32p4.peripherals.ld:102; R1/reg_base.h:63]。v1 と v3 の `mipi_dsi_host_reg.h` / `_struct.h` は著作権表示の年だけが違う（`diff` で確認）。

| オフセット | 名前 | フィールド | 出典（R1/mipi_dsi_host_reg.h） |
|---|---|---|---|
| +0x04 | PWR_UP | bit0 `SHUTDOWNZ`（1 = 動作） | 29 |
| +0x08 | CLKMGR_CFG | bit[7:0] `TX_ESC_CLK_DIVISION`、bit[15:8] `TO_CLK_DIVISION` | 41 |
| +0x0C | DPI_VCID | bit[1:0] | 60 |
| +0x10 | DPI_COLOR_CODING | bit[3:0]（0 = 16bit config1 = RGB565）、bit8 loosely18 | 72 |
| +0x14 | DPI_CFG_POL | bit0 DE、bit1 VSYNC、bit2 HSYNC、bit3 SHUTD、bit4 COLORM の active-low | 91 |
| +0x2C | PCKHDL_CFG | bit0 `EOTP_TX_EN`、bit1 `EOTP_RX_EN`、bit2 `BTA_EN`、bit3 `ECC_RX_EN`、bit4 `CRC_RX_EN`、bit5 `EOTP_TX_LP_EN` | 219 |
| +0x30 | GEN_VCID | bit[1:0] `GEN_VCID_RX` | 266 |
| +0x34 | MODE_CFG | bit0 `CMD_VIDEO_MODE`（1 = コマンド、既定 1） | 292 |
| +0x38 | VID_MODE_CFG | bit[1:0] 種別（0 = non-burst パルス、1 = non-burst イベント、2 = burst）、bit8〜13 `LP_VSA/VBP/VFP/VACT/HBP/HFP_EN`、bit14 `FRAME_BTA_ACK_EN`、bit15 `LP_CMD_EN`、bit16 `VPG_EN`（テストパターン） | 304 |
| +0x3C | VID_PKT_SIZE | bit[13:0] 1 パケットの画素数 | 393 |
| +0x40 / +0x44 | VID_NUM_CHUNKS / VID_NULL_SIZE | 0 にする | 405,417 |
| +0x48 / +0x4C / +0x50 | VID_HSA_TIME / HBP_TIME / HLINE_TIME | bit[11:0] / [11:0] / [14:0]。**単位はレーンのバイトクロック** | 429-465 |
| +0x54〜+0x60 | VID_VSA / VBP / VFP / VACTIVE_LINES | 行数。bit[9:0]（VACT は [13:0]） | 465-512 |
| +0x68 | CMD_MODE_CFG | bit0 `TEAR_FX_EN`、bit1 `ACK_RQST_EN`、bit8〜14 汎用 SW0/1/2・SR0/1/2・LW、bit16〜19 DCS SW0/SW1/SR0/LW、bit24 `MAX_RD_PKT_SIZE`（各 1 = LP で送る） | 525 |
| +0x6C | GEN_HDR | bit[5:0] DT、bit[7:6] VC、bit[15:8] WC 下位（短パケットは data0）、bit[23:16] WC 上位（data1） | 628 |
| +0x70 | GEN_PLD_DATA | 書くと送信ペイロード FIFO、読むと受信 FIFO（バイト 0 が bit[7:0]） | 661 |
| +0x74 | CMD_PKT_STATUS | bit0 `GEN_CMD_EMPTY`、bit1 `GEN_CMD_FULL`、bit2 `GEN_PLD_W_EMPTY`、bit3 `_W_FULL`、bit4 `GEN_PLD_R_EMPTY`、bit5 `_R_FULL`、bit6 `GEN_RD_CMD_BUSY` | 694 |
| +0x78〜+0x8C | TO_CNT_CFG、HS_RD/LP_RD/HS_WR/LP_WR/BTA_TO_CNT | タイムアウト（0 = 無効） | 776-843 |
| +0x94 | LPCLK_CTRL | bit0 `PHY_TXREQUESTCLKHS`、bit1 `AUTO_CLKLANE_CTRL` | 895 |
| +0x98 | PHY_TMR_LPCLK_CFG | bit[9:0] クロックレーン LP→HS、bit[25:16] HS→LP | 914 |
| +0x9C | PHY_TMR_CFG | bit[9:0] データ LP→HS、bit[25:16] HS→LP | 933 |
| +0xA0 | PHY_RSTZ | bit0 `PHY_SHUTDOWNZ`、bit1 `PHY_RSTZ`、bit2 `PHY_ENABLECLK`、bit3 `PHY_FORCEPLL` | 952 |
| +0xA4 | PHY_IF_CFG | bit[1:0] `N_LANES`（= レーン数 − 1）、bit[15:8] `PHY_STOP_WAIT_TIME` | 985 |
| +0xB0 | PHY_STATUS | bit0 `PHY_LOCK`、bit2 クロックレーン stop、bit4 レーン 0 stop、bit7 レーン 1 stop | 1049 |
| +0xB4 / +0xB8 | PHY_TST_CTRL0 / 1 | CTRL0: bit0 `TESTCLR`（既定 1）、bit1 `TESTCLK`。CTRL1: bit[7:0] `TESTDIN`、bit[15:8] `TESTDOUT`、bit16 `TESTEN` | 1117,1136 |
| +0xBC / +0xC0 | INT_ST0 / INT_ST1 | ST0: bit0〜15 相手の ACK のエラー、bit16〜20 D-PHY のエラー。ST1: bit0 HS TX タイムアウト、bit1 LP RX タイムアウト、bit2〜6 ECC/CRC/サイズ/EoTp、bit7 DPI ペイロード書き込み、bit8〜12 汎用 FIFO のエラー、bit19 `DPI_BUFF_PLD_UNDER` | 1162,1314 |
| +0xF4 | PHY_TMR_RD_CFG | bit[14:0] `MAX_RD_TIME` | 1965 |
| +0x100 | VID_SHADOW_CTRL | bit0 `VID_SHADOW_EN`（既定 0 = 書いた値がすぐ効く） | 1977 |

- 「シャドウ」: ホストの映像レジスタには `*_ACT`（+0x10C〜）の読み出し用の写しがあり、`VID_SHADOW_EN` を立てたときだけ二重化される [R1/mipi_dsi_host_reg.h:1977-2274]。IDF は使わない（既定の 0 のまま）。

### 3.2 IDF の初期化（コマンドモードまで）
`esp_lcd_new_dsi_bus` → `mipi_dsi_hal_init` → `mipi_dsi_hal_configure_phy_pll` の順 [DSI/esp_lcd_mipi_dsi_bus.c:33-125; esp_hal_lcd/mipi_dsi_hal.c:17-32]。
1. `PHY_IF_CFG.N_LANES = 1`（2 レーン）→ `PWR_UP = 1` → `PHY_RSTZ.SHUTDOWNZ = 1` → `RSTZ` を 0 → 1 → `ENABLECLK = 1` → `FORCEPLL = 1` [mipi_dsi_hal.c:22-29; LL/mipi_dsi_phy_ll.h:29-67,106-109]。ブリッジのリセット（`mipi_dsi_brg_ll_reset`）は v1 では空 [LL/mipi_dsi_brg_ll.h:489-492]。
2. D-PHY の PLL を設定（§4）→ `PHY_LOCK` を待つ → 使うレーンの stop 状態（bit2、4、7）を待つ [esp_lcd_mipi_dsi_bus.c:93-101; mipi_dsi_phy_ll.h:75-100]。
3. `MODE_CFG = 1`（コマンドモード）、`LPCLK_CTRL = 0`（クロックレーンは LP のまま）[esp_lcd_mipi_dsi_bus.c:103-106; LL/mipi_dsi_host_ll.h:131-163]。
4. 切り替え時間 `set_switch_time(50, 104, 46, 128)`: データ HS→LP 50、LP→HS 104、クロック HS→LP 46、LP→HS 128（バイトクロック数）→ `PHY_TMR_CFG = 0x0032_0068`、`PHY_TMR_LPCLK_CFG = 0x002E_0080` [esp_lcd_mipi_dsi_bus.c:107-108; mipi_dsi_phy_ll.h:234-240]。
5. `PCKHDL_CFG`: CRC 受信、ECC 受信、HS の EoTp 送信を有効 → 0x19 [esp_lcd_mipi_dsi_bus.c:110-114; host_ll.h:566-591]。
6. `CLKMGR_CFG`: タイムアウト用 = Mbps/8/10、エスケープ用 = Mbps/8/18（TxClkEsc は 2〜20MHz であること）[esp_lcd_mipi_dsi_bus.c:10-12,116-119; host_ll.h:83-99]。1040Mbps なら 13 と 7（エスケープ 130/7 = 18.6MHz）→ `0x0D07`。
7. タイムアウトはすべて 0（無効）、`MAX_RD_TIME = 6000`、`PHY_STOP_WAIT_TIME = 0x3F` [esp_lcd_mipi_dsi_bus.c:120-125]。
8. DBI（コマンド）I/O の作成時: `TEAR_FX_EN = 0`、`ACK_RQST_EN = 1`、汎用と DCS の全種類と最大受信サイズを LP で送る → `CMD_MODE_CFG = 0x010F_7F02` [DSI/esp_lcd_panel_io_dbi.c:24-51; host_ll.h:410-545]。

### 3.3 コマンドの送り方
- **DCS の書き込み** `mipi_dsi_hal_host_gen_write_dcs_command` [mipi_dsi_hal.c:110-165]（ペイロード = コマンド 1 バイト + 引数）
  - 1 バイト（引数なし）: DT 0x05、`GEN_HDR = (cmd << 8) | (vc << 6) | 0x05`。
  - 2 バイト（引数 1 個）: DT 0x15、`GEN_HDR = (p0 << 16) | (cmd << 8) | 0x15`。
  - 3 バイト以上: リトルエンディアンの 32bit 語にまとめて `GEN_PLD_DATA` に順に書く（先頭語は `cmd | p0<<8 | p1<<16 | p2<<24`）。`GEN_PLD_W_FULL` が 0 になるのを待ってから書く。最後に `GEN_HDR = (wc_hi << 16) | (wc_lo << 8) | 0x39`（wc = 1 + 引数の数）。ヘッダの前に `GEN_CMD_FULL` が 0 であることを待つ。
  - DT の値 [esp_hal_lcd/include/hal/mipi_dsi_types.h:31-44]: 汎用短 0x03/0x13/0x23、汎用読み 0x04/0x14/0x24、DCS 短 0x05/0x15、DCS 読み 0x06、最大受信サイズ 0x37、汎用長 0x29、DCS 長 0x39。汎用長は `gen_write_long_packet` [mipi_dsi_hal.c:175-198]。
- **完了待ち**: IDF の `tx_param` はヘッダを書いたら戻り、送信の完了は待たない [DSI/esp_lcd_panel_io_dbi.c:69-78]。カーネルでは、パネルの待ち時間（スリープ解除後の 120ms など）を正しく数えるため、**`CMD_PKT_STATUS` の bit0（GEN_CMD_EMPTY）と bit2（GEN_PLD_W_EMPTY）が両方 1 になるまで待ってから** 次へ進む（判断）。
- **DCS の読み出し**（パネル ID の 0xF4、0x04、0xDA〜0xDC など）`mipi_dsi_hal_host_gen_read_dcs_command` [mipi_dsi_hal.c:200-232]
  1. 最大受信サイズ: `GEN_HDR = (n >> 8) << 16 | (n & 0xFF) << 8 | (vc << 6) | 0x37`。
  2. `MODE_CFG = 1`、`PCKHDL_CFG.BTA_EN = 1`、`GEN_VCID.RX = vc`。
  3. `GEN_HDR = (cmd << 8) | (vc << 6) | 0x06`。
  4. `GEN_RD_CMD_BUSY`（bit6）が 0 になるまで待つ → `GEN_PLD_R_EMPTY`（bit4）が 0 になるまで待つ → 空になるまで `GEN_PLD_DATA` を読み、下位バイトから詰める。
  - IDF の待ちには上限がない。パネルが応答しないと止まる。**カーネルでは SYSTIMER で上限（例 50ms）を付け、`INT_ST1` の bit1（LP RX タイムアウト）や bit8〜12 を一緒にログに出す**（判断）。
  - M5GFX は DSI バスを作ってから 80ms 待ち、0xF4 を 2 バイト読む（`71 23` なら ST7123）。だめなら 0xFF に `98 81 01` を書いてページ 1 の 0x00〜0x02 を読む（ILI9881C）[M5G/M5GFX.cpp:2246-2270]。
- `BTA_EN` は IDF では読み出しのときに初めて立つ。`ACK_RQST_EN` と `FRAME_BTA_ACK_EN` は BTA を前提とする機能なので、M5GFX のように先に ID を読む流れでは結果として立っている（推測）。**カーネルでは初期化の時点で `BTA_EN` を立てておく**か、ACK を使わない。ID の読み出しを先にする流れなら差はない（判断）。

## 4. D-PHY

### 4.1 テストコード・インタフェース
`mipi_dsi_hal_phy_write_register(addr, val)` [mipi_dsi_hal.c:94-108; LL/mipi_dsi_phy_ll.h:182-207]:
```
TST_CTRL0 = 0                    // TESTCLR = 0、TESTCLK = 0
TST_CTRL1 = 0x1_0000 | addr      // TESTEN = 1、アドレス
TST_CTRL0 = 0b10; TST_CTRL0 = 0  // TESTCLK の立ち下がりでアドレスが入る
TST_CTRL1 = val                  // TESTEN = 0、データ
TST_CTRL0 = 0b10; TST_CTRL0 = 0  // 立ち上がりでデータが入る
```

### 4.2 PLL の設定（約 1000Mbps）
- 式: f_vco = f_ref × M / N、5MHz ≤ f_ref/N ≤ 40MHz、M は偶数。N を小さい順に試し、誤差が最小（0 なら即決）の組を選ぶ [mipi_dsi_hal.c:43-70]。レーン速度の範囲は 80〜1500Mbps [mipi_dsi_phy_ll.h:16-17]。
- `hsfreqrange` は表から引く [esp_hal_lcd/esp32p4/mipi_dsi_periph.c:10-50]。950〜999 → 0x1A、1000〜1049 → 0x2A、1050〜1099 → 0x3A。
- 書き込み [mipi_dsi_hal.c:82-87]: `0x44 ← hsfreqrange << 1`、`0x19 ← 0x30`（0x17/0x18 の N/M を使う）、`0x17 ← N − 1`、`0x18 ← (M − 1) & 0x1F`、`0x18 ← 0x80 | ((M − 1) >> 5 & 0x0F)`。
- **基準 20MHz（v1）での値**

| レーン速度 | N | M | 0x44 | 0x17 | 0x18（1 回目 / 2 回目） | 由来 |
|---|---|---|---|---|---|---|
| 1040Mbps | 1 | 52 | 0x54 | 0x00 | 0x13 / 0x81 | M5GFX [M5G/M5GFX.cpp:2242] |
| 1000Mbps | 1 | 50 | 0x54 | 0x00 | 0x11 / 0x81 | Tab5 BSP の ILI9881C（doc/06 §1.4） |
| 960Mbps（965 の近似） | 1 | 48 | 0x34 | 0x00 | 0x0F / 0x81 | BSP の ST712x は 965（20MHz では作れない。推測） |

- IDF は実際の速度 `ref × M / N` を `hal->lane_bit_rate_mbps` に入れ直し、水平タイミングの換算に使う [mipi_dsi_hal.c:88-89,234-243]。
- 順序の注意: IDF は `RSTZ`、`ENABLECLK`、`FORCEPLL` を立てた **後に** テストコードで PLL を書き、それからロックを待つ（§3.2）。DesignWare の一般的な手順（シャットダウン中に書く）とは違うが、この SoC で動いている IDF の順をそのまま写す（判断）。

### 4.3 待ちと LP/HS の切り替え
- `PHY_STATUS.PHY_LOCK`（bit0）→ `(PHY_STATUS & 0x94) == 0x94`（クロックレーンとレーン 0/1 の stop）[mipi_dsi_phy_ll.h:75-100]。IDF は 1ms 間隔で無期限に待つ。カーネルは上限を付ける（判断）。
- クロックレーン: コマンドモードの間は `LPCLK_CTRL = 0`（LP）。映像を流し始めるときに `AUTO_CLKLANE_CTRL = 1`、`TXREQUESTCLKHS = 1`（= 0x3）[host_ll.h:131-158; DSI/esp_lcd_panel_dpi.c:451-454]。

## 5. ブリッジと DMA（メモリから DSI ホストへ）

### 5.1 しくみ
- ブリッジ（0x500A_0800）は、DPI 側のタイミング（画素クロック単位）で画素を DSI ホストの DPI 入力に渡す。画素は **ブリッジ内の FIFO（窓 `MIPI_DSI_BRG_MEM_BASE` = 0x5010_5000）に DMA で書き込まれる**[R1/reg_base.h:64,169; peripherals.ld:101]。
- IDF の DPI パネルは **DW-GDMA** のチャネルを 1 つ取り、送り元 = メモリ（PSRAM のフレームバッファ）、送り先 = DSI（ハードウェア・ハンドシェイク）、フロー制御 = DMA 自身とする [DSI/esp_lcd_panel_dpi.c:124-165]。ブリッジ側も「フロー制御は DMA」に設定する [esp_lcd_panel_dpi.c:322-324; LL/mipi_dsi_brg_ll.h:259-282]。
- 1 回の転送で 1 フレーム全体を送る（`DPI_PANEL_MIN_DMA_NODES_PER_LINK = 1`）。送り元は 64bit 幅・増加・バースト 512 項目・AXI 長 16、送り先は 64bit 幅・固定アドレス・256 項目・長 16。転送数 = fb_size × 8 / 64 [DSI/mipi_dsi_priv.h:36-37; esp_lcd_panel_dpi.c:417-442]。720×1280×2 = 1,843,200 バイト → **230,400 回**（BLOCK_TS は 22 ビットで入る [R1/dw_gdma_reg.h:921]）。
- **連続表示**: 単方向リスト（LLI 1 個、次 = 0、last = 1）なので、フレームごとに DMA が止まる。完了割り込み `mipi_dsi_dma_trans_done_cb` で LLI に valid/last を付け直し、LLP を設定し、チャネルを再度有効にする [esp_lcd_panel_dpi.c:72-97]。v1 には VSYNC 割り込みがないので、この割り込みを「フレームの終わり」とみなす [同 88-95]。
- DMA2D（`use_dma2d`）は `draw_bitmap` でユーザのバッファをフレームバッファにコピーするときだけ使う [esp_lcd_panel_dpi.c:230-239]。M5GFX はフレームバッファに直接描くので、DMA2D は実質使っていない（推測）。

### 5.2 ブリッジのレジスタ（v1）
| オフセット | 名前 | フィールド | 設定（720×1280、RGB565） | 出典 |
|---|---|---|---|---|
| +0x04 | EN | bit0 `DSI_EN` | 最後に 1 | R1/mipi_dsi_bridge_reg.h:29; brg_ll.h:52 |
| +0x08 | DMA_REQ_CFG | bit[11:0] `DMA_BURST_LEN`（64bit 単位。ブリッジがフロー制御のときだけ有効） | 256 | :41-46; dpi.c:325 |
| +0x0C | RAW_NUM_CFG | bit[21:0] `RAW_NUM_TOTAL`（1 フレームの 64bit 数）、bit22 `UNALIGN_64BIT_EN`、bit31 `RAW_NUM_TOTAL_SET`（内部カウンタに反映） | 230,400 \| bit31 = 0x8003_8400 | :54-70; brg_ll.h:131-136 |
| +0x18 | PIXEL_TYPE | bit[3:0] `RAW_TYPE`（0 = RGB888、1 = RGB666、2 = RGB565）、bit[5:4] `DPI_CONFIG`、bit6 `DATA_IN_TYPE`（0 = RGB） | 0x2 | :123-138; brg_ll.h:439-487 |
| +0x1C | DMA_BLOCK_INTERVAL | bit28 `RAW_NUM_TOTAL_AUTO_RELOAD`（既定 1） | 既定のまま | :149-172 |
| +0x30 | DPI_V_CFG0 | bit[11:0] `VTOTAL`、bit[27:16] `VDISP` | §5.3 | :234-243; brg_ll.h:185-191 |
| +0x34 | DPI_V_CFG1 | bit[11:0] `VBANK`（= VBP）、bit[27:16] `VSYNC`（= VSA） | §5.3 | :254-263 |
| +0x38 | DPI_H_CFG0 | bit[11:0] `HTOTAL`、bit[27:16] `HDISP` | §5.3 | :274-283; brg_ll.h:202-208 |
| +0x3C | DPI_H_CFG1 | bit[11:0] `HBANK`（= HBP）、bit[27:16] `HSYNC`（= HSA） | §5.3 | :294-303 |
| +0x40 | DPI_MISC_CONFIG | bit0 `DPI_EN`、bit[15:4] `FIFO_UNDERRUN_DISCARD_VCNT` | 720 << 4、最後に bit0 | :314-322; brg_ll.h:216-230 |
| +0x44 | DPI_CONFIG_UPDATE | bit0（WT）: DPI 設定を反映する | 設定のたびに 1 | :334-335; brg_ll.h:237-240 |
| +0x50〜+0x5C | INT_ENA / CLR / RAW / ST | bit0 `UNDERRUN`（v1 はこれだけ） | ENA bit0 = 1 | :346-384 |
| +0x6C | DMA_FRAME_INTERVAL | bit28 `DMA_MULTIBLK_EN` | 0（ブロック 1 個） | :452-468; brg_ll.h:272-276 |
| +0x80 | HOST_CTRL | bit0 `DSI_CFG_REF_CLK_EN`（既定 1） | 1 を確認 | :541-548 |
| +0x88 | DMA_FLOW_CTRL | bit0 `DSI_DMA_FLOW_CONTROLLER`（0 = DMA、既定 1）、bit[7:4] `DMA_FLOW_MULTIBLK_NUM` | 0x10 | :574-583 |
| +0x8C | RAW_BUF_ALMOST_EMPTY_THRD | bit[10:0] | 1024 − 256 = 768 | :595-596; dpi.c:326 |

- **「更新」ビット**: ブリッジの DPI 設定は `DPI_CONFIG_UPDATE` に 1 を書くと反映される。IDF は、ブリッジの設定の最後（`DSI_EN = 1` の後）と、`DPI_EN = 1` の後の 2 回書く [esp_lcd_panel_dpi.c:327-329,456-458]。
- 色: 入力 RGB565 → `RAW_TYPE = 2`、`DATA_IN_TYPE = 0`。出力も RGB565 で、v1 は同じ `RAW_TYPE` に書く（§6）。ホスト側は `DPI_COLOR_CODING = 0`（16bit config1）[host_ll.h:185-200]。
- エラー: `INT_RAW.UNDERRUN` は「外部メモリから十分速く読めなかった」。IDF はログを出すだけで、復旧はしない [esp_lcd_panel_dpi.c:99-112,460-461]。ホスト側では `INT_ST1` bit19 `DPI_BUFF_PLD_UNDER` も見る（推測）。

### 5.3 タイミング（ホストはバイトクロック、ブリッジは画素クロック）
- IDF [mipi_dsi_hal.c:234-249; host_ll.h:377-400; brg_ll.h:185-208]
  - ホスト: 比 r = レーン Mbps / DPI MHz / 8。`HSA = ⌊hsw·r⌋`、`HBP = ⌊hbp·r⌋`、`HLINE = ⌊hact·r⌋ + HSA + HBP + ⌊hfp·r⌋`（各項を先に切り捨ててから足す）。VSA/VBP/VFP/VACT は行数そのまま。`VID_PKT_SIZE = hact`。
  - ブリッジ: `HTOTAL = hsw + hbp + hact + hfp`、`HSYNC = hsw`、`HBANK = hbp`。垂直も同じ形。
  - DPI のモードは **burst（sync pulse 付き）**、LP 遷移を全期間で許可、LP でのコマンド送信を許可、フレームごとの BTA ACK を要求 → `VID_MODE_CFG = 0xFF02`。極性はすべて active-high（`DPI_CFG_POL = 0`）[esp_lcd_panel_dpi.c:280-307]。
- M5GFX の値（DPI 80MHz、1040Mbps、r = 1.625）[M5G/M5GFX.cpp:2271-2299]

| パネル | hsw/hbp/hfp | vsw/vbp/vfp | ホスト HSA/HBP/HLINE | ブリッジ HTOTAL/VTOTAL | フレーム |
|---|---|---|---|---|---|
| ILI9881C | 40/140/40 | 4/20/20 | 65/227/1527 | 940/1324 | 64.3Hz |
| ST7123 | 2/40/40 | 2/8/220 | 3/65/1303 | 802/1510 | 66.1Hz |

- 帯域: DPI 80MHz × 16bit = 1280Mbps に対し、リンクは 2 × 1040 = 2080Mbps なので burst が成り立つ。PSRAM からの読み出しは約 1.84MB × 65 = 120MB/s（推測）。
- ST7123 について M5GFX は「vbp + vsw がずれると表示が縦にずれる」「vfp を減らすとタッチが止まる」と書いている [M5G/M5GFX.cpp:2294-2297]。パネル側の担当に引き継ぐ。

### 5.4 DW-GDMA（0x5008_1000）
基底 [peripherals.ld:106; R1/reg_base.h:42]。チャネル n（0 始まり）のレジスタは 0x100 × (n+1) から [R1/dw_gdma_reg.h:873,2375]。IDF は空いている最初のチャネルを使う（チャネル 0 = 「CH1」。推測）。

| レジスタ | アドレス（CH1） | 中身 | 出典 |
|---|---|---|---|
| CFG0 | 0x5008_1010 | bit0 `DMAC_EN`、bit1 `INT_EN` | dw_gdma_reg.h:41; GDMA:151-160 |
| CHEN0 | 0x5008_1018 | bit0 CH1_EN、bit8 CH1_EN_WE（書き込みの有効化）。有効 = `0x101`、無効 = `0x100` | :60; GDMA:304-311 |
| CHEN1 | 0x5008_101C | bit0 CH1_ABORT（bit8 が WE）。`0x101` を書き、両方 0 に戻るまで待つ | :177; GDMA:341-346 |
| RESET0 | 0x5008_1058 | bit0 `DMAC_RST`（1 を書き、0 に戻るまで待つ） | :802; GDMA:136-140 |
| CH1_CFG0 | 0x5008_1120 | bit[1:0] 送り元、bit[3:2] 送り先のマルチブロック種別（0 連続、1 リロード、2 シャドウ、3 LLI） | :1097; esp_hal_dma/include/hal/dw_gdma_types.h:17-21 |
| CH1_CFG1 | 0x5008_1124 | bit[2:0] `TT_FC`（1 = M→P、DMA が制御）、bit3/4 `HS_SEL_SRC/DST`（0 = HW）、bit[13:12] `DST_PER`（0 = DSI）、bit[19:17] 優先度、bit[26:23]/[30:27] 未完了要求の上限 − 1 | :1130; GDMA:76-85,632-770,797-813 |
| CH1_LLP0 | 0x5008_1128 | bit0 `LMS`（LLI を読むマスタ。1 = メモリ）、bit[31:6] LLI のアドレス >> 6 | :1219; GDMA:823-858 |
| CH1_INTSTATUS_ENABLE0 | 0x5008_1180 | 状態ビットの生成の許可（IDF は全ビット 1） | :1490; dw_gdma.c:292 |
| CH1_INTSTATUS0 / INTCLEAR0 | 0x5008_1188 / 0x5008_1198 | bit1 `DMA_TFR_DONE`、bit5〜13 各種エラー | :1712,2154 |

- マスタ: **マスタ 0 が DSI ブリッジの窓、マスタ 1 が L2MEM・ROM・フラッシュ/PSRAM** [GDMA:23-25,397-430]。送り元（PSRAM）は `SMS = 1`、送り先（0x5010_5000）は `DMS = 0`。
- LLI は 64 バイトで、64 バイト境界に置く [GDMA:29,955-981]。中身（CH1 のレジスタと同じ並び）:

| +off | 内容 | 値 |
|---|---|---|
| 0x00 / 0x04 | SAR 下位 / 上位 | フレームバッファのアドレス（0x48xx_xxxx）/ 0 |
| 0x08 / 0x0C | DAR | 0x5010_5000 / 0 |
| 0x10 | BLOCK_TS | 230,400 − 1 = 0x3_83FF |
| 0x18 / 0x1C | LLP | 0 / 0（単方向、次なし） |
| 0x20 | CTL 下位 | SMS = 1、DMS = 0、SINC = 0（増加）、DINC = 1（固定）、SRC/DST_TR_WIDTH = 3（64bit）、SRC_MSIZE = 8（512）、DST_MSIZE = 7（256）→ **0x001E_1B41** |
| 0x24 | CTL 上位 | ARLEN_EN、ARLEN = 16、AWLEN_EN、AWLEN = 16、LLI_LAST（bit30）、LLI_VALID（bit31）→ **0xC010_8840** |

[dw_gdma_reg.h:933-1015（CTL のビット位置）; GDMA:433-560,1163-1168; esp_lcd_panel_dpi.c:417-442]
- IDF の初期化: クロック 2 ビットを立て、`RST_EN_GDMA` をパルス → `RESET0` → `CFG0 = DMAC_EN | INT_EN` [esp_hw_support/dma/dw_gdma.c:118-125; esp_hal_dma/dw_gdma_hal.c:12-18]。チャネルの設定は dw_gdma.c:262-292、LLI の設定は :395-421、開始は `CHEN0` [:305-323]。
- **LLI はキャッシュ越しに見えない**。IDF は LLI を L2MEM の非キャッシュの別名（+0x4000_0000）から書く [dw_gdma.c:45,395-407; soc/esp32p4/include/soc/ext_mem_defs.h:68]。カーネルも、LLI を L2MEM に置き、非キャッシュの別名で書くか、書いた後に L1 D を書き戻す（判断）。
- **フレームごとの再起動**（ポーリング版の案）: `CH1_INTSTATUS0.DMA_TFR_DONE` を見たら INTCLEAR0 で消し、LLI の CTL 上位に VALID/LAST を書き直し、`LLP0` を書き、`CHEN0 = 0x101`。割り込みにするなら、DW-GDMA の割り込みソースは 24、ブリッジは 86、DSI ホストは 88 [soc/esp32p4/include/soc/interrupts.h:43,108,110（列挙の位置から数えた）]。1 フレームは約 15.5ms で、ブリッジ側は `RAW_NUM_TOTAL_AUTO_RELOAD` で次のフレームを待つので、再起動が垂直帰線の間に間に合えばよい（推測）。
- 別案: CFG0 のマルチブロック種別を両方「リロード」（1）にすると、DW AXI DMAC は毎ブロック SAR/DAR/BLOCK_TS を初期値に戻して続けるので、CPU なしで表示が続くはず（推測。IDF は使っていない）。まず IDF と同じ LLI 方式で動かし、余裕ができたら試す（判断）。
- **ソフトリセットの前に止める**: IDF は `esp_restart_noos` の中で、DW-GDMA のバスクロックが入っていれば全チャネルを abort する（「再起動後のメモリ破壊を防ぐ」）[esp_system/port/soc/esp32p4/system_internal.c:66-70]。カーネルのリブート経路（rollback コマンド、更新後の再起動）にも入れる（判断）。

### 5.5 フレームバッファとキャッシュ
- IDF はフレームバッファを `MALLOC_CAP_SPIRAM | MALLOC_CAP_DMA` で取る（キャッシュ行の境界にそろう）。作った直後と描画の後に、L1 D → L2 の順で書き戻す [esp_lcd_panel_dpi.c:211-224,497-505; hal/esp32p4/include/hal/cache_ll.h:678-695]。
- カーネル: ROM `Cache_WriteBack_Addr(map, addr, size)`（0x4FC0_03F4）を `map = 0x10`（L1 D）→ `0x20`（L2）の順に呼ぶ [esp_rom/esp32p4/ld/esp32p4.rom.ld:190; esp_rom/esp32p4/include/esp32p4/rom/cache.h:230-231,630]。L2 を止めていない時なら呼んでよい（doc/research/m4 §9.1）。
- 置き場所: 0x4800_0000 の窓の中で 4KiB 境界（推測で十分な条件。DMA の幅 64bit と `RAW_NUM_TOTAL` の 64bit 単位を満たす）。1 枚 1.76MiB。M4 のアプリプール（4 × 8MiB）と重ならないように、プールの 1 スロットを削るか、プールを縮める（判断）。
- CPU が非キャッシュの 0x8800_0000 側から書けば書き戻しは要らないが、書き込みが遅い（推測。M4 で非キャッシュ読みは確認済み、doc/research/m4 §8）。

## 6. v1 と v3 の違い（この範囲）
| 項目 | v1（Tab5） | v3 | 出典 |
|---|---|---|---|
| DSI ホストのレジスタ | 同一 | 同一 | `diff R1/mipi_dsi_host_*.h R3/...`（年の表記だけ） |
| D-PHY の PLL 基準 | 設定クロックと共用（F20M/RC_FAST/F25M）。分周なし | 独立したソース選択（XTAL/APLL/CPLL/SPLL/MPLL）と分周。既定 XTAL | LL/mipi_dsi_ll.h:210-310; R1/R3 hp_sys_clkrst_struct.h |
| ブリッジのソフトリセット | なし（`EN` bit1 がない） | `DSI_BRIG_RST` | `diff R1/R3 mipi_dsi_bridge_reg.h`; brg_ll.h:410-413,489-492 |
| ブリッジの出力色形式 | フィールドなし。`RAW_TYPE` に書く | `DPI_TYPE` bit[10:7] | brg_ll.h:390-408,474-487 |
| 入力の YUV/GRAY | YUV422 の一部だけ | RAW_TYPE 9/12 など | brg_ll.h:350-388,439-472 |
| VSYNC 割り込み | なし | INT bit1 | brg_ll.h:19-24 |
| DW-GDMA | 4 チャネル。v1 のヘッダを使う | — | GDMA:20-21 |
- IDF のコードは `HAL_CONFIG(CHIP_SUPPORT_MIN_REV)`（= `CONFIG_ESP_REV_MIN_FULL`）で分岐する [hal/platform_port/include/hal/config.h:22,55]。Tab5 の設定は 100 なので、すべて v1 側が選ばれる。

## 7. カーネルの手順（レジスタ単位）
前提: PSRAM 初期化済み（M4）。パネルのリセット（IO エクスパンダ #1 bit4）と I2C は済ませておく（doc/06 §1.4、M5G/M5GFX.cpp:2195-2222）。各ステップでイベントログを出す（psram.rs と同じ方針）。値は 1040Mbps、DPI 80MHz、ILI9881C のポーチの場合。

| # | 操作 | アドレス | 値・フィールド | 理由 |
|---|---|---|---|---|
| 1 | 読む | 0x5011_51C0 / C4、0x5012_D050 | — | VO3 の現状と eFuse の K/Vos/C をログに出す |
| 2 | 書く | 0x5011_51C0 | bit14 = 0 | tieh を外す（3.3V 直結にしない） |
| 3 | RMW | 0x5011_51C4 | DREF[31:28]、MUL[25:23]（§1.2 の探索結果）、bit26 = 1 | 2500mV、リップル抑制 |
| 4 | RMW | 0x5011_51C0 | bit[11:9] = 0、bit7 = 1、bit8 = 1 | ソフトウェア制御で出力オン。待つ（psram.rs と同じく 100µs。推測） |
| 5 | 確認 | 0x500E_6028 bit30、0x500E_602C bit8 | 1 であること | PLL_F240M と PLL_F20M のゲート |
| 6 | 立てる | 0x500E_6018 | bit12 | DSI の APB クロック |
| 7 | パルス | 0x500E_60C0 | bit26 を 1 → 0 | DSI ブリッジのリセット |
| 8 | 確認 | 0x500A_0880 | bit0 = 1 | ホストの cfg/ref クロック（§2） |
| 9 | RMW | 0x500E_6038 | bit[31:30] = 0 | D-PHY の設定と PLL 基準 = PLL_F20M |
| 10 | RMW | 0x500E_603C | bit0 = 1、bit1 = 1 | D-PHY の設定クロックと PLL 基準クロック |
| 11 | 書く | 0x500A_00A4 | 0x01 | 2 レーン |
| 12 | 書く | 0x500A_0004 | 1 | ホストの電源 |
| 13 | 書く | 0x500A_00A0 | 0x1 → 0x1 → 0x3 → 0x7 → 0xF | PHY: shutdownz、rstz の 0→1、enableclk、forcepll |
| 14 | テスト書き | 0x500A_00B4/B8 | (0x44,0x54)、(0x19,0x30)、(0x17,0x00)、(0x18,0x13)、(0x18,0x81) | PLL N = 1、M = 52、hsfreqrange 0x2A |
| 15 | 待つ | 0x500A_00B0 | bit0 = 1、その後 `& 0x94 == 0x94` | PLL ロックとレーンの stop。上限付き |
| 16 | 書く | 0x500A_0034 / 0x94 | 1 / 0 | コマンドモード、クロックレーン LP |
| 17 | 書く | 0x500A_009C / 0x98 | 0x0032_0068 / 0x002E_0080 | HS/LP の切り替え時間 |
| 18 | 書く | 0x500A_002C | 0x19（読み出しを先にするなら 0x1D） | CRC/ECC 受信、EoTp 送信（BTA） |
| 19 | 書く | 0x500A_0008 | 0x0D07 | タイムアウト 10MHz、エスケープ 18.6MHz |
| 20 | 書く | 0x500A_0078〜0x8C | 0 | タイムアウト無効 |
| 21 | 書く | 0x500A_00F4 / RMW 0x500A_00A4 | 6000 / bit[15:8] = 0x3F | 読み出しの最大時間、stop 待ち |
| 22 | 書く | 0x500A_0068 | 0x010F_7F02 | 全コマンドを LP、ACK 要求 |
| 23 | 待つ | — | 80ms | M5GFX と同じ（M5G/M5GFX.cpp:2249） |
| 24 | DCS 読み | §3.3 | 0xF4 を 2 バイトなど | パネルの判別（パネル側の担当と合わせる） |
| 25 | 立てる | 0x500E_6014 bit13、0x500E_6018 bit5 | — | DW-GDMA のクロック |
| 26 | パルス | 0x500E_60C0 | bit21 を 1 → 0 | DW-GDMA のリセット |
| 27 | 書く・待つ | 0x5008_1058 | 1 → 0 になるまで | DMAC のソフトリセット |
| 28 | 書く | 0x5008_1010 | 0x3 | DMAC の有効化（割り込みも） |
| 29 | 書く | 0x5008_1120 / 0x5008_1124 | 0xF / 0x0A02_0001 | LLI、M→P、DSI、優先度 1、未完了 5/2 |
| 30 | 書く | 0x5008_1180 | 0xFFFF_FFFF | 状態ビットを有効に |
| 31 | RMW | 0x500E_603C | bit[6:5] = 1、bit[15:8] = 2、bit7 = 1 | DPI クロック = 240/3 = 80MHz |
| 32 | 書く | 0x500A_000C / 0x10 / 0x14 | 0 / 0 / 0 | VC 0、RGB565、極性 |
| 33 | 書く | 0x500A_0038 | 0xFF02 | burst、LP 許可、フレーム ACK |
| 34 | 書く | 0x500A_003C / 40 / 44 | 720 / 0 / 0 | パケット = 1 行 |
| 35 | 書く | 0x500A_0048 / 4C / 50 | 65 / 227 / 1527 | 水平（バイトクロック） |
| 36 | 書く | 0x500A_0054 / 58 / 5C / 60 | 4 / 20 / 20 / 1280 | 垂直（行） |
| 37 | 書く | 0x500A_080C | 0x8003_8400 | 1 フレーム = 230,400 × 64bit |
| 38 | 書く | 0x500A_0840 | 720 << 4（0x2D00） | アンダーラン時の破棄数。DPI_EN はまだ 0 |
| 39 | 書く | 0x500A_0818 | 0x2 | RGB565 |
| 40 | 書く | 0x500A_0838 / 3C / 30 / 34 | 0x02D0_03AC / 0x0028_008C / 0x0500_052C / 0x0004_0014 | ブリッジの水平・垂直（画素） |
| 41 | 書く | 0x500A_0888 / RMW 0x500A_086C | 0x10 / bit28 = 0 | フロー制御 = DMA、ブロック 1 個 |
| 42 | 書く | 0x500A_0808 / 0x500A_088C | 256 / 768 | バースト長、ほぼ空の閾値 |
| 43 | 書く | 0x500A_0804、0x500A_0844 | 1、1 | ブリッジ有効、設定の反映 |
| 44 | DCS 書き | §3.3 | **（パネルの初期化表: プレースホルダ）**。MADCTL 0x36 と COLMOD 0x3A = 0x55 は M5GFX が最後に送る [M5G/lgfx/v1/platforms/esp32p4/Panel_DSI.cpp:41-66] | パネル側の担当 |
| 45 | 書く | フレームバッファ | 塗りつぶし → `Cache_WriteBack_Addr` を L1 D、L2 の順 | DMA から見えるようにする |
| 46 | 書く | LLI（L2MEM、64B 境界、非キャッシュ側から） | §5.4 の表 | 1 フレーム分の転送 |
| 47 | 書く | 0x5008_1128 / 0x5008_112C | LLI アドレス \| 1 / 0 | LLI の先頭、LMS = メモリ |
| 48 | 書く | 0x5008_1018 | 0x101 | DMA 開始 |
| 49 | 書く | 0x500A_0034 / 0x94 | 0 / 0x3 | 映像モード、クロックレーン自動 HS |
| 50 | RMW・書く | 0x500A_0840 bit0 = 1、0x500A_0844 = 1 | — | ブリッジの DPI 出力を開始して反映 |
| 51 | 書く | 0x500A_0850 | bit0 = 1 | アンダーランの検出 |
| 52 | 毎フレーム | 0x5008_1188 bit1 → 0x5008_1198、LLI の VALID/LAST、0x5008_1128、0x5008_1018 | — | 再起動（§5.4） |
| 53 | — | GPIO22 | PWM | バックライト（M5G/M5GFX.cpp:2339。別担当） |

- 順序の出典: 1〜22 は `esp_ldo_acquire_channel` → `esp_lcd_new_dsi_bus` → `esp_lcd_new_panel_io_dbi`、25〜43 は `esp_lcd_new_panel_dpi`、45〜51 は `dpi_panel_init`（M5GFX は 44 のパネル初期化を DPI の作成と `esp_lcd_panel_init` の間に行う）[M5G/lgfx/v1/platforms/esp32p4/Panel_DSI.cpp:36-66,110-124; DSI/esp_lcd_panel_dpi.c:167-342,409-464]。
- 確かめ方（判断）: 15 の後に PHY_STATUS、24 で ID、49 の後に `INT_ST0/1`、ブリッジの `INT_RAW`、`FIFO_FLOW_STATUS`（+0x14、bit[13:0] FIFO の深さ）[R1/mipi_dsi_bridge_reg.h:111-112]、DMA の `CH1_STATUS0`（+0x130、完了したブロックの量）を読んでログに出す。パネルの初期化の前でも、ホストの `VPG_EN`（VID_MODE_CFG bit16）でテストパターンを出せる（推測。IDF には `set_pattern_type` がある [host_ll.h:300]）。

## 8. 未確認事項
- **VO3 の eFuse 係数**（K/Vos/C）の実際の値と、探索の結果。bootloader の時点で VO3 が有効かどうか（§7 の 1 で読む）。
- `BTA_EN` を立てないまま `ACK_RQST_EN` と `FRAME_BTA_ACK_EN` を使ったときの動き（§3.3）。
- `DSI_CFG_REF_CLK_EN` が DSI_BRG のリセットで 1 に戻るか（§2）。DSI ホストに独立したリセットがないか。
- D-PHY の PLL をリセット解除後に書く IDF の順で問題ないか（§4.2。IDF の実績はある）。
- LLI 再起動の代わりに DW-GDMA の「リロード」モードで連続転送できるか（§5.4）。
- DW-GDMA から PSRAM への読み出しが APM（doc/02 §5）で許されているか。IDF の設定に特別な処理は見当たらない（推測で既定のまま通る）。
- PSRAM の帯域: 120MB/s の DMA 読み出しと CPU の描画が重なったときにアンダーランしないか。IDF は「AXI-ICM で帯域を調整せよ」とだけ書いている [esp_lcd_panel_dpi.c:108-111]。
- 965Mbps（BSP の ST712x）は 20MHz 基準では作れない。M5GFX の 1040Mbps がどのパネルでも動くか（パネル側の担当と合わせる）。
