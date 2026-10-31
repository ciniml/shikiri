# 22. N2b: Wi-Fi（ESP32-C6、SDIO、esp_hosted）

[research/net-stack.md](research/net-stack.md) §1.1 と §4 の段階 N2b。Tab5 の ESP32-C6 を SDIO でつなぎ、esp_hosted の上で Wi-Fi の STA にする。

利用者の判断（2026-10-31）:
- 「C6の電源投入はおこなってよいです」: IO エクスパンダ #2（0x44）の bit0（WLAN_PWR_EN）を立ててよい。bit4（電源断）は変えない。
- 「C6のファームウェアはそのままで試してみてください」: C6 のファームウェアは書き換えない。

## 1. 区切り
| 区切り | 内容 | 状態 |
|---|---|---|
| N2b-1 | 電源、リセット、SDMMC slot1 での SDIO の識別（CMD5/3/7、CCCR、function 1） | 実機で確認（§4） |
| N2b-2 | 4 ビット・20MHz、function 1 のブロック長、SLC ホストのレジスタ、esp_hosted の INIT イベント（C6 のファームウェアの版） | 次 |
| N2b-3 | 送受信（CMD53）、`netdev:wlan0`、`wifi` サービスの RPC（接続）、DHCP | |

## 2. 電源とリセット（kernel/src/platform/esp32p4v1/tab5.rs `wlan_power_on`）
- エクスパンダ #2 の OUT（0x05）、DIR（0x03）、OUT_H_IM（0x07）を読んでログに出す（`wlan.expander`）。bit0 が「出力・High・高インピーダンスでない」なら、もう電源は入っているので**何も書かない**。
- 入っていなければ、レジスタごとに 1 回の読み・変更・書きで **bit0 だけ**を変える（OUT → DIR → OUT_H_IM の順。ピンがすぐ High になる）。書いた後に読み直し、bit0 以外が 1 つでも変わっていたら `wlan.expander_changed`（Error）を出して止まる。
- TCC は #2 をチップリセットしてから全部を書き直している（research §1.1）。これは真似しない（判断）。
- その後、CHIP_EN（GPIO15）を Low にして（電源が入ったままなら 50ms、入れたばかりなら 300ms）High に戻し、1.5 秒待つ（TCC の `wifi_setup.cpp:117-121` と同じ待ち）。待ちの間は 100ms ごとにウォッチドッグを止めないように蹴る。

## 3. SDMMC の 2 つのスロット（kernel/src/platform/esp32p4v1/sd.rs、sdio.rs）
- ホスト（0x5008_3000）は SD カード（slot0、M7）と共有する。ホストの初期化（クロック、リセット、IDMAC）は 1 回だけにした（`host_up`）。
- **クロックの第 1 段（160MHz の分周）を 40MHz に固定**し、スロットごとの分周器（CLKDIV の 8 ビットの欄。slot n は分周器 n、CLKSRC で選ぶ）で各スロットのクロックを作る（判断）。片方のスロットのクロックを変えても、もう片方に響かない。
  - SD: 400kHz（分周 50）→ 20MHz（分周 1）→ 40MHz（素通し）。周波数は M7 と同じで、作り方だけが変わった。M7 では第 1 段を 16 → 20 → 40MHz と変えていた。
  - SDIO: 識別は 400kHz（分周 50）。
  - CLKENA と CTYPE（バス幅）はスロットのビットだけを変える。クロック更新のコマンドとコマンド本体には、カード番号（CMD の bit20:16）を入れる。
  - 変更の後、実機で SD を試し直した: `sdinfo` と `sdtest` が通り、読み 5578KiB/s、書き 3954KiB/s（M7 のときは 5.4MB/s と 3.3MB/s）。
- slot1 のピンは GPIO マトリクス経由（IO_MUX は MCU_SEL 1、駆動 3、入力有効、プルアップ）: CLK=GPIO12（信号 0）、CMD=GPIO13（信号 1）、D0〜D3=GPIO11/10/9/8（信号 2〜5）。出力の有効はホストに任せる（OE_SEL 0）。カード検出（信号 127）と書き込み保護（131）は定数の Low、割り込み（129）は定数の High（`gpio_sig_map.h` の `SD_CARD_*_2_PAD`）。
- コマンド（slot0 の値に slot を足す）: CMD0、CMD5（R4、CRC を見ない）、CMD3（R6）、CMD7、CMD52（R5）。CMD52 の応答の R5 のフラグ（COM_CRC_ERROR、ILLEGAL_COMMAND、ERROR、FUNCTION_NUMBER、OUT_OF_RANGE）が立っていたら誤りにする。
- 手順（`wlanprobe`）: CCCR の I/O ABORT に RES を書く（失敗は無視）→ CMD0 → CMD5（引数 0）で OCR → 準備ができるまで CMD5 → CMD3 で RCA → CMD7 → CCCR の 0x00〜0x13 を CMD52 で読む → I/O ENABLE に function 1 を足し、I/O READY を待つ。

## 4. 実機の結果（N2b-1、2026-10-31）
```
wlan.expander out=1 dir=185 out_h_im=6
sd.clock slot=1 khz=400
wlan.ocr r4=553647872 functions=2 memory=false
wlan.card rca=1 ready_tries=2 was_on=true cccr=3202000200000040170010000000000000000101
wlan.func1 io_ready=2 ready=true
```
- エクスパンダ #2 の bit0 は**最初から立っていた**（OUT=0x01、DIR=0xB9、OUT_H_IM=0x06）。このため #2 には**書いていない**。リセットは GPIO15 だけ。誰が立てたのか（電源投入時の既定か、以前のファームウェアが残した状態か）は未確認。エクスパンダは SoC のリセットでは状態を保つ（推測）。
- R4 = 0x2100_0000: I/O の機能は 2 個、メモリ部なし。OCR の欄（23:0）が 0 だった。それでも 2 回目の CMD5 で準備完了になった（推測: C6 のスレーブは OCR を見ていない）。
- CCCR（0x00〜0x13）:
  - 0x00 = 0x32: CCCR の版 2、SDIO の版 3。
  - 0x07 = 0x40: 今は 1 ビットのバス。
  - 0x08 = 0x17: 複数ブロック（SMB）、4 ビットの割り込み（S4MI）ほか。
  - 0x09〜0x0B: 共通の CIS は 0x00_1000。
  - 0x12 = 0x01、0x13 = 0x01: High Speed に対応（SHS）。
- function 1 は有効にした直後に準備完了だった。

## 5. まだ
- N2b-2: 4 ビットにして 20MHz に上げる（TCC は 40MHz で CMD5 がタイムアウトしたので 20MHz、research §1.1）。function 1 のブロック長を 512 にする。SLC のレジスタを読み、C6 から INIT イベント（PRIV、0x22）を受けてファームウェアの版を確かめる（1.4.x か 2.x か）。
- C6 の電源を入れた・入っていた状態を、M5Stack の元のファームウェア（バックアップ）がどう扱うかは確かめていない。
