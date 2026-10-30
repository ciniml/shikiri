# M6 調査: パネルとタッチの立ち上げ（ボード側。I2C、IO エクスパンダ、パネル、バックライト、タッチ）

対象は Tab5（ESP32-P4 rev v1.0）。M6 で画面とタッチを動かす。SoC の MIPI-DSI ホスト、D-PHY、DPI ブリッジは別担当で、この調査には含めない。ここで扱うのは、DSI より手前と外側（I2C1、IO エクスパンダ、パネルの判別、パネルの初期化コマンドとタイミング、バックライト、タッチ）。doc/06 §1.2〜1.4、§5、§7 の続きで、そこにある表は繰り返さない。
調査日: 2026-10-31。実機には触っていない（書き込み、シリアル、プローブなし）。

出典の表記（doc/06 と同じものは同じ略号）:
- `IDF/` = `~/esp-idf/6.0/components/`。`HW1/` = `IDF/soc/esp32p4/register/hw_ver1/soc/`
- `M5G/` = `~/repos/tab5_claude_client/components/M5GFX/src/`（ローカル版 0.2.20）。`M5G↑` = upstream M5GFX master（0.2.36、commit 7cb26b4、2026-10-30）https://github.com/m5stack/M5GFX/blob/7cb26b44ab31bb41a0a597196dd2c6d868869a21/src/
- `M5U/`、`TCC/`、`SCH` は doc/06 と同じ
- `UD/` = `~/repos/M5Tab5-UserDemo/platforms/tab5/`（M5Stack 公式デモ、commit 611f2d6、2025-09-11。ST7121 には未対応）
- `BSP` = espressif/esp-bsp master（commit 67f41a1、2026-10-30）の `bsp/m5stack_tab5/`（v1.3.2）https://github.com/espressif/esp-bsp/tree/67f41a169401d1f23e827188dd08fb351cb0febe/bsp/m5stack_tab5 。`BSPC` = 同じリポジトリの `components/io_expander/esp_io_expander_pi4ioe5v6408/`
- `IOT` = espressif/esp-iot-solution master の `components/display/lcd/esp_lcd_st7123/esp_lcd_st7123.c`（v1.0.2）
- `DS` = Diodes PI4IOE5V6408 データシート DS40583 Rev 3-5（2022-09）https://www.diodes.com/assets/Datasheets/PI4IOE5V6408.pdf
- （推測）は推測、（判断）は判断を表す。M5G↑、BSP、IOT、DS、SCH はダウンロードして読んだ一時コピーで、リポジトリには入れていない。

## 0. 結論
- **I2C**: 内部バスは **I2C1（ベース 0x500C_5000）** に割り当てる。クロックは HP_SYS_CLKRST の 4 か所（APB クロック、機能クロック、XTAL 選択、分周）とリセット 1 か所で有効になる。信号番号は SCL=70、SDA=71。ピンはオープンドレインにする。外付けのプルアップ（5.1kΩ）がある。転送は、8 本のコマンドレジスタと 32 バイトの FIFO を使って **ポーリングでできる**（§1）。レジスタ配置は v1 と v3 で同じ。
- **PI4IOE5V6408 のレジスタ番号は M5GFX が正しい**。データシートの表 2 で確定した（0x09=Input Default State、0x0D=Pull Select、0x0F=Input Status で読み出し専用）。`TCC/main/wifi_setup.cpp` の名前は間違いで、0x09 に 0xB9 を書き、0x0F への書き込みは無視されている（§2.1）。
- **判別**: 先に **I2C で判別する**。LCD_RST と TP_RST を解除したあと、0x55 の FW バージョン（16 ビットのレジスタ 0x0000）が 1 なら ST7121、3 なら ST7123。0x55 が応答せず、0x14 が ACK を返せば ILI9881C+GT911。DSI での ID 読み出しは確認用にとどめる。ST7121 と ST7123 では lane 速度が違うので、DSI の ID では安全に区別できない（M5G↑ のコメント）（§3）。
- **パネル**: 3 種類とも、初期化テーブル、DPI タイミング、遅延を出典つきで全部書いた（§4）。どのパネルも DPI は **RGB565**（COLMOD 0x55）で、2 lane。ILI9881C のテーブルは 3 つの出典で一致する。ST7121 のテーブルは M5G↑ と BSP で一致する。ST7123 は、本体の表は一致するが、末尾の SLPOUT と DISPON の順番と、MADCTL の扱いが出典ごとに違う。
- **バックライト**: GPIO22 は昇圧 LED ドライバ ME2212 の EN ピンにつながっている（SCH）。**GPIO を High にするだけで点く**（推測）。別の昇圧イネーブルはない。PWM で調光するなら LEDC を使う。P4 の LEDC は、duty を反映するのにガンマ RAM の設定まで必要なので、M6 では GPIO の High/Low だけにする（判断）（§5）。
- **タッチ**: GT911 は 0x814E の bit7 を見て点を読み、0x814E に 0 を書いて消す。ST712x は 0x0010 の bit3 を見て、0x0014 から 7 バイトずつ読む。どちらもレジスタアドレスは 16 ビットのビッグエンディアン。INT（GPIO23）は 5.1kΩ でプルアップされている。M5GFX と BSP はどちらもポーリングで読んでいる。最初はポーリングにする（判断）（§6）。
- 推奨の順番と待ち時間は §7。未確認事項は §8。

## 1. I2C1（SDA=GPIO31、SCL=GPIO32）

### 1.1 インスタンスとクロック
| 項目 | 値 | 根拠 |
|---|---|---|
| 使うコントローラ | HP I2C の 1 番（I2C1）。M5Unified も M5GFX も I2C_NUM_1 | doc/06 §1.2; `M5G/M5GFX.cpp:2327`（`cfg.i2c_port = 1`） |
| ベースアドレス | **0x500C_5000**（`DR_REG_HPPERIPH1_BASE` 0x500C_0000 + 0x5000）。I2C0 は 0x500C_4000 | HW1/reg_base.h:10,85-86 |
| APB クロック | `SOC_CLK_CTRL2`（0x500E_601C）bit13 `I2C1_APB_CLK_EN`。リセット値 0 | HW1/hp_sys_clkrst_reg.h:605,697-703; IDF/esp_hal_i2c/esp32p4/include/hal/i2c_ll.h:138-145 |
| 機能クロック | `PERI_CLK_CTRL10`（0x500E_6040）bit27 `I2C1_CLK_EN`、bit26 `I2C1_CLK_SRC_SEL`（0 = XTAL 40MHz、1 = RC_FAST） | hp_sys_clkrst_reg.h:1378,1419-1427; i2c_ll.h:835-850,929-941 |
| 分周 | `PERI_CLK_CTRL11`（0x500E_6044）bit[7:0] `I2C1_CLK_DIV_NUM`（= 分周 − 1）、bit[15:8] 分子、bit[23:16] 分母（IDF はどちらも 0） | hp_sys_clkrst_reg.h:1432-1453; i2c_ll.h:179-189,218-228 |
| リセット | `HP_RST_EN1`（0x500E_60C4）bit21 `RST_EN_I2C1` を 1 にしてから 0 に戻す | hp_sys_clkrst_reg.h:3293,3446-3447; i2c_ll.h:156-166 |
| 既定のクロック源 | XTAL（`I2C_CLK_SRC_DEFAULT = SOC_MOD_CLK_XTAL`） | IDF/soc/esp32p4/include/soc/clk_tree_defs.h:492 |
| FIFO とコマンド | FIFO は 32 バイト、コマンドレジスタは 8 本 | IDF/soc/esp32p4/include/soc/soc_caps.h:312-313 |
| ハードウェアの回復機能 | FSM リセット（`SOC_I2C_SUPPORT_HW_FSM_RST`）とバスクリア（`SOC_I2C_SUPPORT_HW_CLR_BUS`）がある | soc_caps.h:316-317 |

- 解析用 I2C（regi2c、`I2C_ANA_MST`）とは別物。doc/research/m4 §1.1 で「bootloader がクロックを有効にしたまま残す」と書いたのは regi2c のほうで、I2C1 ではない。

### 1.2 ピン（GPIO31=SDA、GPIO32=SCL）
IDF の `s_hp_i2c_pins_config` [IDF/esp_driver_i2c/i2c_common.c:319-363] と同じことをレジスタで書くと次のようになる。`DR_REG_GPIO_BASE` = 0x500E_0000、`DR_REG_IO_MUX_BASE` = 0x500E_1000 [HW1/reg_base.h:112,114]。

| 手順 | レジスタ | 値 | 根拠 |
|---|---|---|---|
| 出力ラッチを High にする | `GPIO_OUT_W1TS`（0x500E_0008）bit31、`GPIO_OUT1_W1TS`（0x500E_0014）bit0 | 1 | gpio_reg.h:41,77 |
| オープンドレイン | `GPIO_PIN31_REG`（0x500E_00F0）、`GPIO_PIN32_REG`（0x500E_00F4）の bit2 `PAD_DRIVER` | 1 | gpio_reg.h:2182-2197,2241 |
| IO_MUX | `IO_MUX_GPIO31`（0x500E_1080）、`IO_MUX_GPIO32`（0x500E_1084）: `MCU_SEL`[14:12] = 1（GPIO）、`FUN_IE` bit9 = 1、`FUN_PD` bit7 = 0、`FUN_PU` bit8 は任意 | io_mux_reg.h:42-65,150,332,339 |
| 出力の行き先 | `GPIO_FUNC31_OUT_SEL_CFG`（0x500E_05D4）= 71（I2C1_SDA）、`GPIO_FUNC32_OUT_SEL_CFG`（0x500E_05D8）= 70（I2C1_SCL）。`OEN_SEL` bit10 = 0（出力イネーブルは周辺から）。さらに `GPIO_ENABLE_W1TS`（0x500E_0024）bit31 と `GPIO_ENABLE1_W1TS`（0x500E_0030）bit0 を立てる | gpio_reg.h:113,149,10880-10916; IDF/soc/esp32p4/include/soc/gpio_sig_map.h:145-148 |
| 入力の行き先 | `GPIO_FUNC70_IN_SEL_CFG`（0x500E_0270）= 32 \| bit7、`GPIO_FUNC71_IN_SEL_CFG`（0x500E_0274）= 31 \| bit7（bit7 `SIG_IN_SEL` = GPIO マトリクスを通す）。`FUNCn_IN_SEL_CFG` は 0x500E_0158 + 4n | gpio_reg.h:3716,5525-5552 |

- 出力の行き先を書いたあと ENABLE ビットも立てるのは、ESP32/S2 向けのパッチ版 `esp_rom_gpio_connect_out_signal` の動きに合わせた [IDF/esp_rom/patches/esp_rom_gpio.c:12-31]。P4 は ROM 版（`rom_gpio_matrix_out` = 0x4FC0_0570）を使っており、中身は同じと考えている（推測）[IDF/esp_rom/esp32p4/ld/esp32p4.rom.ld:302]。
- プルアップ: SYS_SDA と SYS_SCL には 5.1kΩ（R24、R26）が SOC_3.3V につながっている（SCH の pdftotext。部品番号と配線の対応は図の位置から読んだので推測）。UD は内蔵プルアップも有効にしている [UD/components/m5stack_tab5/m5stack_tab5.c:104-109]。
- I2C コントローラ側もオープンドレインにする。`CTR.SDA_FORCE_OUT`（bit0）と `SCL_FORCE_OUT`（bit1）を 0 にする（0 = オープンドレイン）[HW1/i2c_reg.h:30-50; i2c_ll.h:960-964]。

### 1.3 I2C1 のレジスタ（使うものだけ）
オフセットは I2C1 のベースからの値 [HW1/i2c_reg.h]。v1 と v3 でオフセットとビット位置は同じ（`hw_ver1` と `hw_ver3` の `i2c_reg.h` を比べて確認）。

| オフセット | 名前 | 使うフィールド |
|---|---|---|
| 0x00 | SCL_LOW_PERIOD | [8:0] |
| 0x04 | CTR | bit0 SDA_FORCE_OUT、bit1 SCL_FORCE_OUT、bit3 RX_FULL_ACK_LEVEL、bit4 MS_MODE、bit5 TRANS_START、bit6/7 TX/RX_LSB_FIRST、bit8 CLK_EN、bit9 ARBITRATION_EN、bit10 FSM_RST（自動で 0 に戻る）、bit11 CONF_UPGATE |
| 0x08 | SR | bit4 BUS_BUSY、[13:8] RXFIFO_CNT、[23:18] TXFIFO_CNT、[26:24] SCL_MAIN_STATE_LAST |
| 0x0C | TO | [4:0] TIME_OUT_VALUE（2^n サイクル）、bit5 TIME_OUT_EN |
| 0x18 | FIFO_CONF | bit10 NONFIFO_EN（0 にする）、bit12 RX_FIFO_RST、bit13 TX_FIFO_RST |
| 0x1C | DATA | 書くと TX FIFO に入り、読むと RX FIFO から [7:0] が出る |
| 0x20 / 0x24 | INT_RAW / INT_CLR | bit3 END_DETECT、bit5 ARBITRATION_LOST、bit7 TRANS_COMPLETE、bit8 TIME_OUT、bit10 NACK、bit13 SCL_ST_TO、bit14 SCL_MAIN_ST_TO |
| 0x30 / 0x34 | SDA_HOLD / SDA_SAMPLE | [8:0] |
| 0x38 | SCL_HIGH_PERIOD | [8:0] high、[15:9] wait_high |
| 0x40〜0x4C | SCL_START_HOLD、RSTART_SETUP、STOP_HOLD、STOP_SETUP | [8:0] |
| 0x50 | FILTER_CFG | [3:0]/[7:4] しきい値、bit8/9 有効（任意） |
| 0x58〜0x74 | COMD0〜7 | [13:0] コマンド、bit31 DONE |
| 0x78 / 0x7C | SCL_ST_TIME_OUT / SCL_MAIN_ST_TIME_OUT | [4:0]（23 以下） |
| 0x80 | SCL_SP_CONF | bit0 SCL_RST_SLV_EN（終わると自動で 0）、[5:1] SCL_RST_SLV_NUM |

根拠: HW1/i2c_reg.h:17-184（CTR、SR）、308-330（TO）、403-462（FIFO_CONF）、474（DATA）、486-612（INT_RAW）、1143-1167（FILTER）、1180-1199（COMD）、1360-1412（SCL_*_TO、SCL_SP_CONF）。

**コマンドの形式**（COMDn の [13:0]）[i2c_ll.h:28-48]:
`byte_num`[7:0]、`ack_en` bit8（書き込みで ACK を確かめる）、`ack_exp` bit9（期待する ACK の値。0）、`ack_val` bit10（読み込みで返す ACK。0 = ACK、1 = NACK）、`op_code`[13:11]（RSTART=6、WRITE=1、READ=3、STOP=2、END=4）。

| コマンド | 値 |
|---|---|
| RSTART | 0x3000 |
| WRITE n バイト（ACK を確かめる） | 0x0900 \| n |
| READ n バイト（各バイトに ACK） | 0x1800 \| n |
| READ 1 バイト（NACK。最後のバイト） | 0x1C01 |
| STOP | 0x1000 |
| END | 0x2000 |

### 1.4 初期化の手順（ポーリング、割り込みなし）
IDF の `i2c_hal_master_init` と `_i2c_hal_set_bus_timing` に合わせる [IDF/esp_hal_i2c/i2c_hal.c:27-58; IDF/esp_driver_i2c/i2c_master.c:860-871,690-705]。
1. クロックを入れてリセットする: `SOC_CLK_CTRL2` bit13 = 1、`PERI_CLK_CTRL10` bit26 = 0 / bit27 = 1、`PERI_CLK_CTRL11`[23:0] = 0（分周 1）、`HP_RST_EN1` bit21 を 1 → 0。
2. ピン（§1.2）。
3. `CTR` = MS_MODE（bit4）だけを立てる。FORCE_OUT は 0（オープンドレイン）、ARBITRATION_EN は 0、RX_FULL_ACK_LEVEL は 0、MSB ファースト [i2c_hal.c:39-50]。`FIFO_CONF.NONFIFO_EN` = 0。
4. タイミング（下の表）を書き、`TO` に TIME_OUT_VALUE と TIME_OUT_EN を書く。
5. `CTR.CONF_UPGATE`（bit11）= 1 で反映する。TX/RX FIFO をリセットする。`INT_ENA` = 0、`INT_CLR` = 0x3FFFF。

タイミングの計算は `i2c_ll_master_cal_bus_clk`（sclk = 40MHz、分周 1）[i2c_ll.h:93-120,179-210]:

| レジスタ | 100kHz | 400kHz | 計算 |
|---|---|---|---|
| SCL_LOW_PERIOD | 199 | 49 | half − 1（half = 40MHz / f / 2） |
| SCL_HIGH_PERIOD [8:0] | 102 | 27 | half − wait_high |
| SCL_HIGH_PERIOD wait_high [15:9] | 98 | 23 | f ≥ 80kHz なら half / 2 − 2 |
| SDA_HOLD | 49 | 11 | half / 4 − 1 |
| SDA_SAMPLE | 99 | 24 | half / 2 − 1 |
| RSTART_SETUP、STOP_SETUP、START_HOLD、STOP_HOLD | 199 | 49 | half − 1 |
| TO.TIME_OUT_VALUE | 17 | 17 | IDF は既定の 2000µs（`I2C_LL_SCL_WAIT_US_VAL_DEFAULT`）から `32 − clz(40 × 2000)` = 17（約 3.3ms）で上書きする [i2c_ll.h:77,1049-1054; i2c_hal.c:66-70] |

- M5GFX はエクスパンダを 100kHz、タッチを 400kHz で使う [M5G/M5GFX.cpp:2219-2223,2327]。BSP と UD は 400kHz [UD/components/m5stack_tab5/m5stack_tab5.c:254]。最初は 100kHz で通し、あとで 400kHz に上げる（判断）。

### 1.5 1 回の転送（ポーリング）
レジスタ読み出し（書き込みのあと再スタートして読む）の例。IDF の `i2c_master_transmit_receive` と同じ形にする [i2c_master.c:1295-1315]。
```
FIFO_CONF: TX_FIFO_RST, RX_FIFO_RST を 1 → 0
INT_CLR  = 0x3FFFF
DATA     = (addr << 1) | 0 ; DATA = reg（16 ビットのレジスタなら上位、下位の順に 2 バイト）
DATA     = (addr << 1) | 1
COMD0 = RSTART
COMD1 = WRITE(1 + reg_len)        // アドレスとレジスタ番号。ACK を確かめる
COMD2 = RSTART
COMD3 = WRITE(1)                  // 読み出し用のアドレス
COMD4 = READ(n - 1) ACK           // n = 1 のときは省く
COMD5 = READ(1) NACK
COMD6 = STOP
CTR |= CONF_UPGATE ; CTR |= TRANS_START
loop: r = INT_RAW
  r & TRANS_COMPLETE(bit7)        → 成功。RXFIFO_CNT を確かめて DATA を n 回読む
  r & NACK(bit10)                 → 相手がいない（プローブではこれが「不在」）
  r & (TIME_OUT|ARB_LOST|SCL_ST_TO|SCL_MAIN_ST_TO) → 回復（§1.6）
  ソフトのタイムアウト（たとえば 20ms）→ 回復
```
- 書き込み（エクスパンダへの 1 バイト書き込みなど）は `RSTART, WRITE(3), STOP` だけ。プローブは `RSTART, WRITE(1), STOP` で、NACK が立つかどうかを見る。
- **32 バイトの FIFO と 8 本のコマンドを超える転送** は、IDF では END コマンドで止めて詰め直す [i2c_master.c:184,284]。GT911 の 5 点（1 + 40 バイト）や ST712x の 10 点（70 バイト）は 32 バイトを超える。どちらもレジスタ番号を指定して読み直せるので、**31 バイト以下の転送に分ける**（判断）。GT911 は先頭アドレスを変えて読み直しても問題ない（推測。M5GFX も 0x814E を 1 バイト読んでから、続けて 8 × n バイトを読んでいる [M5G/lgfx/v1/touch/Touch_GT911.cpp:139-166]）。

### 1.6 エラーからの回復
- P4 は `SOC_I2C_SUPPORT_HW_FSM_RST` = 1 なので、IDF は `CTR.FSM_RST`（bit10、自動で 0 に戻る）を立て、続けてバスクリアをする [i2c_master.c:115-151; i2c_ll.h:774-778]。
- バスクリア: `SCL_SP_CONF` に `SCL_RST_SLV_NUM` = 9 と `SCL_RST_SLV_EN` = 1 を書き、`CTR.CONF_UPGATE` = 1。`SCL_RST_SLV_EN` が 0 に戻るまで待つ。戻らなければ（IDF は `I2C_CLR_BUS_TIMEOUT_MS` で打ち切る）EN を 0 にする [i2c_master.c:60-103; i2c_ll.h:792-812; i2c_ll.h:76]。
- そのあと FIFO をリセットし、INT_CLR を書く。

### 1.7 rev v1 での注意
- I2C のレジスタ配置は v1 と v3 で同じ。違うのは `CTR.CLK_EN`（bit8）の説明の向きだけ。v1 は「0: レジスタのクロックを常に入れる」、v3 は「1: 常に入れる」と書いてある [HW1/i2c_reg.h:110-119; hw_ver1 と hw_ver3 の i2c_struct.h:238-240 の差分]。どちらの解釈でも、ソフトがレジスタに触っている間はクロックが入るので、M6 では触らない（判断）。
- `esp_driver_i2c` と `esp_hal_i2c/esp32p4` に、チップのリビジョンで分岐するコードはない（`grep` で確認）。

## 2. PI4IOE5V6408（0x43、0x44）

### 2.1 レジスタ番号の確定
データシートの表 2（p.8）と §c（pp.9-11）で確定した。**M5GFX、M5Unified、UD、BSPC の番号はすべてデータシートと一致する**。

| 番号 | 名前（DS） | 電源投入時 | 意味（DS） |
|---|---|---|---|
| 0x01 | Device ID and Control | 1010 0010 | [7:5] メーカー ID = 101、[4:2] FW リビジョン、bit1 リセット割り込み（読むと消える）、bit0 ソフトリセット（1 を書くとリセット、読むと常に 0） |
| 0x03 | I/O Direction | 0x00 | 1 = 出力、0 = 入力 |
| 0x05 | Output State | 0x00 | 出力の値。読むとラッチの値（ピンの値ではない） |
| 0x07 | Output High-Impedance | 0xFF | 1 = 出力を Hi-Z |
| 0x09 | Input Default State | 0x00 | 割り込みの基準値 |
| 0x0B | Pull-up/down Enable | 0xFF | 1 = 有効 |
| 0x0D | Pull-up/down Select | 0x00 | 1 = プルアップ、0 = プルダウン |
| 0x0F | Input Status | x | 読み出し専用。書き込みは無視される。出力ピンは 0 と読める |
| 0x11 | Interrupt Mask | 0x00 | 1 = 割り込みを禁止 |
| 0x13 | Interrupt Status | x | 読むと消え、INT が戻る |

- 同じ番号を使っているもの: `M5G/M5GFX.cpp:2195-2217` のコメント、`M5U/utility/PI4IOE5V6408_Class.cpp:17,26-93`（入力は 0x0F、割り込み状態は 0x13 から読む）、`UD/components/m5stack_tab5/m5stack_tab5.c:230-240`、BSPC `esp_io_expander_pi4ioe5v6408.c:22-32`。
- **`TCC/main/wifi_setup.cpp:58-81` の名前は間違い**。0x09 を `kRegPullSel`、0x0F を `kRegInDefSta` と名づけている。その結果、
  - 0x09（Input Default State）に 0xB9 を書いている。#2 の入力は bit1、bit2、bit6 で、bit6（CHG_STAT）の基準が 1 になる。M5GFX の 0x40 と bit6 は同じなので、割り込みの振る舞いは変わらない（推測）。
  - 0x0F への 0x40 の書き込みは無視される（DS p.10）。
  - 本当の PULL_SEL（0x0D）は書いていない。M5GFX や M5Unified がそれより前に書いていれば、その値が残る。
  - 害は小さいが、Rust では DS の名前を使う。
- 電源投入時は **全ピンが入力で、プルダウンが有効**（DIR=0x00、PULL_EN=0xFF、PULL_SEL=0x00）。つまり LCD_RST と TP_RST は Low に引かれ、**パネルとタッチはリセットされたまま** で起動する（DS 表 2、p.11 の Power-on Reset）（推測。回路側に別のプルアップがないことは回路図で確かめていない）。
- エクスパンダは SoC のリセットではリセットされない（電源が別）。SW リセットで再起動したときは、前の状態が残っている（推測）。手順は、今の状態によらず同じ結果になるようにする（判断）。
- 0x01 は、電源投入やソフトリセットのあと 0xA2 と読め、一度読むと bit1 が消えて 0xA0 になる（DS p.9 表 3）。M5GFX と M5Unified は「0x01 が読めるか」と「0 でないか」でボードの存在を判定している [M5G/M5GFX.cpp:2189-2190; M5U/utility/PI4IOE5V6408_Class.cpp:15-20]。
- BSPC の `reset()` は、ソフトリセットのあと DIR=0xFF、OUT_H_IM=0xFF、PULL_SEL=0x00、PULL_EN=0xFF、OUT=0x00 を書く。コメントは「all inputs」だが、DIR=0xFF は全出力（ただし Hi-Z）である [BSPC/esp_io_expander_pi4ioe5v6408.c:34-39,244-270]。結果として出力は Hi-Z になり、プルダウンで Low になるので、実害はない（推測）。

### 2.2 各ソースの書き込み内容
**#1（0x43）**。ビットの意味は doc/06 §1.3。bit4 = LCD_RST、bit5 = TP_RST。

| 出典 | 書く順番と値 |
|---|---|
| M5G（0.2.20）[M5GFX.cpp:2195-2205,2219-2223] | DIR=0x7F → OUT=0x46 → OUT_H_IM=0x00 → PULL_SEL=0x7F → PULL_EN=0x7F →（#2 を書く）→ 10ms → OUT=0x76 |
| M5G↑（0.2.36）[M5GFX.cpp:3213-3244] | **OUT=0x46 を先に書いてから** DIR=0x7F → OUT_H_IM=0x00 → PULL_SEL=0x7F → PULL_EN=0x7F →（#2）→ 10ms → **DIR=0x6F**（LCD_RST を入力にする。プルアップで High になる）→ OUT=0x76（TP_RST を High）→ GPIO23 を入力に戻す → 100ms |
| UD [m5stack_tab5.c:258-282] | 0x01=0xFF（ソフトリセット）→ 0x01 を読む → DIR=0x7F → OUT_H_IM=0x00 → PULL_SEL=0x7F → PULL_EN=0x7F → OUT=0x76（Low の期間をとらない） |
| BSP [src/bsp_feature_en.c:19-35; src/bsp_display.c:148-154,213] | BSPC の reset() のあと、LCD: bit4 をプルアップ、入力、オープンドレインにする。TOUCH: bit5 を出力、High、プッシュプルにする。そのあと 500ms 待ってタッチを判別する |

- M5G↑ のコメントは「繰り返し初期化するとき、リセット出力を有効にする前に Low を入れておく」[M5GFX.cpp:3214]。DIR より先に OUT を書くのは、出力に切り替えた瞬間に High が出ないようにするため。
- **LCD_RST の解除は「入力 + プルアップ」** が BSP と M5G↑ に共通している。TP_RST はプッシュプルで High にしている。理由は書かれていない（§8）。

**#2（0x44）**: LCD とタッチには関係しない。M5GFX は DIR=0xB9、OUT_H_IM=0x06、PULL_SEL=0xB9、PULL_EN=0xF9、IN_DEF_STA=0x40、INT_MASK=0xBF、OUT=0x89（CHG_EN、USB5V_EN、WLAN_PWR_EN を High）を書く [M5G/M5GFX.cpp:2208-2217; M5G↑ 3230-3238]。UD は OUT=0x09（CHG_EN は 0）[m5stack_tab5.c:318-323]。

### 2.3 M6 でのやり方（推奨）
1. **#1 だけを設定する**。#2 は M6 では触らない（判断）。#2 には PWROFF_PULSE（bit4）があり、トグルすると電源が切れる（doc/06 §1.3）。WLAN、USB、充電の方針は別のマイルストーンで決める。
   - ただし #2 を一度も書かないと、電源投入時の状態（全入力、プルダウン）のままになる。CHG_EN（bit7）が Low に引かれるので、**充電が止まっている可能性がある**（推測。IP2326 のイネーブルの極性は未確認）。M6 の段階で M5GFX と同じ値を書くかどうかは、§8 に残す。
2. #1 の手順（M5G↑ に合わせる）:
   ```
   0x43: OUT      = 0x44   // bit2 EXT5V_EN=1, bit6 CAM_RST=1, LCD_RST=TP_RST=0, SPK_EN=0
   0x43: DIR      = 0x7F   // bit0-6 を出力、bit7 HP_DET を入力
   0x43: OUT_H_IM = 0x00
   0x43: PULL_SEL = 0x7F   // bit0-6 プルアップ（入力に戻した bit4 で効く）
   0x43: PULL_EN  = 0x7F
   GPIO23 = 出力 High        // GT911 のアドレス選択（§3.2）
   10ms 待つ                 // リセットの Low 期間（M5GFX と同じ）
   0x43: DIR      = 0x6F   // bit4 LCD_RST を入力に。プルアップで High = 解除
   0x43: OUT      = 0x64   // bit5 TP_RST = 1 = 解除
   GPIO23 = 入力（プルアップは外付け 5.1kΩ）
   100ms 待つ               // M5G↑ と同じ
   ```
   - M5GFX の OUT は 0x46（SPK_EN=1）。スピーカのアンプは M6 では要らないので 0 にする（判断）。
   - EXT5V_EN（bit2）と CAM_RST（bit6）は、画面には関係しない。M5GFX の値（1）に合わせておく（判断）。外部 5V を止めたいなら 0 でもよい。
3. 触らなくてよいビット: #1 の bit0（アンテナ切り替え。Wi-Fi まで不要）、bit1（SPK_EN）、bit2（EXT5V_EN）、bit3（NC）、bit6（CAM_RST）、bit7（HP_DET、入力）。#2 は全部。
4. 0x01 にソフトリセット（0xFF）を書く方法（UD、BSPC）もある。#1 だけならリセットの Low を確実に作れる。ただし DIR=0x00 とプルダウンに戻るので、手順 2 と結果は同じになる。#2 にはソフトリセットを書かない（WLAN の電源などが一瞬切れる）（判断）。

## 3. パネルの判別

### 3.1 3 つのロットと判別方法
| ロット | パネル | タッチ | I2C の応答 | DSI の ID |
|---|---|---|---|---|
| 初期 | ILI9881C | GT911 | 0x14（または 0x5D） | page1 の reg 0x00, 0x01 = 98 81 |
| 2025-10-14 以降 | ST7123 | 内蔵（0x55） | 0x55、FW バージョン = 3 | reg 0xF4 = 71 23 |
| 2026-04-28 以降 | ST7121 | ST712x 互換（0x55） | 0x55、FW バージョン = 1 | 未確認（§8） |

ロットの日付は doc/06 §7。FW バージョンの値は BSP `src/bsp_display.c:156-183` と M5G↑ `M5GFX.cpp:3270-3299` で一致する。

### 3.2 I2C での判別（先にやる）
- **前提**: LCD_RST と TP_RST の両方を解除しておく（§2.3）。ST712x はパネルとタッチが同じチップなので、LCD_RST だけで足りるかは不明（§8）。両方解除すれば、どのロットでも問題ない。
- **ST712x**: 0x55 に、2 バイトのレジスタ番号 `00 00` を書いてから 1 バイト読む（ビッグエンディアンの 16 ビット）[M5G↑ M5GFX.cpp:3272-3275; UD/managed_components/espressif__esp_lcd_touch_st7123/include/esp_lcd_touch_st7123.h:31-46]。
  - 1 なら ST7121、3 なら ST7123。それ以外は不明として扱う。
  - M5G↑ は **10ms おきに最大 60 回（600ms）** 試す。コメントによると、ST71xx のタッチは、動作中にリセットされると応答できるまで数十 ms（実測で約 50ms）かかる。待ちを打ち切ると判別に失敗して画面が出ない [M5GFX.cpp:3262-3269]。
  - BSP は、タッチを有効にしてから **500ms** 待って 0x55 をプローブし、それから FW バージョンを読む [src/bsp_display.c:148-172]。
- **GT911**: 0x55 が応答しなければ、0x14 に **アドレスだけを送って ACK を見る**。M5G↑ のコメントは「実際に読むと内部のポインタが進むので、ACK の確認だけにする」[M5GFX.cpp:3290-3297]。BSP も 0x14 だけを見る [src/bsp_display.c:184-186]。
- **GT911 のアドレス選択**: GT911 は、リセットが解除される瞬間の INT の値でアドレスを決める。High なら 0x14、Low なら 0x5D [M5G/M5GFX.cpp:2186（コメント）; UD/managed_components/espressif__esp_lcd_touch_gt911/esp_lcd_touch_gt911.c:100-133]。M5GFX は GPIO23 を High に出力してから TP_RST を解除する。
  - Tab5 の TP_INT_GPIO23 には **5.1kΩ のプルアップ（R92）** があるように読める（SCH の pdftotext。推測）。BSP にも「INT に 3V3 への抵抗がある」とコメントがある [src/bsp_display.c:499-503]。そうなら、SoC が駆動しなくても INT は High なので、**アドレスは常に 0x14** になる（推測）。BSP と UD が 0x14 しか見ないことと合っている。
  - M5GFX の `Touch_GT911` は 0x14 で失敗すると 0x5D も試す [M5G/lgfx/v1/touch/Touch_GT911.cpp:66-87]。カーネルも、0x14 が NACK なら 0x5D を見る（判断）。
- 順番は M5G↑ と同じにする（ST の FW → GT911 の ACK、をくり返す）。BSP は 0x55 → 0x14 の順に 1 回だけ見る。

### 3.3 DSI での判別（確認用）
- M5G（0.2.20）は DSI だけで判別していた。DSI バスを初期化し、80ms 待ってから、reg 0xF4 を 2 バイト読む。`71 23` なら ST7123。違えば `FF 98 81 01` を送って page1 に切り替え、reg 0x00、0x01、0x02 を 1 バイトずつ読む。`98 81` なら ILI9881C で、`FF 98 81 00` で page0 に戻す。これを 3 回まで試す [M5G/M5GFX.cpp:2245-2270]。
- M5G↑ は I2C で ST 系と分かっていれば DSI の ID を読まない。ILI9881C の確認にだけ使う [M5GFX.cpp:3321-3341]。
- **I2C を先にする理由**:
  1. タッチとパネルはどちらもエクスパンダのリセットで止まっているので、解除さえすれば I2C はすぐ使える。DSI は LDO、PHY、ホストの初期化が終わるまで使えない。
  2. ST7121 と ST7123 は lane 速度が違う（900 / 1040Mbps）。どちらの速度で DSI を立ち上げるかを、DSI の ID を読む前に決める必要がある [M5G↑ M5GFX.cpp:3266,3314]。
  3. DSI の読み出しは、BTA（バスの向きの反転）を使うので、ホスト側の実装が進むまで当てにできない（推測）。
- DSI の ID は、パネルの初期化コマンドを送る前の確認に使う（ILI9881C なら page1 の ID、ST7123 なら 0xF4）（判断）。

## 4. パネルごとのデータ

### 4.1 共通
- 解像度は 720 × 1280（縦長が本来の向き）、2 lane、映像モード（DPI）[M5G/M5GFX.cpp:2304-2309; BSP include/bsp/display.h:39-42]。
- コマンドは DCS の書き込みとして **LP モードで** 送られる（IDF の DBI IO は short write、long write、short read をすべて LP に設定する）[IDF/esp_lcd/dsi/esp_lcd_panel_io_dbi.c:47-50,75,86]。パラメータなしは DCS short write（0x05）、1 個は 0x15、2 個以上は long write（0x39）になる（推測。データ型の選び方は DSI 担当の範囲）。
- **画素の形式は全パネル RGB565**。DPI の入力と出力はどちらも RGB565 [M5G/lgfx/v1/platforms/esp32p4/Panel_DSI.cpp:80-81; BSP src/bsp_display.c:246,267,288]。M5GFX は初期化の最後に MADCTL=0x00、COLMOD=0x55 を送る [Panel_DSI.cpp:53-64]。
  - RGB888 にしない理由（推測）: 80MHz × 24 ビット / 2 lane = 960Mbps/lane で、lane 速度（900〜1040Mbps）の余裕がほぼない。RGB565 なら 640Mbps/lane。
- M5GFX のテーブルは「(長さ, コマンド, パラメータ…) の並び、長さ 0 で終わり」のリストを順に送り、リストごとに決まった時間待つ [Panel_DSI.cpp:36-67]。そのあと MADCTL と COLMOD を送り、最後に DPI（映像）を始める（`esp_lcd_panel_init`）。M5G↑ も、Tab5 のパネルについては同じ順番（`initInCommandMode()` の既定値が true で、どのパネルも上書きしていない）[M5G↑ lgfx/v1/platforms/esp32p4/Panel_DSI.inl:39-78, Panel_DSI.hpp:92]。
- バックライトは M5GFX でもパネル初期化の後に点ける（§5）。

### 4.2 タイミング
| | ILI9881C | ST7123 | ST7121 |
|---|---|---|---|
| hsync パルス / bp / fp | 40 / 140 / 40 | 2 / 40 / 40 | 2 / 40 / 40 |
| vsync パルス / bp / fp | 4 / 20 / 20 | 2 / 8 / **220** | 20 / 24 / 200 |
| h 合計 × v 合計 | 940 × 1324 | 802 × 1510 | 802 × 1524 |
| DPI クロック（M5GFX） | 80MHz（64.3Hz） | 80MHz（66.1Hz） | 70MHz（57.3Hz） |
| DPI クロック（BSP） | 60MHz（48.2Hz） | 70MHz（57.8Hz） | 70MHz（57.3Hz） |
| lane 速度（M5GFX） | 1040Mbps | 1040Mbps | **900Mbps** |
| lane 速度（BSP） | 1000Mbps | 1000Mbps | **965Mbps** |
| lane 速度（UD） | 1000Mbps | 965Mbps | – |
| 必要な帯域（RGB565、2 lane） | 640Mbps/lane（80MHz） | 640Mbps/lane（80MHz） | 560Mbps/lane（70MHz） |

根拠: M5G/M5GFX.cpp:2242,2271-2298; M5G↑ M5GFX.cpp:3314,3343-3385; BSP src/bsp_display.c:45,242-304,395-417、include/bsp/display.h:42; UD/components/m5stack_tab5/m5stack_tab5.c:1168-1185（ILI9881C）,1373-1405（ST7123）。括弧内のリフレッシュレートは、クロック ÷（h 合計 × v 合計）で計算した値。

- doc/06 §1.4 は「965Mbps（BSP、ST712x）」と書いたが、今の BSP で 965 を使うのは **ST7121 だけ**。ST7123 は 1000Mbps [src/bsp_display.c:410-417]。965 を ST7123 に使っていたのは UD [m5stack_tab5.c:1376]。
- **ST7123 の癖**（M5GFX のコメント）[M5G/M5GFX.cpp:2294-2297]:
  - 「back + pulse == 10 にする。ずれると表示が縦にずれる」（vbp 8 + vpw 2）。
  - 「front porch を減らすとタッチが止まる」（vfp 220）。ST712x はタッチとパネルが同じチップで、垂直のブランキング期間にタッチを走査していると考えられる（推測）。ST7121 も vfp 200 と大きい。**この 2 つは BSP も同じ値** なので、変えない。
- DPI クロックをどこまで正確に出せるかは、DSI 担当の範囲。M5GFX と BSP で 60〜80MHz と幅があるので、リフレッシュレートが多少ずれても表示はされる（推測）。

### 4.3 ILI9881C（M5GFX の順番）
出典: M5G/lgfx/v1/platforms/esp32p4/Panel_ILI9881C.hpp:49-303（M5G↑ と完全に同じ）。リスト 2 と 3 のコマンド 193 個は、UD の `include/bsp/ili9881_init_data.c:3-216` と BSP の `priv_include/disp_init_data.h` とも **1 バイトも違わない**（スクリプトで比べた）。

形式: 1 行が 1 コマンドで `CMD  パラメータ…`。`RR=VV` は「コマンド RR、パラメータ 1 個 VV」を 8 個ずつ並べたもの。`FF 98 81 0n` はページの切り替え。

```
# リスト 0（送ったあと待たない）
FF  98 81 01           # page 1
B7  03                 # PAD_CONTROL: 2 lane（3〜4 lane なら 02）
# リスト 1（送ったあと 120ms 待つ）
FF  98 81 00           # page 0
11                     # SLPOUT
# リスト 2（待たない）
FF  98 81 03           # page 3（GIP の設定）
  01=00  02=00  03=73  04=00  05=00  06=08  07=00  08=00
  09=1B  0A=01  0B=01  0C=0D  0D=01  0E=01  0F=26  10=26
  11=00  12=00  13=02  14=00  15=00  16=00  17=00  18=00
  19=00  1A=00  1B=00  1C=00  1D=00  1E=40  1F=00  20=06
  21=01  22=00  23=00  24=00  25=00  26=00  27=00  28=33
  29=03  2A=00  2B=00  2C=00  2D=00  2E=00  2F=00  30=00
  31=00  32=00  33=00  34=00  35=00  36=00  37=00  38=00
  39=00  3A=00  3B=00  3C=00  3D=00  3E=00  3F=00  40=00
  41=00  42=00  43=00  44=00  50=01  51=23  52=45  53=67
  54=89  55=AB  56=01  57=23  58=45  59=67  5A=89  5B=AB
  5C=CD  5D=EF  5E=11  5F=02  60=00  61=07  62=06  63=0E
  64=0F  65=0C  66=0D  67=02  68=02  69=02  6A=02  6B=02
  6C=02  6D=02  6E=02  6F=02  70=02  71=02  72=02  73=05
  74=01  75=02  76=00  77=07  78=06  79=0E  7A=0F  7B=0C
  7C=0D  7D=02  7E=02  7F=02  80=02  81=02  82=02  83=02
  84=02  85=02  86=02  87=02  88=02  89=05  8A=01
# リスト 3（待たない）
FF  98 81 04           # page 4（電源まわり）
  38=01  39=00  6C=15  6E=1A  6F=25  3A=A4  8D=20  87=BA
  3B=98
FF  98 81 01           # page 1（電源とガンマ。A0-B3 が正、C0-D3 が負）
  22=0A  31=00  50=6B  51=66  53=73  55=8B  60=1B  61=01
  62=0C  63=00  A0=00  A1=15  A2=1F  A3=13  A4=11  A5=21
  A6=17  A7=1B  A8=6B  A9=1E  AA=2B  AB=5D  AC=19  AD=14
  AE=4B  AF=1D  B0=27  B1=49  B2=5D  B3=39  C0=00  C1=01
  C2=0C  C3=11  C4=15  C5=28  C6=1B  C7=1C  C8=62  C9=1C
  CA=29  CB=60  CC=16  CD=17  CE=4A  CF=23  D0=24  D1=4F
  D2=5F  D3=39
FF  98 81 00           # page 0
35                     # TEON（パラメータなし）
FE                     # 不明（ベンダのコマンド。3 つの出典すべてにある）
29                     # DISPON
# このあと（M5GFX）
36  00                 # MADCTL
3A  55                 # COLMOD = RGB565
→ DPI の映像を開始
```
- ESP-IDF の `esp_lcd_ili9881c`（UD と BSP が使う）は、順番が少し違う。SWRESET（0x01）+ 20ms（RST の GPIO がないため）→ page1 の ID を読む → B7=03 → page0 → SLPOUT → **120ms** → MADCTL=0x00 → COLMOD=0x55 → 上のテーブル（先頭の page1 と B7 を含む）→ DPI 開始 → DISPON をもう一度 [UD/managed_components/espressif__esp_lcd_ili9881c/esp_lcd_ili9881c.c:366-470,472-487,537-550; UD/components/m5stack_tab5/m5stack_tab5.c:1205-1208]。
- SLPOUT のあと 120ms 待つことは、どの出典も同じ。M5GFX は SWRESET を送らない。カーネルは、エクスパンダによるハードウェアリセットのあとに M5GFX の順番で送る（判断）。

### 4.4 ST7123
出典: M5G/lgfx/v1/platforms/esp32p4/Panel_ST7123.hpp:34-131（M5G↑ と完全に同じ）。リスト 0 の 26 個は、BSP `priv_include/disp_init_data_1.h:10-32` と UD `components/m5stack_tab5/m5stack_tab5.c:1303-1353` と、バイトの並びが一致する（スクリプトで比べた）。括弧内はパラメータの数。

```
# リスト 0（待たない）
60  71 23 A2                       # ベンダのコマンドの鍵を開ける（推測）
60  71 23 A3
60  71 23 A4
A4  31
D7  10 0A 10 2A 80 80
90  71 23 5A 20 24 09 09
A3  80 01 88 30 05 00 00 00 00 00 46 00 00 1E 5C 1E          (39)
    80 00 4F 05 00 00 00 00 00 46 00 00 1E 5C 1E 80
    00 6F 58 00 00 00 FF
A6  03 00 24 55 36 00 39 00 6E 6E 91 FF 00 24 55 38          (55)
    00 37 00 6E 6E 91 FF 00 24 11 00 00 00 00 6E 6E
    91 FF 00 EC 11 00 03 00 03 6E 6E FF FF 00 08 80
    08 80 06 00 00 00 00
A7  19 19 80 64 40 07 16 40 00 44 03 6E 6E 91 FF 08          (60)
    80 64 40 25 34 40 00 02 01 6E 6E 91 FF 08 80 64
    40 00 00 40 00 00 00 6E 6E 91 FF 08 80 64 40 00
    00 00 00 20 00 6E 6E 84 FF 08 80 44
AC  03 19 19 18 18 06 13 13 11 11 08 08 0A 0A 1C 1C          (44)
    07 07 00 00 02 02 01 19 19 18 18 06 12 12 10 10
    09 09 0B 0B 1C 1C 07 07 03 03 01 01
AD  F0 00 46 00 03 50 50 FF FF F0 40 06 01 07 42 42          (25)
    FF FF 01 00 00 FF FF FF FF
AE  FE 3F 3F FE 3F 3F 00
B2  15 19 05 23 49 AF 03 2E 5C D2 FF 10 20 FD 20 C0          (17)
    00
E8  20 6F 04 97 97 3E 04 DC DC 3E 06 FA 26 3E                (14)
75  03 04
E7  3B 00 00 7C A1 8C 20 1A F0 B1 50 00 50 B1 50 B1          (36)
    50 D8 00 55 00 B1 00 45 C9 6A FF 5A D8 18 88 15
    B1 01 01 77
EA  13 00 04 00 00 00 00 2C
B0  22 43 11 61 25 43 43
B7  00 00 73 73
BF  A6 AA
A9  00 00 73 FF 00 00 03 00 00 03
C8  00 00 10 1F 36 00 5D 04 9D 05 10 F2 06 60 03 11          (37)
    AD 00 EF 01 22 2E 0E 74 08 32 DC 09 33 0F F3 77
    0D B0 DC 03 FF
C9  （C8 と同じ 37 バイト）
# リスト 1（M5GFX。送ったあと 120ms 待つ）
29                                 # DISPON
35  00                             # TEON
11                                 # SLPOUT
# このあと（M5GFX）
36  00                             # MADCTL
3A  55                             # COLMOD
→ DPI の映像を開始
```
末尾は出典によって違う。

| 出典 | 末尾 | 開始前 |
|---|---|---|
| M5G / M5G↑ | DISPON → TEON(00) → SLPOUT → 120ms → MADCTL=00 → COLMOD=55 | なし |
| BSP（`disp_init_data_1.h:33-36`）+ IOT | MADCTL=03 → SLPOUT(1 バイト 00) → 120ms → DISPON(1 バイト 00) → 50ms → TEON(00)。DPI 開始のあと、`mirror(false,false)` で MADCTL=00 に書き直す | SWRESET + 120ms（IOT `esp_lcd_st7123.c:288-307`）。IOT は各コマンドのあとに 5ms 待つ（:279） |
| UD（`m5stack_tab5.c:1354-1357`） | MADCTL=00 → SLPOUT(00) → 100ms → DISPON(00) → TEON(00) → 100ms | なし（UD のドライバは SWRESET を送らず、20ms 待つだけ。`esp_lcd_st7123.c:285-302`） |

- 最終的な MADCTL はどれも 0x00。BSP の 0x03 は、IOT の `panel_st7123_mirror` が GS（bit0）と SS（bit1）を消して 0x00 を書き直す [IOT esp_lcd_st7123.c:22-23,327-351; BSP src/bsp_display.c:366-369]。
- COLMOD は、M5GFX だけが送る。IOT と UD のドライバは送らない [IOT esp_lcd_st7123.c（`colmod_val` を保存するだけ）; UD/components/m5stack_tab5/esp_lcd_st7123.c:209-232]。映像モードでは、DSI のパケットの種類で画素の形式が決まるので、なくても動くと考えられる（推測）。送っても害はない。
- **BSP のバグ**: `disp_init_data_1.h` の A3 は、パラメータ 39 個に対して長さを 40、E8 は 14 個に対して 15 と書いている [:16,23]。UD も同じ [m5stack_tab5.c:1309-1312,1331]。配列の外の 1 バイトを送ってしまう。M5GFX の長さ（39 と 14）が正しい。
- SLPOUT と DISPON の順番は、M5GFX だけ逆（DISPON が先）。M5GFX は出荷品で使われているので、どちらでも動く（推測）。カーネルでは、一般的な順番（SLPOUT → 120ms → DISPON → TEON）にする（判断。BSP と UD と同じ）。うまくいかなければ M5GFX の順番を試す。

### 4.5 ST7121
出典: M5G↑ lgfx/v1/platforms/esp32p4/Panel_ST7121.hpp:34-143。ローカルの M5G（0.2.20）にはない。リスト 0 の 30 個と末尾は BSP `priv_include/disp_init_data_st7121.h:9-110` と一致する（スクリプトで比べた）。

```
# リスト 0（待たない）
60  71 21 A2
60  71 21 A3
60  71 21 A4
78  21
79  EF
A4  31
B7  00 00 5F 5F 44 1A
B0  22 6B 11 89 25 43 43
BF  A7 A7
A5  F0 03
D7  10 2C 14 2A 80 80
90  71 23 5A 20 24 11 21                                     # 71 23 のまま（両出典とも）
A3  80 01 8C FF 45 00 00 00 00 00 46 00 00 1E 5C 1E          (39)
    80 10 00 05 00 00 00 00 00 46 00 00 1E 5C 1E 80
    10 EF 58 00 00 00 FF
A6  0A 00 24 71 36 00 00 00 68 68 91 FF 00 24 71 37          (55)
    00 00 00 68 68 91 FF 00 24 71 00 00 00 00 68 68
    91 FF 00 2C 71 00 01 00 00 68 68 FF FF 00 08 80
    08 80 06 00 00 00 00
A7  1A 1A C0 64 40 04 15 40 00 40 00 68 68 91 FF 08          (60)
    80 64 40 26 37 40 00 00 00 68 68 91 FF 08 80 64
    40 8C 9D 40 00 00 00 68 68 91 FF 08 80 64 40 AE
    BF 00 00 20 00 68 68 91 FF 08 80 79
AC  1D 18 19 1D 18 19 04 1C 1D 08 0A 10 12 0C 0E 14          (44)
    16 00 1D 1D 1D 1D 1D 18 19 1D 18 19 06 1C 1D 09
    0B 11 13 0D 0F 15 17 02 1D 1D 1D 1D
AD  0C 40 46 00 07 4B 4B FF FF F0 40 0E 01 07 42 42          (25)
    FF FF 01 00 00 FF FF FF FF
AE  F0 FF 03 F0 FF 03 00
B2  15 19 05 23 49 2D 03 2E 5C D2 FF 10 60 FD 20 C0          (17)
    00
E8  20 60 04 8E 8E 3E 04 DC DC 3E 06 FA 26 3E                (14)
75  03 04
E7  4B 00 00 BE 4B 8C 20 1A F0 7D 14 7D 14 7D 14 7D          (42)
    14 FF 00 32 30 73 00 00 C8 6A FF 5A 64 38 88 15
    B1 01 01 64 01 01 7C FF 1A 51
E1  0C 0C
EA  15 00 01
C8  00 00 04 08 10 00 1F 01 39 3E 00 78 06 E2 02 11          (37)
    33 01 7A 0D 21 C4 0B 19 08 32 A0 08 1A 0A F3 7F
    0E C5 E8 03 FF
C9  （C8 と同じ 37 バイト）
60  71 21 00                       # 鍵を閉じる（推測）
# リスト 1: 11（SLPOUT）→ 80ms
# リスト 2: 29（DISPON）→ 800ms
# リスト 3: 35 00（TEON）
# このあと（M5GFX）: 36 00、3A 55 → DPI の映像を開始
```
- BSP も同じ遅延（SLPOUT 80ms、DISPON 800ms）[disp_init_data_st7121.h:107-109]。BSP では MADCTL を送らないが、`mirror(false,false)` で 0x00 になる。
- BSP の E8 と A3 の長さは、ST7121 では正しい（14 と 39）。

### 4.6 ソース間の違いのまとめ
| 項目 | M5GFX | BSP / IOT | UD |
|---|---|---|---|
| ソフトウェアリセット | 送らない | ILI: SWRESET + 20ms。ST: SWRESET + 120ms | ILI: SWRESET + 20ms。ST: 20ms 待つだけ |
| ILI9881C の SLPOUT の位置 | テーブルより前（120ms） | 同じ | 同じ |
| ST7123 の末尾 | DISPON → TEON → SLPOUT → 120ms | SLPOUT → 120ms → DISPON → 50ms → TEON | SLPOUT → 100ms → DISPON → TEON → 100ms |
| MADCTL / COLMOD | 00 / 55（最後に送る） | 00（mirror で）/ ILI のみ 55 | ILI: 00 / 55。ST: 00 / なし |
| フレームバッファの数 | 1（0.2.20）、2（0.2.36） | Kconfig | 1 |

## 5. バックライト（GPIO22）
- 回路: GPIO22（LCD_BL_GPIO22）は **U8 ME2212 の EN（4 番ピン）** につながる。ME2212 の VIN は SOC_3.3V、出力は LCD_LEDA（FPC の 3、4 番）、FB は LCD_LEDK から 3.9Ω（R25、1%）で GND に落ちる（SCH の pdftotext。ピン番号は図の位置から読んだので推測）。**昇圧回路を別に有効にする信号はない**。EN がそのままオンオフになる。
  - LED の電流は FB の基準電圧 ÷ 3.9Ω で決まる。ME2212 の FB 電圧は手元の資料にない（§8）。
- 3 つの実装:
  - M5GFX: LEDC のチャネル 7、タイマー 3（`(ch >> 1) & 3`）、9 ビット、44.1kHz、反転なし [M5G/M5GFX.cpp:683-694,2339; M5G/lgfx/v1/platforms/esp32/Light_PWM.cpp]。
  - BSP: LEDC のチャネル `CONFIG_BSP_DISPLAY_BRIGHTNESS_LEDC_CH`、タイマー 0、10 ビット、5kHz [src/bsp_display.c:54-78]。
  - UD: LEDC のチャネル 1、タイマー 0、12 ビット、5kHz。GPIO の High だけにするコードもコメントで残っている [UD/components/m5stack_tab5/m5stack_tab5.c:1040-1080]。
  - どれも「High = 点灯」（反転なし）。
- **M6 は GPIO の High/Low だけにする**（判断）。
  - `IO_MUX_GPIO22`（0x500E_105C）: MCU_SEL = 1、FUN_IE = 0、FUN_PU = FUN_PD = 0。
  - `GPIO_FUNC22_OUT_SEL_CFG`（0x500E_05B0）= 256（GPIO_OUT を出す）、OEN_SEL（bit10）= 1。
  - `GPIO_ENABLE_W1TS`（0x500E_0024）bit22 = 1。
  - 点灯: `GPIO_OUT_W1TS`（0x500E_0008）bit22。消灯: `GPIO_OUT_W1TC`（0x500E_000C）bit22。
  - 根拠: HW1/gpio_reg.h:29-125,10880-10896（OUT_SEL の意味。256 = GPIO_OUT_REG）; io_mux_reg.h:284。
  - 電源投入時の GPIO22 は入力で、ME2212 の EN は不定かプルダウンで消灯（推測）。パネルの初期化が終わって最初の画面を書いてから点ける（判断。初期化中のごみを見せない）。
- PWM（あとで）: LEDC のベースは 0x500D_3000 [HW1/reg_base.h:100]。手順の骨子は次のとおり。v1 と v3 でオフセットは同じ。
  1. `SOC_CLK_CTRL3`（0x500E_6020）bit0 `LEDC_APB_CLK_EN` = 1。`HP_RST_EN1`（0x500E_60C4）bit29 `RST_EN_LEDC` を 1 → 0。`PERI_CLK_CTRL22`（0x500E_609C）[1:0] `LEDC_CLK_SRC_SEL`（0 = XTAL、1 = RC_FAST、2 = PLL_F80M）、bit2 `LEDC_CLK_EN` = 1 [hp_sys_clkrst_reg.h:834-841,2484-2501,3497; IDF/hal/esp32p4/include/hal/ledc_ll.h:131-260]。
  2. `LEDC_CONF`（+0x170）: `APB_CLK_SEL`[1:0]、`CLK_EN` bit31 [ledc_reg.h:2810-2900]。
  3. `TIMERn_CONF`（+0xA0 + 8n）: `DUTY_RES`[4:0]、`CLK_DIV`[22:5]（小数 8 ビットつき）、`PAUSE` bit23、`RST` bit24（リセット値 1）、`PARA_UP` bit26 [ledc_reg.h:915-960]。
  4. `CHn_CONF0`（+0x14n）: `TIMER_SEL`[1:0]、`SIG_OUT_EN` bit2、`IDLE_LV` bit3、`PARA_UP` bit4。`CHn_HPOINT`（+0x14n+4）、`CHn_DUTY`（+0x14n+8、duty << 4）、`CHn_CONF1`（+0x14n+0xC）bit31 `DUTY_START` [ledc_reg.h:25-112; ledc_ll.h:435-623]。
  5. **P4 は duty を反映するのに、ガンマ RAM の範囲 0 の fade パラメータと、範囲の数（1）を書く必要がある**。IDF の `ledc_duty_config` は `ledc_hal_set_fade_param(..., 0, dir, cycle, scale, num)` と `ledc_hal_set_range_number(1)` を呼び、残りの範囲を消している [IDF/esp_driver_ledc/src/ledc.c:265-282; ledc_ll.h:514-566]。ここが手間なので、M6 では入れない。
  6. GPIO22 の出力の行き先を `LEDC_LS_SIG_OUT_PAD_OUT{n}_IDX`（126 + n）にする [IDF/soc/esp32p4/include/soc/gpio_sig_map.h:235-241]。

## 6. タッチ

### 6.1 GT911（ILI9881C のロット）
- I2C アドレスは 0x14（§3.2）。レジスタ番号は 16 ビットで、上位、下位の順に送る [UD/managed_components/espressif__esp_lcd_touch_gt911/include/esp_lcd_touch_gt911.h:56-66（`lcd_cmd_bits = 16`）; M5G/lgfx/v1/touch/Touch_GT911.cpp:28]。

| レジスタ | 内容 | 根拠 |
|---|---|---|
| 0x8040 | コマンド。5 を書くとスリープ | esp_lcd_touch_gt911.c:27; Touch_GT911.cpp:127-137 |
| 0x8047 | 設定のバージョン | esp_lcd_touch_gt911.c:25,389 |
| 0x8056 | リフレッシュの設定（下位 4 ビット + 5 = 周期 ms） | Touch_GT911.cpp:73-77 |
| 0x8140 | 製品 ID（ASCII。IDF は 3 バイト読む） | esp_lcd_touch_gt911.c:26,388 |
| 0x814E | 状態。bit7 = データあり、bit4 = キーあり、[3:0] = 点の数 | esp_lcd_touch_gt911.c:219-247 |
| 0x814F〜 | 点ごとに 8 バイト: ID、X(L)、X(H)、Y(L)、Y(H)、サイズ(L)、サイズ(H)、予約 | esp_lcd_touch_gt911.c:265-280; Touch_GT911.cpp:197-206 |

- 読み方: 0x814E を 1 バイト読む。bit7 が 0 なら 0x814E に 0 を書いて終わる。1 なら点の数 n（1〜5。0 や 5 より大きいときは 0 を書いて捨てる）を見て、0x814F から 8 × n バイトを読み、**0x814E に 0 を書いて消す** [esp_lcd_touch_gt911.c:219-270]。
- M5GFX のコメント: 「GT911 は 0x814E に 0 を書くまで同じ値を返し続ける。前の読み出しから時間が空くと古い値が読めるので、一度捨ててから読み直す」[Touch_GT911.cpp:178-196]。128ms 以上空いたら、0 を書いてから 1ms おきに最大 24 回読み直す。
- 座標は 0〜719、0〜1279 で、パネルの画素と同じ向き（M5GFX の設定は `x_max = 719`、`y_max = 1279`、回転 0）[M5G/M5GFX.cpp:2316-2335]。BSP も変換なし [src/bsp_display.c:478-492]。
- **INT の扱いは実装ごとに違う**:
  - M5GFX: 初期化のあと `wakeup()` で INT を 58ms Low に出力し、High を 2ms 出してから、入力（プルアップ）に戻す [Touch_GT911.cpp:114-126]。読むときは、INT が Low のときだけ読むか、ポーリングする [:169-182]。
  - BSP: GT911 のときは **GPIO23 を出力 Low のままにして、INT を使わない**。コメントは「（ver 1 の）INT に 3V3 への抵抗があり、GT911 のタッチを邪魔するので、Low にしておく」[src/bsp_display.c:497-514]。
  - GT911 は、リセット解除のあと INT を一定時間 Low に保つ同期の手順を求める（推測。Goodix の資料は手元にない。M5GFX の 58ms Low と、IDF の「アドレス選択 → 1ms → RST 解除 → 10ms + 50ms」[esp_lcd_touch_gt911.c:112-133] が、その手順に当たると考えている）。
  - カーネル: M5GFX の `wakeup()` と同じ手順を 1 回やってから、ポーリングで読む（判断）。動かなければ BSP のように INT を Low に固定する。

### 6.2 ST712x 内蔵タッチ（ST7123、ST7121）
- I2C アドレスは 0x55。レジスタ番号は 16 ビットで、上位、下位の順 [esp_lcd_touch_st7123.h:31-46; M5G/lgfx/v1/platforms/esp32p4/Touch_ST7123.cpp:38-42]。ST7121 も同じドライバで読む [M5G↑ M5GFX.cpp:3354-3357; BSP src/bsp_display.c:520-525]。

| レジスタ | 内容 | 根拠 |
|---|---|---|
| 0x0000 | FW バージョン（1 バイト。ST7121 = 1、ST7123 = 3） | Touch_ST7123.cpp:29; §3.1 |
| 0x0005〜0x0008 | X 最大（H、L）、Y 最大（H、L） | Touch_ST7123.cpp:31-34 |
| 0x0009 | 最大の点の数 | Touch_ST7123.cpp:35 |
| 0x000C | FW リビジョン（4 バイト） | Touch_ST7123.cpp:30 |
| 0x0010 | 状態（adv_info）。bit2 with_prox、**bit3 with_coord（座標あり）**、[6:4] prox_status、bit7 rst_chip | Touch_ST7123.cpp:119-125; UD/managed_components/espressif__esp_lcd_touch_st7123/esp_lcd_touch_st7123.c:117-130 |
| 0x0014〜 | 点ごとに 7 バイト | 下の表 |

点の形式（7 バイト）[Touch_ST7123.cpp:108-118]:

| バイト | 内容 |
|---|---|
| 0 | [5:0] X の上位、bit6 予約、**bit7 valid** |
| 1 | X の下位 |
| 2 | Y の上位 |
| 3 | Y の下位 |
| 4 | 面積 |
| 5 | 強さ |
| 6 | 予約 |

- 読み方: 0x0010 を 1 バイト読む。bit3 が立っていれば、0x0009 で点の数（最大 10）を読み、0x0014 から 7 × 点の数を読む。valid が立っている点だけを使う。点の番号は配列の位置 [Touch_ST7123.cpp:126-150; esp_lcd_touch_st7123.c:130-148]。
- **状態を消す書き込みは、どの実装もしていない**。読むだけで次の報告に進むと考えられる（推測）。
- 初期化では、0x0000、0x000C（4 バイト）、0x0005（5 バイト）を読み、合計が 0 でなければ成功とする。M5GFX は最大 6 回試す [Touch_ST7123.cpp:44-73,75-104]。
- 座標はパネルの画素と同じ範囲（M5GFX の設定は GT911 と同じ 0〜719、0〜1279）[M5G/M5GFX.cpp:2316-2335]。`Touch_ST7123` の既定値は 0〜2047 だが、Tab5 では上書きしている [Touch_ST7123.hpp（M5G）:32-38]。範囲は 0x0005〜0x0008 で確かめられる。
- INT: BSP は立ち下がりエッジ（`levels.interrupt = 0` → `GPIO_INTR_NEGEDGE`）に設定しているが、コールバックは登録していない [src/bsp_display.c:478-486; esp_lcd_touch_st7123.c:62-70]。M5GFX は INT を見ずにポーリングする [Touch_ST7123.cpp:106-154]。

### 6.3 INT（GPIO23）を割り込みで使う場合
- 最初はポーリング（たとえば 60Hz）にする（判断）。両方の実装がポーリングで動いている。
- 割り込みにするときのレジスタ:
  - `IO_MUX_GPIO23`（0x500E_1060）: MCU_SEL = 1、FUN_IE = 1。
  - `GPIO_PIN23_REG`（0x500E_00D0 = 0x500E_0074 + 4 × 23）: `INT_TYPE`[9:7] = 2（立ち下がり）、`INT_ENA`[17:13] = 1（bit13。GPIO_INTR0 につなぐ。IDF の `GPIO_LL_INTR0_ENA` = BIT(0)）[HW1/gpio_reg.h:353,2205-2240; IDF/hal/esp32p4/include/hal/gpio_ll.h:45,145-147,213]。
  - 状態は `GPIO_STATUS_REG`（0x500E_0044）bit23。`GPIO_STATUS_W1TC`（0x500E_004C）で消す。割り込み 0 に絞った状態は `GPIO_INTR_0_REG`（0x500E_005C）[gpio_reg.h:205-292; gpio_ll.h:150-200]。
  - CPU への配線は `INTERRUPT_CORE0_GPIO_INT0_MAP_REG`（0x500D_6128。`DR_REG_INTR_BASE` = 0x500D_6000）[HW1/interrupt_core0_reg.h:905; reg_base.h:102]。
    - `interrupts.h` の `ETS_GPIO_INTR0_SOURCE` は数えると 75 番目だが、MAP レジスタのオフセット 0x128 は 74 × 4 になる [IDF/soc/esp32p4/include/soc/interrupts.h:18-94]。番号ではなく **MAP レジスタのアドレスで設定する**（判断）。
- GT911 の INT は、GT911 が出力する（推測）。上の 6.1 のとおり、BSP は INT を出力 Low のままにするので、BSP のやり方では割り込みに使えない。

## 7. 推奨の立ち上げ順（カーネル）
```
 0. 前提: 360MHz、PSRAM（フレームバッファ 720×1280×2 = 1,843,200 バイト）、SYSTIMER の遅延関数
 1. I2C1 初期化（§1.4）。100kHz
 2. エクスパンダ #1（0x43）が 0x01 に応答するか見る（#2 も見るだけ）
 3. #1: OUT=0x44 → DIR=0x7F → OUT_H_IM=0x00 → PULL_SEL=0x7F → PULL_EN=0x7F   … LCD_RST と TP_RST を Low
    GPIO23 を出力 High
    10ms 待つ
 4. #1: DIR=0x6F（LCD_RST 解除）→ OUT=0x64（TP_RST 解除）
    GPIO23 を入力に
    100ms 待つ                                                           … M5G↑ と同じ
 5. 判別（最大 600ms）: 10ms おきに
      0x55 の 0x0000 を読む → 1 = ST7121 / 3 = ST7123
      読めなければ 0x14 のアドレスだけ送って ACK → ILI9881C+GT911（だめなら 0x5D も）
 6. DSI（別担当）: LDO VO3 = 2500mV → PHY とホスト（2 lane、lane 速度は §4.2。ST7121 は 900〜965）
    80ms 待つ                                                            … M5GFX と同じ
    （任意）DSI の ID で確認: ILI は FF 98 81 01 → reg 00/01 = 98 81 → FF 98 81 00。ST7123 は reg F4 = 71 23
 7. パネル初期化（§4.3〜4.5）
      ILI9881C: リスト 0 → リスト 1（SLPOUT）→ 120ms → リスト 2 → リスト 3（DISPON まで）
      ST7123:   リスト 0 → SLPOUT → 120ms → DISPON → TEON(00)                 （判断。M5GFX は逆順）
      ST7121:   リスト 0 → SLPOUT → 80ms → DISPON → 800ms → TEON(00)
      共通:     MADCTL=00、COLMOD=55
 8. DPI の映像を開始（別担当）。タイミングは §4.2。最初のフレームを書く
 9. バックライト: GPIO22 を High
10. タッチ
      GT911: INT を Low 58ms → High 2ms → 入力。0x814E をポーリングし、読んだら 0 を書く
      ST712x: 0x0010 をポーリング。bit3 なら 0x0014 から 7 バイト × 点の数
      最初は 60Hz 程度のポーリング。400kHz に上げる
```
- 待ち時間の根拠: 3 の 10ms は M5GFX [M5G/M5GFX.cpp:2221]、4 の 100ms は M5G↑ [M5GFX.cpp:3244]、5 の 600ms は M5G↑ [:3270,3299]、6 の 80ms は M5GFX [M5G/M5GFX.cpp:2249]、7 の遅延は §4。BSP は 4 と 5 の間に 500ms 待つ [src/bsp_display.c:154]。最初は BSP に合わせて余裕をとり、動いてから縮める（判断）。
- 全体で、エクスパンダの解除から最初の表示まで約 0.4〜1.2 秒（ST7121 は DISPON 後の 800ms があるので長い）（推測）。
- 何度実行しても同じ結果になるように、3 で必ず Low を作ってから解除する（判断）。SW リセットで再起動したときも、パネルが確実にリセットされる。
- 構造化ログ（doc/12）: 判別結果（エクスパンダの 0x01 の値、ST の FW バージョン、GT911 の有無、DSI の ID）と各段の所要時間を event として出す（判断）。

## 8. 未確認事項
- **ST7121 の DSI の ID**（reg 0xF4 などの値）。M5G↑ は DSI の ID で ST7121 と ST7123 を区別していない。手元の実機がどのロットかも不明（doc/06 §7）。
- **ST7121 の lane 速度**: M5GFX は 900Mbps、BSP は 965Mbps。どちらが正しいか（両方動くのか）は不明。
- **LCD_RST を「入力 + プルアップ」で解除する理由**（BSP と M5G↑ が同じことをしている）。パネル側にもプルアップがあるのか、オープンドレインでなければならない理由があるのかは、回路図で確かめていない。
- **ST712x のタッチが動くのに LCD_RST の解除が必要か**（同じチップなので、TP_RST だけでは動かない可能性）。§7 では両方解除しているので、M6 の手順には影響しない。
- **GT911 の INT の扱い**: M5GFX（Low パルスのあと入力）と BSP（Low に固定）のどちらが正しいか。GPIO23 のプルアップ（R92）の値と接続先も、回路図の画像で確かめる必要がある。
- **#2（0x44）を書かないときの充電**: 電源投入時は CHG_EN（bit7）がプルダウンで Low。IP2326 の充電がそれで止まるかどうか（極性）。M6 で #2 を M5GFX と同じ値にするか決める。
- **ME2212 の FB 電圧と、EN の PWM 周波数の上限**（LED の電流と調光）。データシートが手元にない。M5GFX は 44.1kHz、BSP は 5kHz で動いている。
- **ILI9881C のコマンド 0xFE の意味**（3 つの出典すべてにあるので、そのまま送る）。
- **ST7123 の SLPOUT と DISPON の順番**: M5GFX と BSP が逆。どちらでも表示されるかは実機で確かめる。
- **ST712x のタッチの状態の消し方**: どの実装も消していない。報告が積み重ならないかは実機で見る。
- **GT911 の 0x5D を使う個体があるか**（INT のプルアップがあるなら、ないはず）。
- `interrupts.h` の `ETS_GPIO_INTR0_SOURCE` の番号（75）と MAP レジスタの位置（74）の食い違い。割り込みを使うときに確かめる。
- LEDC のガンマ RAM の書き方。PWM を入れるときに `ledc_reg.h` を読み直す。

## 9. 参考（このノートで新しく使ったもの）
- M5Stack 公式デモ（ローカル）: `~/repos/M5Tab5-UserDemo/platforms/tab5/components/m5stack_tab5/{m5stack_tab5.c,esp_lcd_st7123.c,include/bsp/ili9881_init_data.c}`、`managed_components/espressif__esp_lcd_{ili9881c,touch_gt911,touch_st7123}/`
- upstream M5GFX: https://github.com/m5stack/M5GFX （`src/M5GFX.cpp`、`src/lgfx/v1/platforms/esp32p4/{Panel_ST7121.hpp,Panel_ST7123.hpp,Panel_ILI9881C.hpp,Panel_DSI.inl,Panel_DSI.hpp,Touch_ST7123.inl}`、`src/lgfx/v1/touch/Touch_GT911.inl`）
- espressif/esp-bsp: `bsp/m5stack_tab5/{src/bsp_display.c,src/bsp_feature_en.c,src/bsp_io_expander.c,priv_include/disp_init_data*.h,include/bsp/*.h}`、`components/io_expander/esp_io_expander_pi4ioe5v6408/`
- espressif/esp-iot-solution: `components/display/lcd/esp_lcd_st7123/esp_lcd_st7123.c`、`components/display/lcd_touch/esp_lcd_touch_st7123/esp_lcd_touch_st7123.c`（UD の版と、`#include "driver/i2c.h"` の 1 行以外同じ）
- PI4IOE5V6408 データシート: https://www.diodes.com/assets/Datasheets/PI4IOE5V6408.pdf
- ESP-IDF 6.0: `esp_hal_i2c/esp32p4/include/hal/i2c_ll.h`、`esp_hal_i2c/i2c_hal.c`、`esp_driver_i2c/{i2c_master.c,i2c_common.c}`、`hal/esp32p4/include/hal/{gpio_ll.h,ledc_ll.h}`、`esp_lcd/dsi/esp_lcd_panel_io_dbi.c`、`soc/esp32p4/register/hw_ver1/soc/{i2c_reg.h,gpio_reg.h,io_mux_reg.h,hp_sys_clkrst_reg.h,ledc_reg.h,interrupt_core0_reg.h,reg_base.h}`
