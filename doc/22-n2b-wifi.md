# 22. N2b: Wi-Fi（ESP32-C6、SDIO、esp_hosted）

[research/net-stack.md](research/net-stack.md) §1.1 と §4 の段階 N2b。Tab5 の ESP32-C6 を SDIO でつなぎ、esp_hosted の上で Wi-Fi の STA にする。

利用者の判断（2026-10-31）:
- 「C6の電源投入はおこなってよいです」: IO エクスパンダ #2（0x44）の bit0（WLAN_PWR_EN）を立ててよい。bit4（電源断）は変えない。
- 「C6のファームウェアはそのままで試してみてください」: C6 のファームウェアは書き換えない。

## 1. 区切り
| 区切り | 内容 | 状態 |
|---|---|---|
| N2b-1 | 電源、リセット、SDMMC slot1 での SDIO の識別（CMD5/3/7、CCCR、function 1） | 実機で確認（§4） |
| N2b-2 | 4 ビット・20MHz、function 1 のブロック長、SLC ホストのレジスタ、esp_hosted の INIT イベント（C6 のファームウェアの版） | 実機で確認（§5）。**C6 は esp_hosted 2.12.7** |
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

## 5. N2b-2: esp_hosted の転送の始まり（sdio.rs）
esp_hosted 2.12.9 のホスト（TCC の managed_components、research の `HOSTED/`）の手順をなぞる。`wlanprobe` の続きとして実装した（ドライバとしての形は N2b-3 で作る）。

- カードの設定（`port_esp_hosted_host_sdio.c:150-218`）: CCCR の INT_ENABLE に function 1 とマスタの有効を足す。function 0 と 1 のブロック長を 512（CCCR 0x10、FBR1 0x110）。バス幅を 4 ビット（CCCR 0x07 の下 2 ビット = 2、ホストの CTYPE の bit1）。クロックを 20MHz（分周 1）。TCC が 20MHz にしているのに合わせた（40MHz では CMD5 がタイムアウトした、research §1.1）。
- CMD53（IO_RW_EXTENDED、function 1、アドレスを増やす）: 512 の倍数はブロックモード、それ以外は 512 バイトまでのバイトモード。SLC のレジスタは「アドレス & 0x3FF」で読む（`sdio_reg.h:87-94`）: TOKEN_RDATA 0x044、INT_RAW 0x050、PACKET_LEN 0x060、INT_CLR 0x0D4、ホストからの割り込み（SCRATCH_REG_7）0x08C。
- データの経路を開く: 0x08C に `1 << ESP_OPEN_DATA_PATH`（= 1）を CMD52 で書く（`sdio_drv.c:395-405,1146`）。
- 受信（`sdio_drv.c:1096-1290` を、カードの割り込みの代わりにポーリングで）: INT_RAW を読み、**使うビット（bit23 の新パケット、bit7/bit6 の送信抑制）だけ**を INT_CLR で消す。新パケットなら PACKET_LEN（下 20 ビット）から今までに読んだバイト数を引いた分が長さ。`0x1F800 − 長さ` から、512 の倍数に切り上げてブロックで読む（C6 が 0 で埋める）。
  - 初めは INT_RAW の全ビットを消していたが、**bit19 は消してもすぐに立ち直る**（実機）。esp_hosted のホストも bit23/7/6 しか見ないので、それ以外は無視する。
- ペイロードのヘッダ（12 バイト）: if_type（下 4 ビット）、if_num、flags、len、offset、checksum、seq_num、throttle、priv_pkt_type。INIT イベントは if_type = PRIV（5）、priv_pkt_type = 0x33、本体の先頭が 0x22 で、その後ろはタグ・長さ・値の並び（`transport_drv.c:802-880`）。

### 5.1 実機の結果（2026-10-31）
```
wlan.slc int_raw=4737024 packet_len=0 tx_buffers=20 bus_if=66
wlan.rx if_type=5 if_num=0 flags=0 len=32 offset=12 seq=0 priv_type=51 head=050020000c00000000000033221e12010d18010111010d16...
wlan.chip_id id=13
wlan.sdio_mode streaming=true
wlan.init_tag tag=17 value=0d          (ESP_PRIV_CAPABILITY)
wlan.init_tag tag=22 value=00000000    (ESP_PRIV_CAP_EXT)
wlan.init_tag tag=19 value=00          (ESP_PRIV_TEST_RAW_TP)
wlan.init_tag tag=21 value=14          (ESP_PRIV_TX_Q_SIZE)
wlan.init_tag tag=20 value=14          (ESP_PRIV_RX_Q_SIZE)
wlan.fw_version raw=134151             (= 0x00020C07)
wlan.rx if_type=3 if_num=0 flags=0 len=22 offset=12 seq=1 priv_type=0 head=030016000c000000010000000106005250434576740...
```
- **C6 のファームウェアは esp_hosted 2.12.7**（`(major << 16) | (minor << 8) | patch`、`esp_hosted_host_fw_ver.h:23-28`）。research の推測（TCC が 2.x に書き換えた）のとおり。TCC のホストは 2.12.9 なので、同じ 2.12 系。
- チップ ID = 13（ESP32-C6）。SDIO はストリーミングモード。capabilities = 0x0D（TCC のログの `capabilities: 0xd` と同じ）。送受信の待ち行列はどちらも 20。
- 2 つ目は SERIAL（if_type 3）の RPC: TLV で端点名 `RPCEvt`、データ `08 03 10 81 06 8a 30 02 10 01`（protobuf の `Rpc`: msg_type = 3（イベント）、msg_id = 769 = `Event_ESPInit`、`esp_hosted_rpc.proto:465,2445`）。
- 送信のバッファ（TOKEN_RDATA の bit27:16）は 20。

## 6. まだ
- N2b-3: ポーリングをやめてカードの割り込み（SDMMC の `sdio_int_mask`）を使う。送信（CMD53 の書き込み、TOKEN_RDATA のバッファ数を見る）。RPC（protobuf）で `WifiInit` から `WifiConnect` まで。STA のフレームを `netdev:wlan0` に出し、`net` で DHCP。
- C6 の電源を入れた・入っていた状態を、M5Stack の元のファームウェア（バックアップ）がどう扱うかは確かめていない。
