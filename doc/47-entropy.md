# 47. 乱数のエントロピー源（SAR ADC）

利用者の指示（2026-11-03）:「entropysourceよさそう。すすめて」（doc/46 §5 の表の「乱数のエントロピー源」）

## 1. 何のためか
- カーネルの `random`（doc/46 §4）は、RNG のレジスタ（`LP_SYSTEM_REG_RNG_DATA_REG`、0x5011_01A4）を読む。
- ESP-IDF によれば、このレジスタが真の乱数になるのは、エントロピー源が動いているときだけ。
  - ESP32-P4 では、エントロピー源は SAR ADC（`bootloader_random_esp32p4.c`）。
  - Wi-Fi の無線は C6 にあるので、P4 の側では無線の雑音を使えない。
- これまでは動かしていなかったので、`random` は鍵（TLS、SSH）に使わない、としていた。

## 2. 手順（`kernel/src/platform/esp32p4v1/entropy.rs`）
ESP-IDF 6.0 の `bootloader_random_enable` と同じ順に、同じ値を書く。

1. ADC をリセットし、クロックを入れる（`HP_SYS_CLKRST`）。
   - `HP_RST_EN2` の `rst_en_adc` を立てて下ろす。
   - `SOC_CLK_CTRL2` の `adc_apb_clk_en`、`PERI_CLK_CTRL23` の `adc_clk_en`。
   - クロック源は XTAL（`PERI_CLK_CTRL22` の `adc_clk_src_sel` = 0）、分周なし。
2. ADC のアナログのレジスタへの regi2c を使えるようにする（`PMU_RF_PWC`）。
   - `perif_i2c_rstb` を下ろし、`xpd_perif_i2c` を立て、`perif_i2c_rstb` を立てる。
   - アナログ I2C のマスタのクロック（`LPPERI_CLK_EN` の `ck_en_lp_i2cmst`）は、入れたままにする。ESP-IDF のアプリも、乱数のために入れたままにする。
3. regi2c（スレーブ 0x69、`ANA_CONF2` の bit 7）で、ADC のアナログの設定。
   - `DTEST_VDD_GRP1` = 0、`ENT_VDD_GRP1` = 1（`adc_ll_regi2c_init`）。
   - ADC1 の初期コード = 2166。
4. ADC1 のパターン表の 4 つに、channel 10、減衰 12 dB を入れ、長さを 1 にする。
5. ADC1 をデジタルの制御器につなぎ（`LP_ADC` の `sar1_dig_force` など）、電源を入れる（`xpd_sar1_force` = 3）。
6. ADC のクロックの分周 15、タイマーの間隔 100 で、タイマーで変換を続ける。

- ピンには触らない。
  - ADC1 の GPIO の channel は 0〜7（GPIO16〜23）。channel 10 はその外。
  - パッドの選択（`sar1_en_pad`）は 0 のまま。
- regi2c の関数は、PSRAM の MPLL で使っていたもの（`psram.rs`）を、スレーブを引数に取る形にして共用した。書き込みのマスク付きの版（`regi2c_write_mask`）を足した。
- 止める処理（`bootloader_random_disable`）は作っていない。このカーネルは ADC をほかに使わない。
- rev 3.0 以上の P4 にある `LP_TRNG` は、Tab5 の P4（v1.x）にはない。ESP-IDF でも rev 3.0 以上だけで動かす。

## 3. いつ動かすか
- `clock_init` の最初（PSRAM の後、PVT とクロックの前）。
  - PVT がないときの早い戻りより前に置いた。クロックがどうなっても動かすため。
  - アプリ（`net` など）が `random` を読むのは、これより後。
- 動かした後、20 µs おきに ADC1 の値（`SAR1_DATA_STATUS`）を 8 回読み、違う値の数を `rng.entropy`（`distinct`）に出す。変換が進んでいることの目安。
- 失敗（regi2c が busy のまま）なら `rng.no_entropy`。そのときも起動は続ける。

## 4. 結果
- 実機（2026-11-03）:
  - `rng.entropy source=sar_adc distinct=4`、再起動の後は `distinct=3`。ADC の値が揺れている。
  - 自己試験は pass。`net` は一時アドレス（`2001:db8:0:1:980c:300e:2c09:48ca`）を作った。ps で全プロセスが動いている。
- t5emu: ADC のレジスタの模型はない（読むと 0）。`distinct=1` で起動は通る。
- RNG の出力の統計的な検定（NIST SP 800-22 など）はしていない。ホストへ大量の乱数を出す手段がまだない。

## 5. 残り
- 乱数をホストへ取り出して検定する（`random` を読んで出すアプリ、または ctl のコマンド）。
- ESP-IDF の `esp_random` は、エントロピー源があっても読み出しの間隔を空ける（約 14 µs）。カーネルもそうしている（`random_u32`）。
