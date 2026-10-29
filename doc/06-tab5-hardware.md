# 06. M5Stack Tab5 ハードウェア構成と OS が扱うべきデバイス

調査日: 2026-10-29。対象: M5Stack Tab5 (SKU C145/K145)。
出典表記の略号:

- `TCC/` = `~/repos/tab5_claude_client/`（同作者の ESP-IDF 6.0 プロジェクト、読み取りのみ）
- `M5U/` = `TCC/components/M5Unified/src/`、`M5G/` = `TCC/components/M5GFX/src/`（どちらもローカル vendored 版。M5GFX は 0.2.20）
- `IDF/` = `~/esp-idf/6.0/components/`
- `SCH` = 公式回路図 PDF（https://m5stack-doc.oss-cn-shenzhen.aliyuncs.com/1132/Tab5_Schematics_PDF.pdf 、作成日 2025-06-09）。pdftotext で読んだテキスト
- `BLK` = 公式ブロック図 PDF（https://m5stack-doc.oss-cn-shenzhen.aliyuncs.com/1132/Tab5_Overall_Design_Block_Diagram.pdf ）
- `DOC` = M5Stack 公式ページ https://docs.m5stack.com/en/core/Tab5
- `BSP` = espressif/esp-bsp `bsp/m5stack_tab5`（v1.3.2）https://github.com/espressif/esp-bsp/tree/master/bsp/m5stack_tab5
- `NUTTX` = Apache NuttX の Tab5 ボードページ https://nuttx.apache.org/docs/latest/platforms/risc-v/esp32p4/boards/esp32p4-tab5/index.html
- `ESPHAL` = esp-rs/esp-hal（main は 2026-10-28 に push。リリース esp-hal-v1.2.0 は 2026-09-03）

---

## 0. 結論

### 0.1 最重要の発見: Tab5 の ESP32-P4 は rev v1.0。esp-hal は対象外

- 手元の Tab5 実機ログでは `chip revision: v1.0`、ROM は `esp32p4-eco2-20240710` だった（`TCC/logs/boot.log:2,15`）。プロジェクト側でも `CONFIG_ESP32P4_SELECTS_REV_LESS_V3=y` と `REV_MIN_100` を指定している（`TCC/sdkconfig.defaults.esp32p4:7-10`）。NuttX のページにも「Tab5 は ESP32-P4 revision v1.0 で出荷」とある（NUTTX）。
- **esp-hal は「Revisions below v3.0 are not supported」と明記している**（ESPHAL `esp-hal/README.md` の ESP32-P4 行。v1.2.0 と main の両方。`esp-metadata/devices/esp32p4/soc.toml` の `support_note` にも同じ記述）。PAC の `esp32p4` crate（0.4.0、2026-09-15）も esp-hal と同じ系統の SVD から作られている。このため v3 のレジスタマップを前提にしている可能性が高い（推測）。
- ESP-IDF はレジスタ定義を `IDF/soc/esp32p4/register/hw_ver1/` と `hw_ver3/` に分けて持っている。`diff -rq hw_ver1 hw_ver3` は 207 件の相違を報告する（hw_ver1/soc は 191 ファイル）。相違は MSPI/キャッシュ/PMU/DMA/MIPI などに広がっている。一方で、USB Serial/JTAG・LP_WDT・TIMG・reg_base は名前とコピーライトの違いだけだった（`diff hw_ver1/soc/usb_serial_jtag_reg.h hw_ver3/...` で確認）。
- **方針**: esp-hal と PAC はそのまま使わない。**ESP-IDF の `hw_ver1` ヘッダと HAL（`*_ll.h`）を一次資料として自前のレジスタ定義を書く**。esp-hal のコードはロジックの参考に留める。簡単な周辺（USJ・WDT・GPIO・I2C・UART）は v1/v3 で差がほとんどないので、esp-hal の実装をそのまま参考にできる。PSRAM・キャッシュ・DSI は差分があるので IDF の v1 パスを読む必要がある（推測）。NuttX は rev v1.0 の Tab5 で DSI・PSRAM・タッチを動かしており、C 実装の参考になる（NUTTX）。

### 0.2 OS が扱うデバイスの優先順位

| 順位 | デバイス | 理由 |
|---|---|---|
| 1 | **USB Serial/JTAG (USJ) コンソール** | 初期化なしで FIFO に書くだけで出力できる（§3）。最初のマイルストーン「U モードアプリが ecall で UART 出力」の出力先になる。JTAG デバッグも同じ USB-C ケーブルで済む |
| 2 | **ウォッチドッグ（RWDT）停止 / SYSTIMER** | IDF 2nd stage bootloader から起動すると RWDT が 9 秒で有効になっている（§2）。放置するとリセットされる。タイマ割り込みは U モードのプリエンプションに必須 |
| 3 | **GPIO/IO_MUX + I2C マスタ（GPIO31/32）** | 内部 I2C バスには IO エクスパンダ 2 個、RTC、INA226、タッチ、コーデック、IMU、カメラ SCCB がすべて載っている。これがないと LCD 電源・リセット、Wi-Fi 電源、USB-A 5V を制御できない |
| 4 | **PI4IOE5V6408 ×2（0x43/0x44）** | LCD/タッチ/カメラのリセット・イネーブル、スピーカ、外部 5V、Wi-Fi(C6) 電源、USB-A 5V、充電制御を担う（§1.3） |
| 5 | **PSRAM（32MB、HEX/X16、200MHz）** | 1280×720×RGB565 のフレームバッファは約 1.76MB で、内部 RAM（L2MEM 約 768KB）に入らない（`TCC/sdkconfig.defaults.esp32p4:12-13`）。LCD より先に必要 |
| 6 | **MIPI-DSI LCD + バックライト（GPIO22）** | 画面を出すまでの最大の山場。パネルが 3 種類ある（§5） |
| 7 | タッチ（ST7123/GT911）、RTC（RX8130CE）、INA226 | I2C デバイスなので軽い。UI と時計と電池残量に使う |
| 8 | microSD（SDMMC slot0） | アプリやデータのロード元になる。UART 経由ロードの次に欲しい |
| 9 | USB-A ホスト（OTG HS）、Wi-Fi（C6 + SDIO + esp_hosted）、オーディオ、カメラ、RS485、IMU | どれも重いか、後回しで困らない |

---

## 1. ブロック構成とピンアサイン

### 1.1 SoC・メモリ

| 項目 | 値 | 出典 |
|---|---|---|
| SoC | ESP32-P4NRW32（HP RISC-V 2 コア 360MHz + LP 1 コア 40MHz） | DOC |
| リビジョン | v1.0（efuse block v0.3）、ROM `esp32p4-eco2-20240710` | `TCC/logs/boot.log:2,15-16` |
| CPU クロック（IDF アプリ設定時） | 360 MHz | `TCC/logs/boot.log:58` |
| Flash | 16MB。IDF 2nd stage では DIO・80MHz で起動 | `TCC/logs/boot.log:17-19`、`TCC/sdkconfig.defaults.esp32p4:4-5` |
| PSRAM | 32MB、AP Memory（vendor 0x0d）、256Mbit、**X16（HEX）モード**、200MHz | `TCC/logs/boot.log:39-54`、`TCC/sdkconfig.defaults.esp32p4:14-16` |
| 内部 RAM（IDF ヒープとして使える範囲） | 0x4FF40000 から 384KB ほか、RETENT_RAM 124KB、TCM 7KB、RTCRAM 31KB | `TCC/logs/boot.log:69-74` |
| PSRAM の仮想アドレス | `extmem_origin = 0x4800_0000` | ESPHAL `esp-metadata/devices/esp32p4/soc.toml` [device.psram] |
| 2nd stage bootloader のオフセット | 0x2000（ESP32 系の 0x0/0x1000 とは違う） | `TCC/sdkconfig:1096`、NUTTX（`write_flash 0x2000`） |

公式ページは PSRAM を「32 MB Octal」と書いている（DOC）。実機ログでは HEX（X16）だった。ドライバは HEX モードで書く。

### 1.2 内部 I2C バス（SYS_I2C: SDA=GPIO31, SCL=GPIO32）

M5Unified はこのバスを I2C_NUM_1 として使う（`M5U/M5Unified.cpp:105`、`M5G/M5GFX.cpp:35`）。

| デバイス | 7bit アドレス | 用途 | 出典 |
|---|---|---|---|
| PI4IOE5V6408 #1 | 0x43 | IO エクスパンダ（LCD/TP/CAM のリセット、SPK_EN、EXT5V_EN、RF スイッチ、HP_DET） | `M5U/M5Unified.cpp:1538-1544`、`M5G/M5GFX.cpp:152` |
| PI4IOE5V6408 #2 | 0x44 | IO エクスパンダ（WLAN_PWR_EN、USB5V_EN、充電制御、PWROFF パルス） | 同上 |
| INA226 | 0x41 | バッテリの電圧と電流（シャント 5mΩ） | `M5U/utility/Power_Class.hpp:221`、`Power_Class.cpp:101-109`、DOC |
| RX8130CE | 0x32 | RTC | `M5U/utility/rtc/RX8130_Class.hpp:14`、`RTC_Class.cpp:33-34`、DOC |
| ES8388 | 0x10 | DAC（スピーカとヘッドホン） | `M5U/M5Unified.cpp:375`、DOC |
| ES7210 | 0x40 | マイク ADC（AEC） | `M5U/M5Unified.cpp:372`、DOC |
| BMI270 | 0x68（DOC）。M5Unified は 0x68 と 0x69 の両方を試す | IMU | DOC、`M5U/utility/IMU_Class.cpp:76-78` |
| GT911（初期ロット） | 0x14（INT=GPIO23 を High にして選択）または 0x5D | タッチ | `M5G/M5GFX.cpp:1579,2186` |
| ST7123/ST7121 内蔵タッチ（後期ロット） | 0x55 | タッチ（FW バージョン reg で ST7121=1 / ST7123=3 を判別） | `M5G/lgfx/v1/platforms/esp32p4/Touch_ST7123.hpp:30`、BSP `src/bsp_display.c:140-191` |
| SC2356 カメラ SCCB | 未確認（0x36 前後と推測） | カメラ | DOC（バスのみ記載） |

外部 I2C（Port.A、HY2.0-4P）は SCL=GPIO54、SDA=GPIO53（`M5U/M5Unified.cpp:105`、DOC）。

### 1.3 IO エクスパンダのビット割り当て（電源シーケンスの要）

PI4IOE5V6408 のレジスタ: 0x01=ChipID/Reset、0x03=IO_DIR（1=出力）、0x05=OUT_SET、0x07=OUT_H_IM（High-Z）、0x09=IN_DEF_STA、0x0B=PULL_EN、0x0D=PULL_SEL（1=Pull-up）、0x11=INT_MASK（`M5G/M5GFX.cpp:2195-2219` のコメント表記に従う）。

**#1 (0x43)**（`M5U/utility/Power_Class.cpp:67-81`、BSP `include/bsp/m5stack_tab5.h:88-117`）

| bit | M5Unified の名前 | BSP の名前 | 備考 |
|---|---|---|---|
| 0 | RF_PTH_L_INT_H_EXT | – | アンテナ選択: L=内蔵 / H=外部 MMCX（DOC） |
| 1 | SPK_EN | BSP_SPEAKER_EN | NS4150B アンプのイネーブル |
| 2 | EXT5V_EN | – | M5-Bus・Ext.Port・Grove の 5V（`Power_Class.cpp:597-608`） |
| 3 | NC | – | |
| 4 | LCD_RST | BSP_LCD_EN | BSP は「入力 + プルアップ（オープンドレイン的に High）」で有効化する（BSP `src/bsp_feature_en.c`） |
| 5 | TP_RST | BSP_TOUCH_EN | |
| 6 | CAM_RST | BSP_CAMERA_EN | |
| 7 | HP_DET（入力） | – | ヘッドホン検出 |

**#2 (0x44)**（`M5U/utility/Power_Class.cpp:83-97`）

| bit | 名前 | 備考 |
|---|---|---|
| 0 | WLAN_PWR_EN | ESP32-C6 の電源（`TCC/main/wifi_setup.cpp:85-104`、BSP BSP_WIFI_EN） |
| 3 | USB5V_EN | USB-A の VBUS（`Power_Class.cpp:597-608`、BSP BSP_USB_EN） |
| 4 | PWROFF_PULSE | 50ms 周期で 10 回トグルすると電源断（`Power_Class.cpp:1066-1072`） |
| 5 | nCHG_QC_EN | IP2326 の QC 充電（Low=有効）（`Power_Class.cpp:1673-1700`） |
| 6 | CHG_STAT（入力） | 充電中の表示（`Power_Class.cpp:1856-1858`） |
| 7 | CHG_EN | 充電の許可 |

M5GFX の LCD 立ち上げ手順（`M5G/M5GFX.cpp:2195-2219`）は次のとおり。
1. #1 に IO_DIR=0x7F、OUT_SET=0b01000110 を書く（LCD_RST/TP_RST は Low）。
2. #2 に IO_DIR=0xB9、OUT_H_IM=0x06、PULL_SEL=0xB9、PULL_EN=0xF9、IN_DEF_STA=0x40、INT_MASK=0xBF、OUT_SET=0b10001001 を書く（CHG_EN、USB5V_EN、WLAN_PWR_EN が High）。
3. 10ms 待ってから #1 の OUT_SET を 0b01110110 にする（LCD_RST と TP_RST を解除）。

> 注意（推測）: `TCC/main/wifi_setup.cpp:60-82` は 0x09 を「PullSel」、0x0F を「InDefSta」と命名して値を書いている。これは M5GFX の命名（0x09=IN_DEF_STA、0x0D=PULL_SEL）と食い違う。Rust で実装するときは PI4IOE5V6408 のデータシートで確認すること。

### 1.4 LCD（MIPI-DSI）

| 項目 | 値 | 出典 |
|---|---|---|
| パネル | 5" IPS、720×1280（縦長が native）、RGB565 | BSP `include/bsp/display.h:30-40`、`M5G/M5GFX.cpp:2298-2303` |
| DSI | 2 lane。1000Mbps（BSP、ILI9881C）/ 965Mbps（BSP、ST712x）/ 1040Mbps（M5GFX） | BSP `display.h:41-42`、`src/bsp_display.c:45`、`M5G/M5GFX.cpp:2242` |
| D-PHY 電源 | **内蔵 LDO チャネル 3（LDO_VO3）を 2500mV に設定**（VDD_MIPI_DPHY） | BSP `display.h:44-45`、`M5G/M5GFX.cpp:2243-2244` |
| DPI クロック | 60/70MHz（BSP）、80MHz（M5GFX） | BSP `bsp_display.c:245,266,287`、`M5G/M5GFX.cpp:2276,2290` |
| バックライト | GPIO22（LCD_BL）。ME2212 昇圧で LEDA を駆動。M5GFX は PWM 44.1kHz | `M5G/M5GFX.cpp:2339`、SCH net `LCD_BL_GPIO22`、NUTTX |
| リセット | GPIO の RST 線はない。IO エクスパンダ #1 の bit4 で行う | BSP `m5stack_tab5.h:95-97`、NUTTX |
| パネル判別 | M5GFX は DSI で reg 0xF4 を読み、`71 23` なら ST7123 と判定する。それ以外は page1 の ID が `98 81` なら ILI9881C | `M5G/M5GFX.cpp:2247-2270` |

### 1.5 オーディオ（I2S0）

MCLK=GPIO30、BCLK=GPIO27、LRCK=GPIO29、DOUT(→ES8388)=GPIO26、DIN(←ES7210)=GPIO28（`M5U/M5Unified.cpp:1860-1871,2031-2041`、BSP `m5stack_tab5.h:82-88`、DOC）。アンプは NS4150B 1W で、IO エクスパンダ #1 の bit1 で有効化する。ESP-IDF 6.0 上の M5Unified のオーディオは「コンパイルは通るが動作は未検証」（`TCC/M5_IDF6_PATCHES.md:71-76`）。

### 1.6 カメラ

SC2356（2MP、MIPI-CSI 2 lane）。MCLK=GPIO36、SCCB は内部 I2C、リセットは IO エクスパンダ #1 の bit6（DOC、SCH net `CAM_MCLK_GPIO36`）。

### 1.7 ESP32-C6（Wi-Fi 6 / BLE / 802.15.4）

| P4 側 | C6 側 | 出典 |
|---|---|---|
| GPIO11/10/9/8 | SDIO D0/D1/D2/D3 | DOC、`TCC/sdkconfig.defaults.esp32p4:49-54` |
| GPIO13 / GPIO12 | SDIO CMD / CLK | 同上 |
| GPIO15 | C6 CHIP_EN（High=動作） | DOC、`TCC/M5_IDF6_PATCHES.md:78-116` |
| GPIO14 | C6 IO2 | DOC |
| IO エクスパンダ #2 bit0 | WLAN_PWR_EN | §1.3 |

- P4 では SDMMC の slot1（GPIO マトリクス経由）、4bit、20MHz で使っている。40MHz では CMD5 がタイムアウトした（`TCC/sdkconfig.defaults.esp32p4:56-60`）。
- 出荷時の C6 ファームウェアは M5Stack 版 `ESP32C6-WiFi-SDIO-Interface-V1.4.1`（esp_hosted 1.4.x）。ホストが esp_hosted 2.x の場合は C6 の書き換えが必要（`TCC/CLAUDE.md` Wi-Fi 節、`TCC/slave_c6_fw/README.md:1-10`）。書き換えはカバーを開けずに SDIO 経由でできる（`TCC/c6_updater/`）。
- 起動手順: WLAN_PWR_EN を Off にして 300ms 待ち、On にして 1500ms 待ってから SDIO を初期化する（`TCC/main/wifi_setup.cpp:107-121`）。

### 1.8 USB

| ポート | 接続先 | 出典 |
|---|---|---|
| **USB-C** | P4 の GPIO24/25（`USB1P1_0-/+`、FSLS PHY 0）＝ net `USB_DEVICE_DM/DP`。既定では USJ（USB Serial/JTAG）に繋がる | SCH（pdftotext 行 528-538、1533-1552）、BLK「USBC — USB1_D+/D-」 |
| **USB-A**（ホスト） | P4 の専用 HS PHY（`USB2_OTG_D+/-`、ピン 49/50）＝ net `USB_HOST_DP/DM`。VBUS は IO エクスパンダ #2 の bit3 | SCH（行 519-528、1862-1895）、BLK「USBA — USB2_OTG_D+/D-」 |
| M5-Bus / 拡張 MX1.25-4P | `EXT_USB_DP/DM`（USB-A と同じ HS ホスト系統に見える。推測） | SCH 行 1880-1893 |

- 実機では USB-C に `303a:1001` 系の CDC-ACM（`/dev/ttyACM*`）が現れる。ROM のリセット理由は `CHIP_USB_UART_RESET` で、ROM の出力もこのポートに出る（`TCC/logs/boot.log:1-4`）。
- 公式は USB-C を「USB 2.0 OTG」と説明している（DOC）。PHY0 を OTG FS（USB_WRAP）側に切り替えれば USB-C をデバイスとして使える（`IDF/hal/esp32p4/include/hal/usb_serial_jtag_ll.h:216-240`、推測）。ただしその間 USJ コンソールは使えなくなる。
- BSP の `BSP_USB_POS/NEG = GPIO20/19` は RS485_TX と SPI MISO に重なる。誤記と思われる（推測）。

### 1.9 microSD（SDMMC slot0、IO_MUX 固定ピン）

D0=GPIO39、D1=GPIO40、D2=GPIO41、D3=GPIO42、CLK=GPIO43、CMD=GPIO44（BSP `m5stack_tab5.h:124-134`、SCH net `SDIO1_*` と GPIO39-44 の対応、行 255-295）。SPI モードの場合は CLK=43、MOSI=44、MISO=39、CS=42（`M5U/M5Unified.cpp:173`）。slot0 は IO_MUX 固定パッドを使う（ESPHAL soc.toml [device.sdmmc] のコメント）。

### 1.10 その他の I/O

| 機能 | ピン | 出典 |
|---|---|---|
| RS485（SIT3088） | TX=GPIO20、RX=GPIO21、DIR=GPIO34。6〜24V、120Ω 終端スイッチ付き | DOC、SCH net `RS485_*_GPIO2x/34` |
| UART0（ROM 既定） | TX=GPIO37、RX=GPIO38。**M5-Bus の 14/13 番ピンに出ている** | `IDF/soc/esp32p4/include/soc/uart_pins.h:12-13`、DOC M5-Bus 表、SCH net `U0TXD_GPIO37` |
| BOOT | GPIO35（`BOOT_GPIO35`。M5-Bus 24 番ピンにも出ている） | SCH、DOC |
| Port.A（Grove） | G53/G54 | DOC |
| Port.B/C 相当 | B: G17/G52、C: G7/G6（`M5U/M5Unified.cpp:134`） | |
| M5-Bus SPI | MOSI=G18、MISO=G19、SCK=G5 | DOC |
| JTAG 端子（MTCK/MTDI/MTMS/MTDO） | GPIO2/3/4/5。M5-Bus の汎用 GPIO と兼用 | `IDF/soc/esp32p4/register/hw_ver1/soc/io_mux_reg.h:194-198`、DOC |
| ボード判別 | GPIO32 が常に High（I2C プルアップ） | `M5U/M5Unified.cpp:1487` |
| バッテリ | NP-F550 7.4V 2000mAh（2S）。充電 IC は IP2326、昇降圧は MP4560。6.0V で遮断 | DOC |
| ブザー | なし（スピーカのみ） | DOC |

---

## 2. ブート直後に OS が初期化すべきもの

前提: ESP-IDF の 2nd stage bootloader（0x2000 に配置）から Rust カーネルの app イメージを起動する。espflash/esptool 互換の app イメージ形式を使う。

1. **RWDT（LP_WDT）**: ROM は RWDT と MWDT0 を flashboot 保護で有効にする。bootloader はその flashboot を解除したうえで、`CONFIG_BOOTLOADER_WDT_ENABLE=y`（既定 9000ms）なら **RWDT を改めて有効にして app に渡す**（`IDF/bootloader_support/src/bootloader_init.c:73-96`、`TCC/sdkconfig:1164-1168`）。MWDT0 の flashboot は解除済み（同 98 行以降）。
   → カーネルは最初に LP_WDT を止めるかフィードする。手順は `LP_WDT_WPROTECT_REG`（`DR_REG_LP_WDT_BASE+0x18`、base は 0x50116000）に `0x50D83AA1` を書いて解除し、CONFIG0 の EN を落とす（`IDF/soc/esp32p4/register/hw_ver1/soc/lp_wdt_reg.h:18,160`、`IDF/esp_hal_wdt/esp32p4/include/hal/lpwdt_ll.h:25-27`、`reg_base.h:11,136`）。TIMG0/1 の MWDT は `TIMG_WDTCONFIG0/WPROTECT`（base 0x500C2000/0x500C3000、+0x48/+0x64）で念のため止める（`timer_group_reg.h:319,483`）。
2. **SWD（Super WDT）**: bootloader が `LP_WDT_SWD_AUTO_FEED_EN` を立てている（`IDF/bootloader_support/src/esp32p4/bootloader_esp32p4.c:89-93,134`）。OS 側の対応は不要。
3. **クロック**: bootloader が設定したまま始めても USJ 出力はできる（推測）。IDF アプリは 360MHz に上げている（`TCC/logs/boot.log:58`）。最初のマイルストーンでは触らず、後で `IDF/esp_hw_support` の P4 用 `rtc_clk` を移植する。
4. **キャッシュと Flash の MMU**: bootloader が app セグメントの XIP マッピングを済ませて渡す（`TCC/logs/boot.log:30-37` の segment map）。
5. **PSRAM**: ログでは bootloader の終了後、`cpu_start` より前に `hex_psram`/`esp_psram` の初期化と MSPI タイミングチューニング（約 170ms）が出ている（`TCC/logs/boot.log:39-56`）。つまり **app 側の責務**であり、Rust カーネルが自前で行う必要がある（推測、ログの順序による）。IDF の実装は `IDF/esp_psram/device/esp_psram_impl_ap_hex.c`（512 行）と `IDF/esp_hw_support/mspi_timing_tuning/port/esp32p4/`。
6. **I2C → IO エクスパンダ**: LCD・タッチ・Wi-Fi・USB-A を使う前に §1.3 の手順で設定する。コンソール出力だけなら不要。
7. **LDO3 = 2.5V**: DSI を使う前に D-PHY 用の内蔵 LDO を有効にする（§1.4）。

最初のマイルストーン（USJ 出力）に必要なのは 1 と 2 だけ（2 は確認のみ）。

---

## 3. コンソール / デバッグ経路

### 3.1 推奨: USB Serial/JTAG の FIFO 直書き

- ベースアドレスは `DR_REG_USB_SERIAL_JTAG_BASE = 0x500C0000 + 0x12000 = 0x500D2000`（`IDF/soc/esp32p4/register/hw_ver1/soc/reg_base.h:10,99,181`）。
- `EP1_REG`（+0x0）の下位 8bit に 1 バイトずつ書く。`EP1_CONF_REG`（+0x4）の bit1 `SERIAL_IN_EP_DATA_FREE` が 1 のときだけ書き込める。最後に bit0 `WR_DONE` に 1 を書いてフラッシュする（`IDF/soc/esp32p4/register/hw_ver1/soc/usb_serial_jtag_reg.h:17,33-53`、`IDF/hal/esp32p4/include/hal/usb_serial_jtag_ll.h:137-185`）。受信は bit2 `SERIAL_OUT_EP_DATA_AVAIL` を見て EP1 を読む（同 118-155）。
- ROM が同じ USJ にブートログを出している（`TCC/logs/boot.log:2-4`）。IDF 既定のコンソールも USJ になっている（`TCC/sdkconfig.defaults.esp32p4:20-21`）。NuttX も Tab5 の NSH コンソールに USJ を使っている（NUTTX）。そのため **PHY 選択やクロック設定を追加しなくても、書くだけで出る** と見込まれる（推測。ROM と bootloader の状態を引き継ぐ前提）。
- 注意点:
  - ホストが CDC ポートを開いていないと FIFO が空かず、`DATA_FREE` 待ちで止まる。esp-println と同様にタイムアウトしたら破棄する実装にする。
  - USB の再列挙（カーネル側から PHY をリセットするなど）をすると、ホスト側の `/dev/ttyACM*` が付け直しになる。
  - U モードから直接触らせない。カーネルの ecall ハンドラで書く。PMP では 0x500D2000 の 4KB を U モードに対して無効のままにする。
- ホスト側では `espflash monitor` / `idf.py monitor` / `picocom /dev/ttyACM0` で読める。

### 3.2 代替: UART0（GPIO37/38、M5-Bus 13/14 番ピン）

外付けの USB-UART アダプタが必要になる。USJ が USB の都合で使えないとき（USB-C を OTG デバイスにする場合など）の予備として使う。NuttX の `lvgl_demo` 設定もコンソールを UART0 に出している（NUTTX）。ボーレートの設定とクロックの確認が必要なので、USJ より手間は少し多い。

### 3.3 JTAG デバッグ

- USB-C の USJ は JTAG ブリッジ機能を持つ（`usb_serial_jtag_ll.h:190-198`）。手元には OpenOCD-esp32 の `board/esp32p4-builtin.cfg` がある（`~/.espressif/tools/openocd-esp32/v0.12.0-esp32-20251215/.../board/esp32p4-builtin.cfg`）。
- probe-rs の master には `probe-rs-espressif/src/sequences/esp32p4.rs` がある。ただし P4 のリセット失敗 issue（#4357、2026-09-16 クローズ）があった。rev v1.0 で動くかは未確認。
- 外部 JTAG 端子（GPIO2-5）は M5-Bus の汎用ピンと兼用。

---

## 4. デバイス一覧表

難易度は ★1（易）〜★5（難）で、Rust ベアメタルで自前実装する前提の主観的な見積もり（推測）。

| デバイス | 接続 | ピン / アドレス | 既存ドライバ（C） | 既存ドライバ（Rust） | Rust 移植難易度 | 優先度 |
|---|---|---|---|---|---|---|
| USB Serial/JTAG | 内蔵、USB-C（GPIO24/25） | 0x500D2000 | IDF `usb_serial_jtag_ll.h` | esp-hal（P4 は ⚒ partial、v3 のみ）、esp-println | ★1 | 1 |
| LP_WDT / TIMG WDT | 内蔵 | 0x50116000 / 0x500C2000, 0x500C3000 | IDF `lpwdt_ll.h`、`mwdt_ll.h` | esp-hal（v3 のみ） | ★1 | 1 |
| SYSTIMER / CLIC 割り込み | 内蔵 | – | IDF | esp-hal（⚒） | ★2〜3 | 2 |
| GPIO / IO_MUX | 内蔵 | – | IDF | esp-hal（✔、v3 のみ） | ★2 | 2 |
| I2C マスタ（内部バス） | GPIO31 SDA / 32 SCL | – | IDF i2c_master、M5Unified I2C_Class | esp-hal（✔、v3 のみ） | ★2 | 2 |
| PI4IOE5V6408 ×2 | I2C | 0x43 / 0x44 | M5Unified PI4IOE5V6408_Class、esp_io_expander_pi4ioe5v6408 | crates.io になし | ★1 | 2 |
| PSRAM（AP HEX 32MB） | MSPI（専用ピン） | 0x4800_0000 | IDF esp_psram + mspi_timing_tuning | esp-hal（⚒、v3 のみ） | ★4 | 3 |
| MIPI-DSI ホスト + DPI | 専用 D-PHY（2 lane）+ LDO3 | – | IDF esp_lcd mipi_dsi（約 1,085 行 + LL 約 560 行）、M5GFX Bus_DSI/Panel_DSI | esp-hal（MIPI-DSI ⚒、v3 のみ） | ★4〜5 | 3 |
| LCD パネル ILI9881C / ST7123 / ST7121 | DSI | – | esp_lcd_ili9881c、esp_lcd_st7123、M5GFX Panel_*、UserDemo esp_lcd_st7121 | なし | ★3（初期化シーケンスの移植と 3 種類の判別） | 3 |
| バックライト | GPIO22（LEDC PWM） | – | M5GFX | esp-hal LEDC は P4 ❌ | ★1（GPIO High だけなら） | 3 |
| タッチ GT911 / ST7123 | I2C、INT=GPIO23 | 0x14(0x5D) / 0x55 | esp_lcd_touch_gt911/st7123、M5GFX | `gt911` 0.3.0 | ★2 | 4 |
| RTC RX8130CE | I2C | 0x32 | M5Unified RX8130_Class | なし | ★1 | 4 |
| INA226 | I2C | 0x41 | M5Unified INA226_Class | `ina226` 0.3.0 | ★1 | 4 |
| microSD | SDMMC slot0（GPIO39-44） | – | IDF sdmmc、BSP | esp-hal SDMMC（⚒）、`embedded-sdmmc` 0.10（FS 層） | ★3 | 5 |
| UART0 / RS485 | GPIO37/38 ・ 20/21/34 | – | IDF uart | esp-hal UART（✔、v3 のみ） | ★2 | 5 |
| USB-A ホスト | USB OTG HS（DWC2）+ USB5V_EN | – | IDF usb_host、TinyUSB | esp-hal usb_otg_hs（⚒）。ホストスタックは事実上なし | ★5 | 6 |
| ESP32-C6 Wi-Fi/BLE | SDIO slot1（GPIO8-13）、RST=GPIO15、WLAN_PWR_EN | – | esp_hosted 1.4 / 2.x、esp_wifi_remote | `embassy-net-esp-hosted` 0.3.0（SPI トランスポート前提。SDIO 版はない） | ★5 | 6 |
| オーディオ ES8388 / ES7210 | I2S0（GPIO26-30）+ I2C | 0x10 / 0x40 | esp_codec_dev、M5Unified | `es7210` 0.1.0。ES8388 はなし | ★3 | 7 |
| IMU BMI270 | I2C | 0x68 | espressif/bmi270、M5Unified | `bmi270` 0.1.1（2023、古い） | ★2（設定 blob の転送が必要） | 7 |
| カメラ SC2356 | MIPI-CSI + ISP、MCLK=GPIO36 | – | esp_video、esp_cam_sensor | esp-hal CSI ❌ | ★5 | 8 |

esp-hal の ✔/⚒/❌ は esp-hal-v1.2.0 README のドライバ表の ESP32-P4 列による。crate のバージョンは crates.io API を 2026-10-29 に照会した結果。

---

## 5. 初期化シーケンス（推奨順）

```
[ROM] → [IDF 2nd stage bootloader @0x2000] → Rust カーネル _start (M モード)
 1. mtvec/スタック/BSS 初期化、PMP 初期設定
 2. LP_WDT(RWDT) 解除・停止、TIMG0/1 MWDT 停止          … §2-1
 3. USJ コンソール（FIFO 直書き）                         … §3.1 ← ここで "hello"
 4. SYSTIMER + 割り込みコントローラ → U モードアプリ起動 / ecall で putc
 --- ここまでが最初のマイルストーン ---
 5. GPIO/IO_MUX、I2C1 (SDA=31, SCL=32, 100〜400kHz)
 6. PI4IOE#1(0x43) / #2(0x44) 設定（§1.3 の M5GFX シーケンス）
      #1: DIR=0x7F, OUT=0x46 → 10ms → OUT=0x76 (LCD/TP リセット解除)
      #2: DIR=0xB9, OUT_H_IM=0x06, PULL_SEL=0xB9, PULL_EN=0xF9,
          IN_DEF=0x40, INT_MASK=0xBF, OUT=0x89 (CHG_EN, USB5V, WLAN_PWR)
      ※ Wi-Fi を使わないうちは WLAN_PWR_EN=0 にして電力を節約するのも可
 7. RX8130 から時刻読み出し、INA226 設定
 8. クロック 360MHz 化（必要なら）、PSRAM 初期化 + MSPI タイミングチューニング
 9. LDO3=2500mV → DSI ホスト/PHY 初期化 → パネル判別:
      (a) I2C 0x55 に応答あり → FW ver 1=ST7121 / 3=ST7123 （BSP 方式）
      (b) なければ GT911(0x14/0x5D) → ILI9881C
    → パネル初期化コマンド → DPI(フレームバッファは PSRAM) → GPIO22 バックライト ON
10. タッチ（INT=GPIO23）
11. SDMMC slot0 → FAT
12. 以降: USB-A ホスト, C6 Wi-Fi(電源 OFF→300ms→ON→1.5s→SDIO CMD5…), オーディオ, カメラ
```

パネル判別の方式は 2 つある。M5GFX ローカル版（0.2.20）は DSI の ID 読み出しで判別するが、ST7121 を知らない（`M5G/lgfx/v1/platforms/esp32p4/` に Panel_ST7121 がない）。upstream の M5GFX 0.2.35（2026-10-25）は Panel_ST7121 を持ち、タッチ IC の FW バージョンで判別する（https://github.com/m5stack/M5GFX/blob/master/src/M5GFX.cpp 3259-3279 行付近）。BSP も FW バージョンで判別している（BSP `src/bsp_display.c:140-191`）。Rust では **I2C の FW バージョンで判別する方式** を採るのがよい。

---

## 6. Rust で書き直すときに重いもの（見積もり、推測を含む）

1. **MIPI-DSI**（★4〜5）: DSI ホスト（Synopsys DW 系）、D-PHY の PLL とタイミング、DPI ブリッジ、（任意で）DMA2D、LDO が絡む。パネルごとに初期化シーケンスとポーチ値が違う（`M5G/M5GFX.cpp:2271-2296`）。ST7123 には「vsync front porch を減らすとタッチが止まる」という癖がある（同 2295 行のコメント）。v1 と v3 で `mipi_dsi_*_struct.h` に差分がある。NuttX の Tab5 実装（C、v1.0 対応）が最良の参考になる。
2. **PSRAM**（★4）: MPLL 設定、HEX PSRAM のモードレジスタ、MSPI タイミングチューニング（サンプル点のスイープ）、キャッシュ/MMU へのマップが必要。失敗すると何も見えないまま壊れる。暫定案として、IDF の C 実装をリンクして FFI で呼ぶ方法もある（推測）。
3. **SDIO + esp_hosted**（★5）: SDMMC ホスト（SDIO CMD52/53）に加えて esp_hosted プロトコル（バージョン依存。出荷 FW は 1.4.x）と RPC（protobuf）が必要。既存の Rust crate（embassy-net-esp-hosted）は SPI トランスポートで、プロトコル世代の整合も要確認。C6 ファームウェアを自作プロトコルに置き換える手もあるが、書き込みに UART 配線や SDIO アップデータが必要（`TCC/slave_c6_fw/README.md`）。
4. **USB OTG HS ホスト**（★5）: DWC2 ホストのチャネル管理、HS PHY、ハブ・HID・MSC のクラスドライバが必要。Rust のホストスタックは事実上ない。
5. **カメラ（CSI + ISP）**（★5）: 優先度が低いので後回しにする。

---

## 7. 未確認事項

- **パネルとタッチの出荷ロット差**（3 種類）: ①初期は ILI9881C + GT911、②2025-10-14 以降は ST7123（タッチ内蔵）、③2026-04-28 以降は ST7121（タッチは ST712x、FW ver=1）（DOC、BSP、NUTTX）。手元の実機がどれかは未確認。背面のシールか、I2C 0x55/0x14/0x5D のプローブで判別できる。ローカルの M5GFX 0.2.20 は ST7121 に未対応で、DOC は M5GFX ≥0.2.30 を推奨している。
- **チップのリビジョン**: 手元の実機は v1.0（ログで確認済み）。今後 v3.x のロットが出るかは不明。もし出れば esp-hal を使える。
- **USJ の初期状態**: bootloader 後に USJ をそのまま使えるかは、実機で未検証（ROM がログを出しているので可能性は高い）。
- **PSRAM 初期化の担当**: bootloader と app のどちらかは、ログの順序からの推測。IDF の `cpu_start.c` で要確認。
- **PI4IOE5V6408 のレジスタ番号**: `wifi_setup.cpp` と M5GFX で 0x09/0x0D/0x0F の解釈が食い違う（§1.3）。データシートで要確認。
- **BMI270 のアドレス**: 0x68 と 0x69 のどちらか（DOC は 0x68）。UserDemo には ICM20602 ドライバも同梱されている（`m5stack/M5Tab5-UserDemo/platforms/tab5/components/sensor_icm20602`）。IMU の部品違いのロットがある可能性（推測）。
- **SC2356 の SCCB アドレス**、**EXT_USB（M5-Bus / MX1.25）の接続先**、**USB-C を OTG デバイスにしたときの PHY 切り替え手順**。
- **ブートローダ以降のクロック状態**（CPU と APB の周波数）。
- **probe-rs で rev v1.0 の P4 をデバッグできるか**。
- **SoC の電源**: 回路図に `VHP_DCDC_EN`、`EN_DCDC` という net があり、HP ドメインを外付け DCDC で供給しているように見える（SCH 行 296-300 付近）。DVFS や電圧の扱いが OS に関係するかは未調査。

---

## 8. 参考文献

**公式・一次資料**
- M5Stack Tab5 ドキュメント: https://docs.m5stack.com/en/core/Tab5
- 回路図: https://m5stack-doc.oss-cn-shenzhen.aliyuncs.com/1132/Tab5_Schematics_PDF.pdf
- ブロック図: https://m5stack-doc.oss-cn-shenzhen.aliyuncs.com/1132/Tab5_Overall_Design_Block_Diagram.pdf
- ESP32-P4 データシートと TRM（esp-hal README 内のリンク）: https://www.espressif.com/sites/default/files/documentation/esp32-p4_datasheet_en.pdf 、https://www.espressif.com/sites/default/files/documentation/esp32-p4_technical_reference_manual_en.pdf

**ドライバ実装**
- espressif/esp-bsp `bsp/m5stack_tab5`（v1.3.2）: https://github.com/espressif/esp-bsp/tree/master/bsp/m5stack_tab5
- m5stack/M5GFX（upstream 0.2.35）: https://github.com/m5stack/M5GFX
- m5stack/M5Tab5-UserDemo: https://github.com/m5stack/M5Tab5-UserDemo
- Apache NuttX の Tab5 ボード: https://nuttx.apache.org/docs/latest/platforms/risc-v/esp32p4/boards/esp32p4-tab5/index.html

**Rust**
- esp-rs/esp-hal（README の ESP32-P4 行と `esp-metadata/devices/esp32p4/soc.toml`）: https://github.com/esp-rs/esp-hal
- probe-rs issue #4357: https://github.com/probe-rs/probe-rs/issues/4357
- crates.io: `gt911`、`ina226`、`bmi270`、`es7210`、`embedded-sdmmc`、`embassy-net-esp-hosted`、`esp32p4`、`esp-hal`（2026-10-29 に照会）

**ローカル**
- `~/repos/tab5_claude_client/`: `sdkconfig.defaults.esp32p4`、`sdkconfig`、`logs/boot.log`、`main/wifi_setup.cpp`、`main/Kconfig.projbuild:34-46`、`M5_IDF6_PATCHES.md`、`CLAUDE.md`、`slave_c6_fw/README.md`
- `components/M5Unified/src/`: `M5Unified.cpp`、`utility/Power_Class.{cpp,hpp}`、`utility/RTC_Class.cpp`
- `components/M5GFX/src/`: `M5GFX.cpp:2170-2345`、`lgfx/v1/platforms/esp32p4/*`
- `~/esp-idf/6.0/components/`: `soc/esp32p4/register/hw_ver1/soc/{reg_base.h,usb_serial_jtag_reg.h,lp_wdt_reg.h,timer_group_reg.h,io_mux_reg.h}`、`soc/esp32p4/include/soc/uart_pins.h`、`hal/esp32p4/include/hal/usb_serial_jtag_ll.h`、`esp_hal_wdt/esp32p4/include/hal/lpwdt_ll.h`、`bootloader_support/src/{bootloader_init.c,esp32p4/bootloader_esp32p4.c}`、`esp_psram/device/esp_psram_impl_ap_hex.c`
- ダウンロードした資料（回路図、BSP、esp-hal メタデータ）は調査時の一時コピーで、リポジトリには含めない
