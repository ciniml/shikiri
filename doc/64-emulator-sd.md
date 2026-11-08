# 64. エミュレータの microSD

利用者の指示（2026-11-08）:「エミュレータにSDカードつけましょう。」

## 1. 作り（`emu/src/sdmmc.rs`）
- **ホスト**: SDMMC（Synopsys DesignWare MMC、0x5008_3000）を、カーネルの `platform/esp32p4v1/sd.rs` が使う範囲で。CMD に start ビットが書かれた時点でコマンドを済ませ（CMD_DONE、応答、データなら DTO）、データは内部 DMA（IDMAC）の記述子（DBADDR、DES2 がバッファ、長さは BYTCNT）でメモリそのものに読み書きする（キャッシュは通らない、doc/44 の 2D と同じ）。RINTSTS と IDSTS は 1 を書いて消す。CTRL のリセットはすぐ終わる。STATUS は忙しくならない。CDETECT はカードがあれば 0。
- **カード**: SDHC（ブロック番地）。CMD0、8、55、ACMD41（すぐ電源が上がる、CCS）、CMD2（CID: 製造者 0xEE、"T5"、"T5EMU"）、3（RCA 0x5E01）、9（CSD 2.0、大きさは像から）、7、13、16、12、ACMD6、ACMD51（SD 2.0、1 本と 4 本）、CMD6（High Speed）、CMD17/18（読む）、CMD24/25（書く）。ほかは応答なし。
- **像**: `--sd FILE`（生のディスク像、512 バイトの倍数）。書き込みはメモリの上だけで、`--sd-out FILE` で終わりに保存する。像がないとき、またスロット 1（Wi-Fi の C6 の SDIO）は応答なし（カードがないのと同じ）。
- 終わりに `t5emu: SD: N command(s), R sector(s) read, W written`。
- カーネルの SD のクロックの確かめ（PLL_F160M の分周）のため、REF_CLK_CTRL0 の初期値をブートローダの残す値（分周 2）にした。

## 2. 試験用の像（`tools/mksdimg.sh`）
- 64 MiB、MBR に FAT32 の区画 1 つ（2048 セクタから）。mtools（`mformat`、`mcopy`、`mmd`、`@@1M` の位置指定）で作るので、PC が書いたのと同じ長い名前の項目になる。
- 中身: `Long File Name.txt`、`sixty-lines-of-text.txt`（60 行）、`My Documents/notes-about-tab5.txt`、`SHORT.TXT`。
- `make` は `build/sd-test.img` を作る（p4emu-prep）。

## 3. 試験（p4emu-run-33）
- `fs` を起動（`fs.ready`、カーネルの `sd.card name=T5EMU`、`sd.bus four_bit=true high_speed=true`）、`term` で `cat My\ Documents/notes-about-tab5.txt` と `cat "long file name.txt"`（`shell.cat`）、`write new.txt made in emu`。終わった後の像を mtools で読み、`NEW.TXT` の中身を確かめる。
- 手で動かしたときの画面（`ls` の長い名前、Tab 補完の `My\ Documents/`）も確かめた。

## 4. shell の空白（doc/63 と一緒に）
- 単語は空白で分けるが、`\ ` と `"..."` で空白を含められる（`complete::split`）。Tab 補完は名前の空白を `\ ` にして入れる。共通の頭は文字の途中で切らない。Backspace は UTF-8 の 1 文字を消す。
