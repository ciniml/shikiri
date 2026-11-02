# PPA（ブレンド）と 2D-DMA をレジスタだけで動かす手順（ESP32-P4 rev v1.x / hw_ver1）

対象は ESP32-P4 rev < 3.0（`CONFIG_ESP32P4_SELECTS_REV_LESS_V3`。このときレジスタ定義は `soc/esp32p4/register/hw_ver1` が使われる [soc/CMakeLists.txt:38-41]）。
出典は ESP-IDF 6.0 だけを使う。パスは `~/esp-idf/6.0/components/` からの相対パス。`docs/...` だけは `~/esp-idf/6.0/docs/` からの相対パス。（推測）はソースに書かれていない推測、（判断）はこちらの判断を表す。
略記: `SOCR` = `soc/esp32p4/register/hw_ver1/soc/`、`DLL` = `esp_hal_dma/esp32p4/include/hal/dma2d_ll.h`（`dma2d_ll.h` は `hal/esp32p4` ではなくここにある）、`PLL` = `hal/esp32p4/include/hal/ppa_ll.h`、`D2D` = `esp_hw_support/dma/dma2d.c`。

## 0. 結論
- **ベースアドレス**: PPA = `0x5008_7000`、DMA2D = `0x5008_8000`、HP_SYS_CLKRST = `0x500E_6000`（§1）。
- **立ち上げ**: `SOC_CLK_CTRL1`（`0x500E_6018`）の bit6（DMA2D）と bit9（PPA）を 1 にし、`HP_RST_EN0`（`0x500E_60C0`）の bit31（DMA2D）と `HP_RST_EN1`（`0x500E_60C4`）の bit0（PPA）を 1→0 にする。そのあと DMA2D の `RST_CONF`（`0x5008_8A04`）で clk_en=1 と AXI FIFO のリセットを行い、`OUT/IN_ARB_CONFIG` を 0 にする。ESP-IDF が一度だけ行うのはこれで全部。メモリの電源やアクセス範囲の設定はしていない（§2）。
- **チャネル**: rev < 3.0 では TX が 0〜2、RX が 0〜1 の 3+2 本（DLL:26-36）。PPA ブレンドの接続先は `peri_sel` で選ぶ。FILL は RX だけを使い、`peri_sel`=2。BLEND は TX bg=2、TX fg=3、RX=2 を使う。ESP-IDF はチャネル番号を固定しておらず、空いている中から番号の大きいものを選ぶ（§3.1）。
- **完了**: ESP-IDF は DMA2D の RX チャネルの `IN_INT_RAW` の bit1（SUC_EOF）だけを見ている。PPA 側の割り込み（`PPA_INT_RAW` の bit1 BLEND_EOF）は使っていない（§4.6）。
- **単純な矩形コピー**: いちばん簡単なのは **DMA2D の M2M**（PPA を使わない）。同じ番号の TX と RX を組にし、TX の `peri_sel` を 4〜7、RX の `peri_sel` を 3〜7 にして、RX の `in_mem_trans_en`=1 にする。ESP-IDF の `esp_async_fbcpy`（DSI のフレームバッファへのコピー）がこの方法を使っている（§6）。転送元と転送先が重なる場合の動作は、どこにも書かれていない。
- **PSRAM**: ESP-IDF は `0x4800_0000` 台の仮想アドレス（heap が返すポインタ）を、そのまま記述子の buffer に入れている。CPU のキャッシュとの整合は、DMA が読む前に L1→L2 の順に書き戻し、DMA が書く範囲は始める前に無効化する、という方法で取っている（§7）。

## 1. アドレス一覧

### 1.1 ベース
| ブロック | アドレス | 根拠 |
|---|---|---|
| `DR_REG_HPPERIPH0_BASE` | `0x5000_0000` | SOCR/reg_base.h:9 |
| PPA (`DR_REG_PPA_BASE`) | `0x5008_7000` | SOCR/reg_base.h:48 |
| DMA2D (`DR_REG_DMA2D_BASE`) | `0x5008_8000` | SOCR/reg_base.h:49 |
| `DR_REG_HPPERIPH1_BASE` | `0x500C_0000` | SOCR/reg_base.h:10 |
| HP_SYS_CLKRST | `0x500E_6000`（= HPPERIPH1 + 0x26000） | SOCR/reg_base.h:120 |

### 1.2 DMA2D のチャネルごとのレジスタ（hw_ver1）
TX（OUT）チャネル n のベース = `0x5008_8000 + 0x100*n`（n=0..2）、RX（IN）チャネル n のベース = `0x5008_8500 + 0x100*n`（n=0..1）[SOCR/dma2d_reg.h:17,1052,2060,3068,4131]。構造体は TX が `out_channel[3]`、RX が `in_channel[2]`、全体の大きさは 0xA30 [SOCR/dma2d_struct.h:1777-1799]。

| オフセット | TX（OUT） | RX（IN） |
|---|---|---|
| +0x00 | `OUT_CONF0` | `IN_CONF0` |
| +0x04 | `OUT_INT_RAW` | `IN_INT_RAW` |
| +0x08 | `OUT_INT_ENA` | `IN_INT_ENA` |
| +0x0C | `OUT_INT_ST` | `IN_INT_ST` |
| +0x10 | `OUT_INT_CLR` | `IN_INT_CLR` |
| +0x1C | `OUT_LINK_CONF` | `IN_LINK_CONF` |
| +0x20 | `OUT_LINK_ADDR` | `IN_LINK_ADDR` |
| +0x24 | `OUT_STATE` | `IN_STATE` |
| +0x28 | `OUT_EOF_DES_ADDR` | `IN_SUC_EOF_DES_ADDR` |
| +0x2C | `OUT_DSCR` | `IN_ERR_EOF_DES_ADDR` |
| +0x38 | `OUT_PERI_SEL` | （+0x3C が `IN_PERI_SEL`） |
| +0x3C | `OUT_ARB` | （+0x40 が `IN_ARB`） |
| +0x48 | `OUT_COLOR_CONVERT` | （+0x4C が `IN_COLOR_CONVERT`、ch0 だけ） |
| +0x6C | `OUT_DSCR_PORT_BLK` | — |

根拠: SOCR/dma2d_reg.h:17,130,232,328,424,663,697,709,742,755,791,804,892,1033（TX ch0）、3068,3172,3281,3384,3487,3733,3775,3787,3820,3833,3882,3895,3983（RX ch0）。
- RX ch1 には色変換のレジスタがなく、+0x48（`0x5008_8648`）が `IN_ETM_CONF_CH1` になっている [SOCR/dma2d_reg.h:5019; SOCR/dma2d_struct.h:1766-1773; DLL:682]。

よく使う絶対アドレス:

| レジスタ | ch0 | ch1 | ch2 |
|---|---|---|---|
| OUT_CONF0 | 0x5008_8000 | 0x5008_8100 | 0x5008_8200 |
| OUT_INT_RAW / CLR | 0x5008_8004 / 0x5008_8010 | 0x5008_8104 / 0x5008_8110 | 0x5008_8204 / 0x5008_8210 |
| OUT_LINK_CONF / ADDR | 0x5008_801C / 0x5008_8020 | 0x5008_811C / 0x5008_8120 | 0x5008_821C / 0x5008_8220 |
| OUT_STATE | 0x5008_8024 | 0x5008_8124 | 0x5008_8224 |
| OUT_PERI_SEL | 0x5008_8038 | 0x5008_8138 | 0x5008_8238 |
| OUT_COLOR_CONVERT | 0x5008_8048 | 0x5008_8148 | 0x5008_8248 |
| IN_CONF0 | 0x5008_8500 | 0x5008_8600 | — |
| IN_INT_RAW / CLR | 0x5008_8504 / 0x5008_8510 | 0x5008_8604 / 0x5008_8610 | — |
| IN_LINK_CONF / ADDR | 0x5008_851C / 0x5008_8520 | 0x5008_861C / 0x5008_8620 | — |
| IN_STATE | 0x5008_8524 | 0x5008_8624 | — |
| IN_PERI_SEL | 0x5008_853C | 0x5008_863C | — |
| IN_COLOR_CONVERT | 0x5008_854C | （なし） | — |

### 1.3 DMA2D の共通レジスタ
| レジスタ | アドレス | 根拠 |
|---|---|---|
| `AXI_ERR` | 0x5008_8A00 | SOCR/dma2d_reg.h:5045 |
| `RST_CONF` | 0x5008_8A04 | SOCR/dma2d_reg.h:5099 |
| `INTR_MEM_START/END_ADDR` | 0x5008_8A08 / 0x5008_8A0C | SOCR/dma2d_reg.h:5126,5138 |
| `EXTR_MEM_START/END_ADDR` | 0x5008_8A10 / 0x5008_8A14 | SOCR/dma2d_reg.h:5151,5163 |
| `OUT_ARB_CONFIG` / `IN_ARB_CONFIG` | 0x5008_8A18 / 0x5008_8A1C | SOCR/dma2d_reg.h:5176,5195 |
| `DATE` | 0x5008_8A2C（既定値 36716816） | SOCR/dma2d_reg.h:5257-5258 |

### 1.4 PPA（ブレンドで使うもの）
| レジスタ | アドレス | 根拠 |
|---|---|---|
| `PPA_INT_RAW` / `ST` / `ENA` / `CLR` | 0x5008_7010 / 14 / 18 / 1C | SOCR/ppa_reg.h:107,137,163,189 |
| `PPA_BLEND_COLOR_MODE` | 0x5008_7024 | SOCR/ppa_reg.h:264 |
| `PPA_BLEND_BYTE_ORDER` | 0x5008_702C | SOCR/ppa_reg.h:322 |
| `PPA_BLEND_TRANS_MODE` | 0x5008_7034 | SOCR/ppa_reg.h:359 |
| `PPA_BLEND_TX_SIZE` | 0x5008_703C | SOCR/ppa_reg.h:430 |
| `PPA_BLEND_FIX_ALPHA` | 0x5008_7040 | SOCR/ppa_reg.h:451 |
| `PPA_BLEND_RGB` | 0x5008_7048 | SOCR/ppa_reg.h:504 |
| `PPA_BLEND_FIX_PIXEL` | 0x5008_704C | SOCR/ppa_reg.h:530 |
| `PPA_CK_FG_LOW/HIGH`、`CK_BG_LOW/HIGH`、`CK_DEFAULT` | 0x5008_7050 / 54 / 58 / 5C / 60 | SOCR/ppa_reg.h:542,568,594,620,646 |
| `PPA_REG_CONF` | 0x5008_706C | SOCR/ppa_reg.h:774 |
| `PPA_BLEND_ST` | 0x5008_7074 | SOCR/ppa_reg.h:805 |
| `PPA_DATE` | 0x5008_7100（既定値 36716609） | SOCR/ppa_reg.h:1021-1022 |

## 2. モジュールの立ち上げ（一度だけ）

ESP-IDF の順序: PPA のバスクロックを入れてリセットする [esp_driver_ppa/src/ppa_core.c:168-172] → `dma2d_acquire_pool` で DMA2D のバスクロックを入れてリセットする [D2D:400-404] → `dma2d_ll_hw_enable` [D2D:407]。

| 手順 | 書き込み | 根拠 |
|---|---|---|
| 1. PPA のクロック | `0x500E_6018`（SOC_CLK_CTRL1）の **bit9** `reg_ppa_sys_clk_en` を 1 にする | PLL:56-59; SOCR/hp_sys_clkrst_reg.h:376,443 |
| 2. PPA のリセット | `0x500E_60C4`（HP_RST_EN1）の **bit0** `reg_rst_en_ppa` を 1→0 にする | PLL:71-75; SOCR/hp_sys_clkrst_reg.h:3293,3297 |
| 3. DMA2D のクロック | `0x500E_6018` の **bit6** `reg_dma2d_sys_clk_en` を 1 にする | DLL:102-106; SOCR/hp_sys_clkrst_reg.h:422 |
| 4. DMA2D のリセット | `0x500E_60C0`（HP_RST_EN0）の **bit31** `reg_rst_en_dma2d` を 1→0 にする | DLL:120-125; SOCR/hp_sys_clkrst_reg.h:3064,3285 |
| 5. `dma2d_ll_hw_enable` | `RST_CONF`（0x5008_8A04）で clk_en（bit2）=1 にし、axim_rd_rst（bit0）を 1→0、axim_wr_rst（bit1）を 1→0 にする。ESP-IDF はビットごとに読み書きするので、書く値は順に 0x4, 0x5, 0x4, 0x6, 0x4 | DLL:147-155; SOCR/dma2d_struct.h:1528-1546 |
| 6. 調停（重み）を切る | `OUT_ARB_CONFIG`（0x5008_8A18）= 0、`IN_ARB_CONFIG`（0x5008_8A1C）= 0 | DLL:156-158 |

- 書き込みはどれも読み出し・変更・書き戻しで行う（同じレジスタに他の周辺機器のビットがあるため）。ESP-IDF はこれを `PERIPH_RCC_ATOMIC()`（クリティカルセクション）で囲んでいる [ppa_core.c:169; D2D:401]。
- `RST_CONF.clk_en` の説明は「1: レジスタのクロックを常に入れる。0: アプリがレジスタに書くときだけクロックを入れる」[SOCR/dma2d_struct.h:1538-1541]。
- `PPA_REG_CONF.PPA_CLK_EN`（0x5008_706C bit0、「register clock gate enable」、既定値 0）[SOCR/ppa_reg.h:774-778] には、ESP-IDF はどこでも書いていない（`reg_conf` を grep しても `hal/esp32p4` と `esp_driver_ppa` に出てこない）。書かなくても動く（推測）。
- **アクセス範囲**: `INTR_MEM_*` と `EXTR_MEM_*` の既定値は 0x3010_0000〜0x8FFF_FFFF で、範囲外のアドレスは記述子エラーになる [SOCR/dma2d_struct.h:1553-1596]。内部 SRAM（0x4FF0_0000〜）と PSRAM（0x4800_0000〜）はどちらも既定の範囲に入る。ESP-IDF は P4 ではこのレジスタに書いていない（`intr_mem_start` に書いているのは C5/C61/H4 の AHB DMA だけ）。
- **メモリの電源**: reorder FIFO の RAM（`OUT/IN_RO_PD_CONF`）は既定値が force_pu=1 [SOCR/dma2d_struct.h:634-642,1385-1393] で、ESP-IDF も触っていない。PPA で触るメモリの電源設定は SRM 用の `PPA_SR_MEM_PD`（0x68）と CLUT 用だけで、ブレンドや塗りつぶしでは要らない（推測）。
- **割り込み**: ESP-IDF はチャネルごとに割り込みを割り当てる [D2D:439-467] が、ポーリングで使うなら要らない。`*_INT_RAW` は ENA に関係なく立つ（推測。「raw interrupt bit」という説明から）。
- ESP-IDF の再起動前処理は、DMA2D のバスクロックが入っていれば、全チャネルの `cmd_disable` を 1 にして `reset_avail` を待つ [esp_system/port/soc/esp32p4/system_internal.c:56-65]。カーネルの再起動やパニックのときも、これと同じ処理をすると安全（判断）。

## 3. DMA2D チャネルの設定（各操作の前に毎回）

### 3.1 チャネル番号と peri_sel
- rev < 3.0 のチャネル数は TX 3、RX 2 [DLL:31-36]。割り込みも OUT_CH0〜2 と IN_CH0〜1 だけ [esp_hal_dma/esp32p4/dma2d_periph.c:13-27]。
- 機能の違い: reorder は TX ch0 と RX ch0 だけ、色変換（CSC）は TX が ch0〜2（rev ≥ 3.0 では ch0〜3）、RX が ch0 だけ [DLL:76-83]。ブレンドと塗りつぶしではどちらも使わない。
- ESP-IDF の選び方: 空いている中から **番号の大きいもの** を選ぶ（「ch0 は機能が多いので最後に取っておく」）[D2D:93,131]。ブレンドでは、最初に取った TX を bg、次の TX を fg にする [esp_driver_ppa/src/ppa_blend.c:35-46]。全部空いていれば、rev < 3.0 では bg=TX2、fg=TX1、RX=RX1 になる（推測。D2D:93,131 からの計算）。どのチャネルでも、`peri_sel` で PPA に接続される。
- `peri_sel` の値 [soc/esp32p4/include/soc/dma2d_channel.h:10-19]:

| 用途 | TX `out_peri_sel` | RX `in_peri_sel` |
|---|---|---|
| JPEG | 0 | 0 |
| PPA SRM | 1 | 1 |
| PPA BLEND（bg） | 2 | 2（出力） |
| PPA BLEND（fg） | 3 | — |
| M2M | 4〜7 のどれでも | 3〜7 のどれでも |
| どこにも接続しない | 7 | 7 |

  - M2M 用の ID の候補は、マスクで TX 0xF0、RX 0xF8 [DLL:86-87]。ESP-IDF は空いている中で最も小さい番号を使う（`__builtin_ctz`）ので、普通は TX=4、RX=3 になる [D2D:554-557,379-380]。接続しない値は 7 で、フィールドは 3 ビット [DLL:89-91]。
  - レジスタは `OUT_PERI_SEL`、`IN_PERI_SEL` のどちらも bit[2:0]、既定値は 7 [SOCR/dma2d_struct.h:568,1319]。

### 3.2 チャネルのリセット（`dma2d_hal_*_reset_channel`）
`dma2d_connect` はまず停止し、続いてリセットする [D2D:574-575,599-600]。
1. 停止: TX は `OUT_LINK_CONF` の bit20（outlink_stop）に 1 を書く。RX は `IN_LINK_CONF` の bit21（inlink_stop）に 1 を書く [DLL:379-382,905-908; SOCR/dma2d_struct.h:449,1186]。
2. `CONF0` の bit25（cmd_disable）を 1 にする [esp_hal_dma/dma2d_hal.c:17,25; SOCR/dma2d_struct.h:79-82,795]。
3. `STATE` の reset_avail が 1 になるまで待つ。**TX は `OUT_STATE` の bit24、RX は `IN_STATE` の bit23**（ビット位置が違うので注意）[dma2d_hal.c:18,26; SOCR/dma2d_struct.h:501,1238]。
4. `CONF0` の bit24（rst）を 1→0 にする [DLL:287-291,803-807; SOCR/dma2d_struct.h:75-78,791]。
5. `CONF0` の bit25 を 0 に戻す [dma2d_hal.c:20,28]。

### 3.3 CONF0 のビット（TX と RX でほぼ同じ）
| bit | TX `OUT_CONF0` | RX `IN_CONF0` | 根拠 |
|---|---|---|---|
| 0 | out_auto_wrback | in_mem_trans_en（M2M のとき 1） | SOCR/dma2d_struct.h:19-23,740-743 |
| 1 | out_eof_mode（既定値 1） | 予約 | :24-28,744 |
| 2 | outdscr_burst_en | indscr_burst_en | :29-33,745-749 |
| 3 | out_ecc_aes_en | in_ecc_aes_en | :34-39,750-755 |
| 4 | out_check_owner | in_check_owner | :40-43,756-759 |
| 8:6 | mem_burst_length（0:8B, 1:16B, 2:32B, 3:64B, 4:128B） | 同じ | :48-52,764 |
| 10:9 | macro_block_size（0:8x8, 1:8x16, 2:16x16, **3: なし**） | 同じ | :53-58,769 |
| 11 | dscr_port_en | 同じ | :59-62,775 |
| 12 | page_bound_en | 同じ | :63-67,779 |
| 16 | reorder_en（ch0 だけ） | 同じ | :69-73,785 |
| 24 | rst | 同じ | :75-78,791 |
| 25 | cmd_disable | 同じ | :79-82,795 |
| 26 | arb_weight_opt_dis | 同じ | :83-86,799 |

ESP-IDF が設定する値（`dma2d_connect` [D2D:576-597,601-621] のあと、PPA と M2M のテストでは `dma2d_set_transfer_ability` [D2D:838-846] で desc_burst=1、burst=128B、mb=なし を設定する [ppa_fill.c:64-69; ppa_blend.c:120-127]）:

| 項目 | TX | RX | 根拠 |
|---|---|---|---|
| reorder | 0 | 0 | D2D:579,604（channel_flags=0 [ppa_fill.c:146; ppa_blend.c:319]） |
| dscr_port_en | 0（PPA_SRM のときだけ 1） | 0（同じ） | D2D:581,606 |
| check_owner | 0 | 0 | D2D:584,609 |
| auto_wrback | 0 | — | D2D:585 |
| eof_mode | 1 | — | D2D:586 |
| desc burst | 1 | 1 | ppa_fill.c:66; ppa_blend.c:122 |
| data burst | 128B（sel=4） | 128B（sel=4） | D2D:588,612; DLL:264-271; ppa_core.c:278 |
| page_bound_en | 1 | 1 | D2D:589,613 |
| macro block | なし（sel=3） | なし（sel=3） | D2D:590,614; DLL:328-329,854-855 |
| in_mem_trans_en | — | M2M のときだけ 1 | DLL:454 |

**書く値（PPA の FILL/BLEND）**: `OUT_CONF0` = **0x0000_1706**（bit1 + bit2 + 4<<6 + 3<<9 + bit12）、`IN_CONF0` = **0x0000_1704**。M2M では `IN_CONF0` = **0x0000_1705**（`esp_async_fbcpy` は transfer_ability を設定しないので desc burst=0 になり、TX 0x1702、RX 0x1701 になる [esp_lcd/src/esp_async_fbcpy.c:150-182]）。

その他の設定:
- **RX の owner の書き戻し**: `IN_LINK_CONF` の bit20 `inlink_auto_ret` を **0**（CPU）にする（既定値は 1）[D2D:610; DLL:397-400; SOCR/dma2d_struct.h:1181-1185]。RX は常に記述子を書き戻す（auto write back）[D2D:821]。
- **色変換を切る**（CSC を持つチャネルだけ）[D2D:591-593,615-617]:
  - TX（rev < 3.0 では ch0〜2）: `OUT_COLOR_CONVERT` = **0x3A**（output_sel[1:0]=2、3b_proc_en[2]=0、input_sel[5:3]=7）[DLL:994-1009,1078-1080; SOCR/dma2d_struct.h:657-667]。
  - RX ch0: `IN_COLOR_CONVERT` = **0x39**（output_sel=1、input_sel=7）[DLL:488-501,581-583; SOCR/dma2d_struct.h:1408-1418]。RX ch1 には設定しない。
- **割り込みの初期化**: TX は `OUT_INT_ENA`=0、`OUT_INT_CLR`=0x1FFF。RX は `IN_INT_ENA`=0、`IN_INT_CLR`=0x3FFF [D2D:596-597,620-621; DLL:43-44]。

### 3.4 記述子のアドレスと開始
- `OUT_LINK_ADDR` / `IN_LINK_ADDR`（+0x20、32 ビット）に記述子のアドレスを書く [DLL:361-364,887-890; SOCR/dma2d_struct.h:476,1213]。
- 開始: TX は `OUT_LINK_CONF` の **bit21**（outlink_start）、RX は `IN_LINK_CONF` の **bit22**（inlink_start）に 1 を書く [DLL:370-373,896-899; SOCR/dma2d_struct.h:453,1190]。RX は読み出し・変更・書き戻しで行う（bit20 の auto_ret を保つため。0 にしてあれば、そのまま `1<<22` を書けばよい）。
- 開始する前に、ESP-IDF は FSM が idle であることを assert で確かめている（`OUT_STATE[23:20]`==0、`IN_STATE[22:20]`==0）[D2D:746,749; DLL:415-418,932-935; SOCR/dma2d_struct.h:497,1234]。
- `OUT_LINK_CONF` のビット: stop=20、start=21、restart=22、park=23（RO、1=idle）。`IN_LINK_CONF` のビット: auto_ret=20、stop=21、start=22、restart=23、park=24 [SOCR/dma2d_struct.h:449-461,1181-1202]。

## 4. 2D 記述子

### 4.1 形式（`dma2d_descriptor_align8_t`、24 バイト、8 バイト境界）
定義は hal/include/hal/dma2d_types.h:22-63。大きさは 24 バイト（:60）で、8 バイト境界に置く（:58,62-63; DLL:93）。

| ワード | ビット | フィールド | 意味 |
|---|---|---|---|
| W0 | [13:0] | `vb_size` | ブロックの高さ（画素）。dma2d_en=0 のときは buffer の大きさの下位 14 ビット（バイト） |
| W0 | [27:14] | `hb_length` | ブロックの幅（画素）。dma2d_en=0 のときは長さの下位 14 ビット。RX では転送後に DMA が書き込む |
| W0 | [28] | `err_eof` | 受信データのエラー。TX では 0 |
| W0 | [29] | `dma2d_en` | 1 で 2D 転送 |
| W0 | [30] | `suc_eof` | リンクの最後の記述子 |
| W0 | [31] | `owner` | 0=CPU、1=DMA |
| W1 | [13:0] | `va_size` | 画像全体の高さ（画素）。dma2d_en=0 のときは大きさの上位 14 ビット |
| W1 | [27:14] | `ha_length` | 画像全体の幅（画素）。dma2d_en=0 のときは長さの上位 14 ビット。RX では DMA が書き込む |
| W1 | [31:28] | `pbyte` | 1 画素のバイト数のコード（下の表） |
| W2 | [13:0] | `y` | 画像の中でのブロックの y 位置（画素） |
| W2 | [27:14] | `x` | ブロックの x 位置（画素） |
| W2 | [28] | `mode` | 0=SINGLE（hb×vb を 1 回）、1=MULTIPLE。dma2d_en=0 のときは 0 にする |
| W2 | [31:29] | 予約 | 0 |
| W3 | [31:0] | `buffer` | 画像（ha×va）の先頭アドレス。RX では 4 バイト境界の位置と大きさが必要（PSRAM と AXI の制約。満たさないと他のデータを上書きしたり、上書きされたりする） |
| W4 | [31:0] | `next` | 次の記述子。最後なら NULL |
| W5 | — | （パディング） | 8 バイト境界にするため |

根拠: dma2d_types.h:24-57（フィールドの順序と幅）。ビット位置はフィールドの幅を下位から積んだもの。

`pbyte` のコード [dma2d_types.h:71-98]: 0=0.5B（A4 など 4bpp）、1=1B（A8）、2=1.5B（12bpp）、**3=2B（RGB565）**、4=3B（RGB888）、5=4B（ARGB8888）。

Rust でのワードの組み立て（判断）:
```
w0 = vb | hb<<14 | err_eof<<28 | en2d<<29 | suc_eof<<30 | owner<<31
w1 = va | ha<<14 | pbyte<<28
w2 = y  | x<<14  | mode<<28
w3 = buffer; w4 = next; w5 = 0
```
例: 1280x720 の RGB565 画像で、(x=10, y=20) から 100x50 の領域を指す記述子（suc_eof=1、owner=DMA）は w0=`0xE019_0032`、w1=`0x3140_02D0`、w2=`0x0002_8014`。

- ブロックの幅と高さ、画像の幅と高さ、x と y は **どれも 14 ビット（最大 16383）**。dma2d_en=0（1D）のときは、大きさと長さが 28 ビットになる（下位 14 ビットが vb/hb、上位 14 ビットが va/ha）[dma2d_types.h:24-41]。1D の例: TX は ha=va=`size>>14`、hb=vb=`size&0x3FFF`。RX は大きさ（vb/va）だけを設定し、長さ（hb/ha）は 0 にする [esp_hw_support/test_apps/dma2d/main/test_dma2d.c:112-121,34-55]。

### 4.2 置き場所とキャッシュ
- ハードウェアの要件は 8 バイト境界 [DLL:93]。ESP-IDF はこれに加えて「TCM に置かない」としている（ESP-IDF 側の制限）[D2D:715]。
- PPA ドライバは、記述子を `heap_caps_aligned_calloc(max(8, L1 D-cache ライン), …, MALLOC_CAP_DMA | MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT)` で確保する。大きさはラインの倍数に切り上げる。つまり **内部 SRAM の 64 バイト境界、64 バイト単位**（L1 D のラインは 64 バイト）[ppa_core.c:61-66,121-123; hal/esp32p4/include/hal/cache_ll.h:931-939]。`esp_async_fbcpy` も同じ確保をしている [esp_lcd/src/esp_async_fbcpy.c:67-72]。
- 記述子を書いたあとで **L1 を書き戻す**: PPA は `esp_cache_msync(desc, size, C2M)`[ppa_fill.c:55; ppa_blend.c:105-107]、fbcpy は `C2M | INVALIDATE`（書き戻してから無効化）[esp_async_fbcpy.c:134-135]。内部 SRAM のアドレスでは L1 だけが対象になる（`cache_hal_vaddr_to_cache_level_id` が level 1 を返す）（推測）。
- RX 記述子は、転送が終わると DMA が書き換える（hb/ha、owner の書き戻し）[dma2d_types.h:28,42]。ESP-IDF は書き戻された内容を読まず、毎回すべてのフィールドを書き直してから書き戻している。使い回すときも、毎回全部を書いて C2M をすればよい（判断）。

## 5. 操作ごとの手順

### 5.1 RGB565 の矩形 FILL（RX チャネル 1 本）
ESP-IDF の手順は `ppa_fill_transaction_on_picked` [esp_driver_ppa/src/ppa_fill.c:18-87] のとおり。

1. **キャッシュ**: 出力の「拡張窓」（`buf + off_y*pic_w*2` から `pic_w*block_h*2` バイト。行の全幅を含む）を、L2 のライン境界に広げて **書き戻してから無効化する**（`C2M | INVALIDATE`）[ppa_fill.c:122-127]。出力バッファの先頭アドレスと大きさは、ラインの倍数でなければならない（ESP-IDF の検査）[ppa_fill.c:96-98]。
2. **ブレンドエンジンのリセット**: `PPA_BLEND_TRANS_MODE`（0x5008_7034）の bit4 `blend_rst` を 1→0 にする [ppa_fill.c:29; PLL:602-606; SOCR/ppa_reg.h:390-396]。
3. **RX 記述子**: vb=ブロックの高さ、hb=ブロックの幅、err_eof=0、dma2d_en=1、suc_eof=1、owner=1、va=pic_h、ha=pic_w、**pbyte=3**、y=off_y、x=off_x、mode=0、buffer=出力バッファの先頭（ブロックの先頭ではない）、next=0 [ppa_fill.c:40-53]。書いたら C2M（§4.2）。
4. **RX チャネルの接続**（§3.2〜3.3）: 停止 → リセット → `IN_PERI_SEL`=**2** → `IN_CONF0`=0x1704 → `IN_LINK_CONF` の auto_ret=0 → （ch0 なら）`IN_COLOR_CONVERT`=0x39 → ENA=0、CLR=0x3FFF [ppa_fill.c:58-69]。
5. `IN_LINK_ADDR` に記述子のアドレスを書き、`IN_LINK_CONF` の bit22（start）に 1 を書く [ppa_fill.c:76-77]。ESP-IDF はここで RX の ENA に SUC_EOF|ERR_EOF|DSCR_ERR（0xE）を加える [D2D:742; esp_hw_support/dma/dma2d_priv.h:36]。ポーリングだけなら要らない（推測）。
6. **PPA の設定** [ppa_fill.c:80-81; PLL:1050-1081,751-780]:
   - `PPA_BLEND_FIX_PIXEL`（0x5008_704C）= ARGB8888 の 32 ビット値（`a<<24 | r<<16 | g<<8 | b`）[hal/include/hal/color_types.h:154-162; esp_driver_ppa/include/driver/ppa.h:272-276]。RGB565 で出力すると、`r>>3`、`g>>2`、`b>>3` に切り詰めた RGB565（`r<<11 | g<<5 | b`、リトルエンディアンの u16）になる。これは ESP-IDF のテストで確かめられている [esp_driver_ppa/test_apps/main/test_ppa.c:516-559; color_types.h:179-186]。一方、ドライバのコメントには「fill ではブレンドエンジンの色変換を通らない」とある [PLL:1041-1042]。このずれについては §8 を参照。
   - `PPA_BLEND_TX_SIZE`（0x5008_703C）= `hb | vb<<14`（それぞれ [13:0] と [27:14]、14 ビット）[SOCR/ppa_reg.h:430-446]。値は記述子の hb/vb と同じ。
   - `PPA_BLEND_COLOR_MODE`（0x5008_7024）の `blend_tx_cm`[11:8] = **2**（RGB565）[PLL:761-762,779; SOCR/ppa_reg.h:281-288]。
7. **開始**: `PPA_BLEND_TRANS_MODE` に bypass[1]=0、fix_pixel_fill_en[2]=1、blend_en[0]=1、trans_mode_update[3]=1 を書く [ppa_fill.c:83; PLL:621-624,633-634; SOCR/ppa_reg.h:360-389]。ESP-IDF はビットごとに読み書きするので、書く値は順に 0x04 → 0x05 → **0x0D**。update は WT（書いたときだけ効く）ビットなので、最後に 0x0D を書けばよい（推測）。**→ 実機で誤りとわかった（doc/44 §2）**: 0x0D を 1 回で書くと、エンジンは前の操作のモードで動く（最初の塗りつぶしは終わらず、塗りつぶしの後の最初のブレンドは出力が書かれない）。ESP-IDF のように、モード（0x05）を書いてから、別の書き込みで update を立てる（0x0D）。fill_en の説明は「Tx チャネル（PPA から見た出力）だけが動き、出力画素は FIX_PIXEL になる」[SOCR/ppa_reg.h:374-377]。
8. **完了**: §5.4 を見る。
- 塗りつぶしにはブレンドエンジンを使う。このため、ブレンドと塗りつぶしを同時に実行することはできない（ESP-IDF は、FILL のクライアントにもブレンドエンジンを割り当てる [ppa_core.c:284-288]）。

### 5.2 BLEND: A8（または A4）の前景を固定色で RGB565 の背景に重ね、RGB565 で出力する
ESP-IDF の手順は `ppa_blend_transaction_on_picked` [esp_driver_ppa/src/ppa_blend.c:18-184] のとおり。チャネルは TX 2 本（bg、fg）と RX 1 本。

1. **キャッシュ** [ppa_blend.c:272-298]:
   - bg と fg の入力の拡張窓（`buf + off_y*pic_w*bpp/8` から `pic_w*block_h*bpp/8` バイト）を、ライン境界に広げて **書き戻す（C2M）**。
   - 出力の拡張窓は **無効化（M2C）だけ** する。このとき境界は L2 のラインに厳密に合わせる。コメントには「出力が入力と同じバッファの場合、書き戻しと無効化の範囲がずれると壊れる」とある [:278-280]。
2. `blend_rst` を 1→0 にする [ppa_blend.c:29]。
3. **記述子**（3 つとも dma2d_en=1、suc_eof=1、owner=1、mode=0、next=0）[ppa_blend.c:60-103]:

| | TX bg | TX fg | RX（出力） |
|---|---|---|---|
| hb / vb | ブロックの w / h | ブロックの w / h（bg と同じでなければならない） | fg のブロックの w / h |
| ha / va | bg の pic_w / pic_h | fg の pic_w / pic_h | out の pic_w / pic_h |
| x / y | bg のオフセット | fg のオフセット | out のオフセット |
| pbyte | 3（RGB565） | A8 なら **1**、A4 なら **0** | 3 |
| buffer | bg の画像の先頭 | fg の画像の先頭 | 出力画像の先頭 |

   - bg と fg のブロックの大きさは同じでなければならない [ppa_blend.c:237-238]。A4 では、ブロックの幅と x オフセットが偶数でなければならない [ppa_blend.c:213-216]。
   - 書いたら、3 つとも C2M [ppa_blend.c:105-107]。
4. **チャネルの接続** [ppa_blend.c:110-127]: TX bg の `OUT_PERI_SEL`=**2**、TX fg の `OUT_PERI_SEL`=**3**、RX の `IN_PERI_SEL`=**2**。各チャネルについて §3.2〜3.3 を行う（TX は CONF0=0x1706、COLOR_CONVERT=0x3A。RX は CONF0=0x1704、auto_ret=0）。
5. **開始する順序** [ppa_blend.c:134-139]: 3 本の LINK_ADDR を書いてから、**TX bg → TX fg → RX** の順に start を書く。PPA の設定はそのあとで行う。
6. **PPA の設定** [ppa_blend.c:142-178]:

| レジスタ | フィールド | 値 | 根拠 |
|---|---|---|---|
| `BLEND_COLOR_MODE` 0x5008_7024 | blend0_rx_cm[3:0]（bg） | 2（RGB565） | PLL:680-681,704; SOCR/ppa_reg.h:265-268 |
| | blend1_rx_cm[7:4]（fg） | **6=A8、7=A4**（0=ARGB8888、1=RGB888、2=RGB565。4/5 の L8/L4 は ESP-IDF では未対応） | PLL:732-737,742; SOCR/ppa_reg.h:273-276 |
| | blend_tx_cm[11:8] | 2（RGB565） | PLL:761-762,779; SOCR/ppa_reg.h:281-284 |
| | → 書く値 | **0x262**（A8）/ **0x272**（A4）。bit12 以上は hw_ver1 では予約 | SOCR/ppa_struct.h:143 |
| `BLEND_BYTE_ORDER` 0x5008_702C | bit0 bg のバイト入れ替え、bit1 fg のバイト入れ替え、bit2 bg の RGB 入れ替え、bit3 fg の RGB 入れ替え | 0（入れ替えない。ESP-IDF の既定は false） | PLL:924-964; SOCR/ppa_reg.h:323-354 |
| `BLEND_FIX_ALPHA` 0x5008_7040 | bg: fix_alpha[7:0]、alpha_mod[17:16]、alpha_inv[20] | mod=0、inv=0（変えない）。RGB565 の元の α は 255 | PLL:978-981; SOCR/ppa_reg.h:452-458,468-472,484-488 |
| | fg: fix_alpha[15:8]、alpha_mod[19:18]、alpha_inv[21] | mod=0、inv=0 なら A8/A4 の値をそのまま α として使う。全体を薄くしたいときは mod=2、fix=k で α×k/256。固定値にしたいときは mod=1、fix=値 | PLL:1011-1036; SOCR/ppa_reg.h:460-499 |
| | → 書く値 | mod=0 のときは fix フィールドを書き換えない（既定値 0x80/0x80 が残る）ので、0x0000_8080 を書いてよい | PLL:1014-1017; SOCR/ppa_reg.h:452,460 |
| `BLEND_RGB` 0x5008_7048 | blend1_rx_b[7:0]、g[15:8]、r[23:16] | A4/A8 の前景色（RGB888）。書く値は `r<<16 \| g<<8 \| b` | PLL:1089-1094; SOCR/ppa_reg.h:504-525 |
| `CK_FG_LOW` / `CK_FG_HIGH` | B[7:0] G[15:8] R[23:16] | 使わないときは LOW=0xFFFFFF、HIGH=0x000000（空の範囲）。リセット後の既定値も同じ | ppa_blend.c:169-176; SOCR/ppa_reg.h:543-589 |
| `CK_BG_LOW` / `CK_BG_HIGH` | 同じ | 同じ | ppa_blend.c:171-173; SOCR/ppa_reg.h:595-641 |
| `CK_DEFAULT` 0x5008_7060 | RGB[23:0]、fg_bg_reverse[24] | 0 | ppa_blend.c:177-178; SOCR/ppa_reg.h:646-675 |

   - 式は `A_out = A_bg' + A_fg' − A_bg'·A_fg'`、`C_out = (C_bg·A_bg'·(1−A_fg') + C_fg·A_fg') / A_out` [PLL:589-595]。bg が RGB565（α=1）なら、C_out = C_bg·(1−α) + C_fg·α になる（推測。上の式から）。「RGB565/RGB888 の元の α は 255」[SOCR/ppa_reg.h:485-486]。
   - A4 の 1 バイトの中で、どちらのニブルが先の画素になるかは書かれていない（推測が要る。実機で確かめる）。
7. **開始**: `BLEND_TRANS_MODE` に bypass=0、fill_en=0、blend_en=1、update=1 を書く。書く値は **0x09** [ppa_blend.c:180; PLL:617-620,633-634]。ただし 0x01 を書いてから 0x09 を書く（§5.1 の 7 の訂正を参照）。
8. **完了**: RX の SUC_EOF を見る（§5.4）。デバッグには `PPA_BLEND_ST`（0x5008_7074）の bit0 `blend_size_diff_st`（「2 つの画像の大きさが違う」）が使える [SOCR/ppa_reg.h:805-809]。
- 出力のバッファは、入力（bg または fg）と同じポインタでもよい（ドキュメントに明記されている）[docs/en/api-reference/peripherals/ppa.rst:117]。

### 5.3 ESP-IDF が毎回行い、省いてもよさそうなもの
- `dma2d_connect` は、毎回チャネルを停止してリセットする [D2D:574-575,599-600]。設定が変わらないなら、リセットだけで足りるかもしれない（推測。確かめていない）。
- 完了後に `free_up_channels` が、ENA=0 と `peri_sel`=7、`in_mem_trans_en`=0 に戻す [D2D:178-209; DLL:461-465,969-972]。複数の用途でチャネルを共有するなら、同じようにする（判断）。

### 5.4 完了の検出（ポーリング）
- **RX の `IN_INT_RAW`**（ch0 は 0x5008_8504、ch1 は 0x5008_8604）を見る [SOCR/dma2d_struct.h:811-878; DLL:46-59]:
  - bit0 `in_done`（記述子 1 つ分の処理が終わった）、**bit1 `in_suc_eof`（完了）**、bit2 `in_err_eof`、bit3 `in_dscr_err`（owner、2 ワード目、3 ワード目の異常）、bit10 `in_dscr_empty`、bit4〜9 は FIFO のオーバーフロー/アンダーフロー、bit11〜12 は reorder、bit13 は dscr_task_ovf。
  - ESP-IDF は SUC_EOF、ERR_EOF、DSCR_ERR、DSCR_EMPTY のどれかが立ったら終わったとみなし、チャネルを解放する [D2D:309-313]。成功は SUC_EOF だけ [D2D:318-323]。「SUC_EOF/ERR_EOF が立つときは、必ず RX_DONE も同時に立つ」[D2D:291-292]。SUC_EOF のあとは、RX の FSM が idle になっている（assert）[D2D:310-311]。
- **クリア**: `IN_INT_CLR`（ch0 は 0x5008_8510、ch1 は 0x5008_8610）に、立っているビットを書く（ESP-IDF は ST を読んで、それを CLR に書く）[D2D:284-285; DLL:213-216]。TX 側も同じように `OUT_INT_CLR` をクリアする [D2D:264-265]。TX の INT_RAW: bit0 done、bit1 eof、bit2 dscr_err、bit3 total_eof [SOCR/dma2d_struct.h:97-118]。
- **PPA の割り込み**: `PPA_INT_RAW`（0x5008_7010）の bit1 `blend_eof`（「ブレンドエンジンが 1 フレームを計算し終えた」）、bit0 は SRM の eof、bit2 は SRM のパラメータエラー。クリアは `PPA_INT_CLR`（0x5008_701C）の同じビット [SOCR/ppa_reg.h:107-129,189-207]。ESP-IDF は PPA の割り込みを使っておらず、「SRM のパラメータが間違っていると 2D-DMA で止まり、終わらない」という TODO が残っている [ppa_core.c:91-92]。**完了の判定は RX の SUC_EOF で行い**、タイムアウトしたらチャネルとブレンドエンジンをリセットする（判断）。メモリへの書き込みが終わったと言えるのは RX の SUC_EOF のほう（推測）。

## 6. 単純な矩形コピー（RGB565 → RGB565、同じ大きさ）

### 6.1 方法の比較
| 方法 | 内容 | 判定 |
|---|---|---|
| **DMA2D M2M** | TX ch n → RX ch n。PPA を通さない。2D の記述子でブロックの位置を指定できる | **いちばん簡単**。ESP-IDF に実際の使用例がある（`esp_async_fbcpy`、DSI のパネルの draw_bitmap）[esp_lcd/src/esp_async_fbcpy.c:99-182; esp_lcd/dsi/esp_lcd_panel_dpi.c:531-557] |
| PPA BLEND の BYPASS | `blend_bypass`=1 で「背景をそのまま出力する」[PLL:47,625-627; SOCR/ppa_reg.h:367-370] | ESP-IDF には使っているところがない（`BYPASS_BLEND` を grep しても PLL 以外に出てこない）。fg の TX が要るかどうかもわからない（推測が要る） |
| PPA SRM（倍率 1） | TX/RX を dscr-port モード（`dscr_port_en`=1、`OUT_DSCR_PORT_BLK` に 18×18 など）にし、SRM のレジスタを設定する [D2D:581,606; ppa_srm.c:125-132; PLL:531-555] | 手順が多い。入力と出力は **別のバッファ** でなければならない [docs/en/api-reference/peripherals/ppa.rst:94] |

### 6.2 M2M の手順（ESP-IDF の `dma2d_m2m` と `esp_async_fbcpy` のとおり）
1. **チャネル**: TX と RX は **同じ番号** にする（`DMA2D_CHANNEL_FUNCTION_FLAG_SIBLING`「TX and RX channel with same channel ID」）[esp_hw_support/dma/include/esp_private/dma2d.h:118; esp_async_fbcpy.c:195-199; test_apps/dma2d/main/dma2d_test_utils.c:144-146]。`in_mem_trans_en` の説明は「同じチャネルのメモリ転送を有効にする」[SOCR/dma2d_struct.h:740-743]。rev < 3.0 で組にできるのは ch0 か ch1。
2. **キャッシュ**: 転送元を C2M で書き戻す（fbcpy の呼び出し元は `ESP_CACHE_MSYNC_FLAG_UNALIGNED` を使っている）[esp_lcd_panel_dpi.c:537-540]。テストでは、両方のバッファを始める前に C2M し、終わってから M2C している [test_dma2d.c:108-109,151-152]。転送先を CPU で読むなら、始める前に無効化しておくか、終わってから無効化する（§7）。
3. **記述子** [esp_async_fbcpy.c:106-132]（どちらも dma2d_en=1、suc_eof=1、owner=1、mode=0、pbyte=3、next=0）:
   - TX: buffer=転送元の画像の先頭、ha/va=転送元の画像の幅と高さ、hb/vb=コピーする大きさ、x/y=転送元のオフセット。
   - RX: buffer=転送先の画像の先頭、ha/va=転送先の画像の幅と高さ、hb/vb=同じ大きさ、x/y=転送先のオフセット。
   - テストでは RX の suc_eof を 0 にしても動いている [test_dma2d.c:372-376,634-638]。転送先だけを窓にする例（8x8 の中の (2,4) に 2x2）もある [test_dma2d.c:574-638]。
   - 書いたら C2M|INVALIDATE [esp_async_fbcpy.c:134-135]。
4. **接続**（§3.2〜3.3）[dma2d_test_utils.c:87-93; esp_async_fbcpy.c:163-169; D2D:554-557,573-621; DLL:451-455]:
   - TX: 停止 → リセット → `OUT_PERI_SEL`=**4**（4〜7）→ `OUT_CONF0`=0x1706（fbcpy の値は 0x1702）→ `OUT_COLOR_CONVERT`=0x3A → ENA=0、CLR=0x1FFF。
   - RX: 停止 → リセット → `IN_PERI_SEL`=**3**（3〜7）→ `IN_CONF0`=**0x1705**（bit0 の mem_trans_en=1）→ auto_ret=0 →（ch0 なら `IN_COLOR_CONVERT`=0x39）→ ENA=0、CLR=0x3FFF。
5. 両方の LINK_ADDR を書いて、**TX start → RX start** の順に開始する [esp_async_fbcpy.c:176-180; dma2d_test_utils.c:119-123]。
6. 完了は RX の `IN_INT_RAW` bit1 で判定する（§5.4）。終わったら、ESP-IDF と同じように `peri_sel`=7、`in_mem_trans_en`=0 に戻す [D2D:185,203; DLL:461-465]。

- **重なり**: 転送元と転送先が重なる場合（同じバッファの中でのスクロールなど）の動作は、ESP-IDF のソースにもドキュメントにも書かれていない。BLEND で「出力と入力が同じポインタ（同じ位置）」の場合だけが許されている [docs/en/api-reference/peripherals/ppa.rst:117]。SRM は別のバッファでなければならない [ppa.rst:94]。M2M で重なる転送をすると、TX が先に読み込むので、上方向のスクロール（dst < src）ならうまくいく可能性はある（推測）。保証はないので、使うなら実機で全画素を確かめる（判断）。
- M2M の TX で CSC（RGB565→RGB888 など）を使う例もある [test_dma2d.c:205-271]。ただし、その場合は CSC を持つチャネルを選ぶ必要がある。

## 7. 制約、PSRAM、キャッシュ

### 7.1 境界と大きさ
| 項目 | 値 | 根拠 |
|---|---|---|
| 記述子 | 8 バイト境界（HW）。ESP-IDF は内部 SRAM の 64 バイト境界、TCM 以外 | DLL:93; D2D:715; ppa_core.c:61-66 |
| RX のバッファ | 先頭と大きさが 4 バイト境界（PSRAM と AXI の制約） | dma2d_types.h:56; esp_private/dma2d.h:227 |
| PPA の出力バッファ | ESP-IDF は、先頭と大きさがキャッシュラインの倍数であることを要求する（内部は L1 ライン、PSRAM は L1 と L2 のライン）。これはキャッシュの無効化のため | ppa_fill.c:96-98; ppa_blend.c:193-195; esp_driver_ppa/include/driver/ppa.h:137-138; ppa_core.c:68-70; esp_mm/esp_cache_msync.c:154-170 |
| hb, vb, ha, va, x, y | 14 ビット（最大 16383 画素） | dma2d_types.h:24-50 |
| `PPA_BLEND_TX_SIZE` の hb/vb | 14 ビット（ESP-IDF は `PPA_BLEND_HB_V`/`VB_V` 以下であることを assert で確かめる） | SOCR/ppa_reg.h:437-446; PLL:1052 |
| 入れ子の条件 | block_w ≤ pic_w − off_x、block_h ≤ pic_h − off_y（入力も出力も） | ppa_fill.c:117-119; ppa_blend.c:225-241 |
| A4 の前景 | block_w と off_x が偶数 | ppa_blend.c:213-216 |
| PPA のバッファの場所 | 出力は内部 RAM か PSRAM（フラッシュは不可）。入力はどこでもよい（フラッシュも可） | ppa_blend.c:191-192; ppa_fill.c:94-95 |
| ecc/aes の領域 | `ecc_aes_en` を使うときは 16 ビット境界（今回は使わない） | SOCR/dma2d_struct.h:34-38 |
| page_bound_en | 1 にすると、AXI の読み出しが burst 長の境界を越えない | SOCR/dma2d_struct.h:63-66 |

### 7.2 PSRAM のアドレス
- ESP-IDF は、heap が返した PSRAM のポインタ（`SOC_EXTRAM_LOW`=0x4800_0000〜0x4C00_0000 [soc/esp32p4/include/soc/soc.h:152-153]）を **そのまま** 記述子の buffer に入れている [ppa_fill.c:52; ppa_blend.c:72,87,102; test_dma2d.c:85-86,112-121]。物理アドレスに変換したり、キャッシュを通らない別名を使ったりはしていない。つまり、DMA2D は CPU と同じ仮想アドレスで PSRAM にアクセスし、PSRAM の MMU（0x4800_0000 の窓、ページ 64KB [soc/esp32p4/include/soc/ext_mem_defs.h:19,96]）で変換される（推測。ESP-IDF の使い方からの推定。MMU を通ることを明記した箇所はない）。
- `SOC_NON_CACHEABLE_OFFSET` は 0x4000_0000 [ext_mem_defs.h:68]（CPU から見て 0x8800_0000 がキャッシュを通らない別名）。DMA2D のアクセス範囲の既定値（〜0x8FFF_FFFF）にも入っているが、ESP-IDF は DMA でこの別名を使っていない。DMA には 0x48xx_xxxx を渡すのが安全（判断）。
- 内部 SRAM の DMA 可能な範囲は 0x4FF0_0000〜0x4FFC_0000 [soc.h:189-191]。TCM は 0x3010_0000〜0x3010_2000 [soc.h:160-161]。

### 7.3 キャッシュの整合（ESP-IDF が行うこと）
- PSRAM のアドレスに C2M（書き戻し）をすると、**L1 D と L2 の両方** が書き戻される（`cache_ll_writeback_addr` は level 2 に対して L1 → L2 の順に処理する）[hal/esp32p4/include/hal/cache_ll.h:678-693]。M2C は `cache_ll_invalidate_addr`（`CACHE_TYPE_ALL`）[hal/cache_hal.c:266-278]。ここから、DMA はキャッシュを通らずにメモリを読み書きしていると考えられる（推測）。
- 境界: M2C（無効化）はアドレスと大きさがラインの倍数でなければならない（UNALIGNED は使えない）。C2M は UNALIGNED が使える [esp_cache_msync.c:113-125]。PSRAM で使う境界は `esp_cache_get_alignment(MALLOC_CAP_SPIRAM)`、つまり **L2 のライン**（ROM の `Cache_Get_L2_Cache_Line_Size()` の値。L1 D は 64B 固定）[esp_cache_msync.c:154-170; cache_ll.h:931-958]。カーネルのように L2 のラインを 128B にしている場合は、128B 単位になる（推測）。
- 操作ごとの範囲（行の全幅を含む「拡張窓」。ブロックの行だけを取り出すのではなく、`off_y` の行から `block_h` 行分の全幅）:

| 操作 | 範囲 | 種類 | 根拠 |
|---|---|---|---|
| FILL の出力 | `out + off_y*pic_w*bpp` から `pic_w*block_h*bpp`（L2 境界に広げる） | C2M + INVALIDATE（始める前） | ppa_fill.c:122-127 |
| BLEND の bg と fg | 同じ考え方で入力ごと | C2M（始める前） | ppa_blend.c:281-293 |
| BLEND の出力 | 同じ考え方 | M2C（始める前） | ppa_blend.c:294-298 |
| 記述子 | 64B の 1 ブロック | C2M（記述子を書いたあと） | ppa_fill.c:55; ppa_blend.c:105-107 |
| M2M のテスト | バッファ全体 | 始める前に C2M、終わってから M2C | test_dma2d.c:108-109,151-152 |

- ESP-IDF は、出力を始める前にだけ無効化し、終わったあとには何もしない。これは、実行中に CPU がその範囲に触れない（投機的な先読みもしない）ことを前提にしている（推測）。カーネルでは L1/L2 の自動先読みをフレームバッファより下にかけているので、フレームバッファが先読みの範囲に入らないかを確かめる必要がある（判断。doc/43 の設定を参照）。

## 8. rev < 3.0（hw_ver1）に当てはまること

ESP-IDF 6.0 の DMA2D と PPA にある `CHIP_SUPPORT_MIN_REV < 300` の分岐は次で全部（`esp_driver_ppa/src`、`D2D`、`esp_hal_dma` を grep して確認）:

| 項目 | rev < 3.0 | 根拠 |
|---|---|---|
| チャネル数 | TX 3、RX 2（rev 3.0 以上は 4/3） | DLL:26-36 |
| TX の CSC | ch0〜2（rev 3.0 以上は ch0〜3） | DLL:77-81 |
| 割り込み源 | OUT_CH3 と IN_CH2 はない | esp_hal_dma/esp32p4/dma2d_periph.c:17-26 |
| RX ch1 の ETM_CONF | 0x648（rev 3.0 以上と位置が違う） | DLL:682; SOCR/dma2d_struct.h:1766-1773; SOCR/dma2d_reg.h:5019 |
| BLEND の YUV420/YUV422/GRAY8 | 使えない（関数は abort） | PLL:653-657,689-699,764-774,790-805 |
| RGB→GRAY の係数 | 使えない（abort） | PLL:96-104 |
| SRM の macro block | 16x16 だけ（32x32 は rev 3.0 以上） | PLL:36-38,488-519 |
| `BLEND_COLOR_MODE` の bit12 以上 | 予約（YUV の範囲や規格のビットはない） | SOCR/ppa_struct.h:143; SOCR/ppa_reg.h:264-288 |
| PSRAM の速度 | 250MHz は選べない（200MHz まで） | esp_psram/esp32p4/Kconfig.spiram:34 |
| 性能の目安（テストの閾値） | SRM 21 Mpx/s（rev 3.0 以上は 35）、BLEND 31.5 Mpx/s、FILL 150 Mpx/s（rev による違いはない） | esp_driver_ppa/test_apps/main/ppa_performance.h:18-30 |

- rev < 3.0 だけの「DMA2D/PPA のエラッタ対策」に当たる処理は、ESP-IDF 6.0 には見当たらない。上の表は機能の有無の違いだけ。
- FILL の色の扱いが、ドライバのコメント（「色変換を通らない」[PLL:1041-1042]）とテストの結果（ARGB8888 → RGB565 に切り詰める [test_ppa.c:516-559]）で食い違っている。テストは rev を問わず実行される（rev で分岐しているのは YUV の部分だけ [test_ppa.c:563-575]）。したがって、`FIX_PIXEL` には **ARGB8888 で書く** のが正しい（推測）。

## 9. 残っている不明点
1. M2M で転送元と転送先が重なるときの動作（スクロールに使えるか）。資料がない。
2. BLEND の BYPASS で、fg の TX チャネルを省けるか。
3. A4 の、1 バイトの中でのニブルの順序。
4. `PPA_REG_CONF.clk_en` が不要かどうか（ESP-IDF は書いていない）。
5. DMA2D が PSRAM の MMU を通ること、L2 を通らないこと（ESP-IDF の使い方とキャッシュ操作から推定した）。
6. `*_INT_RAW` が ENA=0 でも立つこと（ポーリングの前提）。
