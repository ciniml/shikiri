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
| N2b-3a | 転送のドライバ（受信の切り出し、送信）、`wlan:ctl` と `netdev:wlan0`（ABI 0.12）、`wifi` サービス（RPC で初期化から接続まで） | 実機で AP に接続（§6） |
| N2b-3b | `net` に Wi-Fi のリンク（Ethernet、DHCP）、外への ping と TCP | 実機で DHCP、HTTP、PC からの ping（§7）。ping と DNS の API、SD との並行（§7.4）。**N2b の完了条件を満たした** |

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

## 6. N2b-3a: 転送、オブジェクト、`wifi` サービス
### 6.1 カーネル
- **転送**（sdio.rs の `up`/`poll`/`send`）。`wlanprobe` の手順を `up` にまとめ、終わりにデータの経路を開く。
  - 受信: INT_RAW の bit23 を見て、`PACKET_LEN` との差の分を 4KiB（DMA の中継バッファの大きさ）ずつ読み、8 ブロック分のストリームのバッファにためる。esp_hosted のストリーミングモードでは、パケットはヘッダ（12 バイト）と本体が隙間なく並ぶ（`sdio_drv.c:967-1026`）。ヘッダの `len` で 1 つずつ切り出す。`offset` が 12 でない、`len` が 0 か 1536 を超えるときは、esp_hosted と同じく残りを捨てる（`wlan.bad_packet`）。
  - PRIV（INIT イベント）はここでログに出す。それ以外は種類（if_type）と本体を上に渡す。
  - 送信: ヘッダ（if_type、len、offset = 12、seq_num。チェックサムは使わない。TCC の sdkconfig で `ESP_HOSTED_SDIO_CHECKSUM` が無効で、受け取ったヘッダのチェックサムも 0）を付け、512 の倍数に 0 で埋めて `0x1F800 − 全長` へ CMD53 で書く。C6 の空きバッファは TOKEN_RDATA の bit27:16 から使った数を引いて求め、無ければ `Busy`（`sdio_drv.c:420-440,531-580`）。送信の抑制（bit7）の間も `Busy`。
- **オブジェクト**（kernel/src/wlan.rs、ABI 0.12）:
  - `wlan:ctl`（`wifi` サービス用）: SERIAL（if_type 3）の本体＝esp_hosted の TLV（端点名と RPC）を 1 つずつ `read`/`write`。最初の `open` で C6 を起動する（約 2 秒。この間カーネルは止まる。判断: 起動は 1 回だけなので許す）。`ctl(SET_MAC)` で局の MAC アドレスをカーネルに教える。
  - `netdev:wlan0`（`net` 用）: STA（if_type 1）の本体＝Ethernet のフレーム（FCS なし）。`ctl(MAC_LO/MAC_HI)` で MAC アドレス（未設定なら `Busy`）。
  - どちらかが開いている間、スケジューラのループごとに `poll` で C6 を読む（INT_RAW を CMD53 で 1 回読む）。カードの割り込みはまだ使っていない。
  - 受信の待ち行列は内部 SRAM の残りで決めた: RPC 3 個、フレーム 5 個。8 個と 4 個ではカーネルの RAM が 4480 バイト足りなかった。

### 6.2 `wifi` サービス（user/apps/wifi、`--permit wlan:ctl`）
- RPC は protobuf の `Rpc { msg_type = 1, msg_id = 2, uid = 3, oneof payload（番号 = msg_id） }`（`esp_hosted_rpc.proto:2153-2452`）。必要なメッセージだけを手で組み立て・読む（ネストは一時バッファで長さを先に決める）。応答の id は要求の id + 256。
- 手順: ESPInit のイベントを待つ（3 秒）→ `WifiInit` → `SetMode(STA)` → `GetMacAddress(STA)`（`ctl(SET_MAC)`）→ `WifiSetConfig(STA: ssid, password, pmf capable)` → `WifiStart` → `WifiConnect` → イベントを待ち続ける。切断されたら 5 秒後に `WifiConnect` をやり直す。
  - 最初は `GetMacAddress` を先にしていたが、C6 は `ESP_ERR_WIFI_NOT_INIT`（0x3001）を返した。初期化の後に移した。
  - `WifiInit` の設定値は、C6 がそのまま使う（`slave_wifi_std.c:150-198,423-470`）。TCC のホストが送る値（TCC の sdkconfig の `CONFIG_WIFI_RMT_*`: 静的受信 10、動的受信 32、動的送信 32、AMPDU 送受、NVS、BA の窓 6、管理用 32、magic 0x1F2F3F4F など）に合わせた。
- **接続先**: リポジトリの外のファイル（`$T5_WIFI_FILE`、既定は `~/.config/tab5-os/wifi.txt`。1 行目が SSID、2 行目がパスフレーズ）を build.rs が読み、アプリのイメージに埋め込む（判断。SD カードの設定ファイルに移すまでのつなぎ）。make の変数を通さないので、コマンドの表示にも出ない。パスフレーズはログに出さない。ファイルが無ければ接続せず `wifi.no_credentials` を出す。
  - イメージ（build/apps/wifi.t5a と user/target の ELF）にはパスフレーズが平文で入る。どちらも git の管理外。

### 6.3 実機の結果（2026-10-31）
```
wlan.fw_version major=2 minor=12 patch=7
wifi.chip_init reset_reason=1
wifi.mac mac=e4b3239a1f25
wifi.configured ssid=<利用者の AP>
wifi.event event_id=2      (WIFI_EVENT_STA_START)
wifi.event event_id=43
wifi.connected ssid=<利用者の AP> bssid=… channel=11 authmode=6 aid=42
```
- `WifiConnect` から約 0.7 秒で接続した。authmode 6 は ESP-IDF の `WIFI_AUTH_WPA3_PSK`。
- `wifi.mac` の …:25 は soft-AP の MAC で、引数の取り違えによるもの（§7.2）。STA の MAC は …:24。
- `wifi` はその後も待ち続けている（`ps`: state=waiting）。カーネルのスタックは 40976/49152。

## 7. N2b-3b: `net` の Wi-Fi のリンク
### 7.1 作ったもの
- `net`（user/apps/net）に 2 つ目の smoltcp のインタフェース（`Medium::Ethernet`、MAC は `netdev:wlan0` の `ctl(MAC_LO/HI)`）と、それ専用のソケット集合、DHCP のソケット（smoltcp の `socket-dhcpv4`）を足した。`net` は `--permit netdev:wlan0`。
  - ソケットはどちらのインタフェースのものかを持つ。TCP の接続は宛先が 127/8 でも 10.55.0.0/24 でもなければ Wi-Fi 側に作る。待ち受けと UDP は今は最初のインタフェースだけ。
  - `netdev:wlan0` は Wi-Fi の転送が起動するまで開けない（`NoDevice`）。`net` は `wifi` の後に起動する。
  - 試験用に `run net ADDR ROUTER`（数値の IPv4）で DHCP の代わりに静的な ADDR/24 を使える。
- `nettest 2 ADDR`: ADDR の 80 番に HTTP/1.0 の GET を送り、応答の 1 行目と大きさを記録する。
- 接続先は `wifi` のビルド時に `$T5_WIFI_FILE`（なければ `~/.config/tab5-os/wifi.txt`）から読む（§6.2）。

### 7.2 データが通らなかった原因（2026-10-31、解決）
- 症状: AP には接続できるが、DHCP の応答がなく、自分あてのユニキャストが 1 つも来ない。ブロードキャストの受信はできる。送ったフレームは C6 への書き込みまでは成功する。
- 切り分け: PC と Tab5 を同じ AP（WPA2 の試験用 AP）につなぎ、
  - PC から UDP のブロードキャストを送ると Tab5 の受信の数が増える（受信は正常）。
  - Tab5 に静的なアドレスを与えて PC あてに通信させても、PC の近隣表（`ip neigh`）に Tab5 が現れない（Tab5 の ARP の要求が無線に出ていない）。
- 原因: **`GetMACAddress` の引数を取り違えていた**。C6 はこの値を `esp_wifi_get_mac` にそのまま渡す（esp_hosted の `slave_wifi_std.c:506`）ので、`wifi_interface_t`（STA = 0、AP = 1）である。`wifi` は `WIFI_MODE_STA`（= 1）を渡していたため、**soft-AP の MAC（…:25）を受け取り**、`net` はそれを送信元として使っていた。STA の MAC は …:24。
  - 送信元が自分の STA の MAC と違うフレームは無線に出ない。AP はこの局を …:24 として知っているので、ユニキャストも来ない。ブロードキャストだけが見えていたのはこのため。
- 対処: `wifi` は `WIFI_IF_STA`（0）を渡す。`net` に入れていた DHCP の「応答はブロードキャストで」の旗は外した。
- 以前の iotap（WPA3）で毎回 202 → 205 → 接続となっていた件は別の問題で、未解決（WPA2 の AP では 1 回目で接続する）。

### 7.3 実機の結果（2026-10-31、AP「matter-test」、WPA2）
- 接続まで約 7 秒（電源投入とリセットの 1.5 秒を含む）、1 回目で接続。
- DHCP で 192.168.8.181/24（ゲートウェイと DNS は 192.168.8.1）を得た。
- `nettest 2 192.168.8.1`: 接続 6〜9 ms、HTTP の応答 1084 バイトを 17〜18 ms で受信（`HTTP/1.1 200 OK`）。
- PC（同じ AP）から Tab5 への ping: 5/5 応答、28〜208 ms（C6 の省電力と、カーネルが C6 をポーリングしていることによる遅れと考える。未計測）。

### 7.4 ping、DNS、SD との並行（2026-11-01、AP「iotap」、WPA3）
`svc:net` に 4 つの操作を足した（`abi/src/net.rs`）。
- `PING`/`PING_RESULT`: エコー要求を 1 つ送り、応答と往復の時間（µs）を返す。smoltcp の ICMP のソケット（`socket-icmp`）を、スロットごとの識別子（0x7450 + スロット番号）で使う。
- `RESOLVE`/`RESOLVE_RESULT`: DNS の A レコードを引く。smoltcp の `socket-dns` を Wi-Fi 側に 1 つ作り、DHCP で得た DNS サーバ（最大 3 つ）を使う。同時に引ける名前は 4 つまで。静的なアドレスで起動したときは使えない（`UNREACHABLE`）。
- どちらも、ほかのソケットと同じく `CLOSE` で返す。`t5-rt` の `Net::ping`、`Net::resolve` が待ちまで行う。`nettest 3 ADDR` で 4 回 ping、`nettest 4` で 3 つの名前を引いて、最初の名前に HTTP の GET を送る。

実機の結果:
- iotap でも 1 回目で接続した（以前の 202 → 205 は起きなかった。原因は分からないまま）。DHCP で 192.168.2.112/24。
- `nettest 3 192.168.2.254`（ゲートウェイ）: 4/4 応答。往復は 1.9〜11 ms。
- `nettest 4`: example.com、one.one.one.one、www.espressif.com を 14〜24 ms で解決。example.com への HTTP は応答 358 バイト（`403 Forbidden`、CDN の応答。通信自体は成功）を 21 ms。
- `nettest 2 1.1.1.1`: 応答 358 バイトを 37 ms。インターネットまで出られる。
- **SD との並行**: `fs` と `fstest 2048`（SD に 2 MiB を書いて読み戻して比べる）を動かしている間に、PC（有線で同じ LAN）から Tab5 へ 50 ms ごとに ping を 600 回、Tab5 からは `nettest 4` を動かした。
  - `fstest` は一致（書き込み 23.5 秒、読み出し 2.1 秒）。
  - ping は 598/600 応答（平均 17.7 ms、最大 214 ms）。
  - `nettest 4` も成功。
  - SDMMC のホストを SD（slot0）と C6（slot1）で共有しても、両方とも壊れない。
- PC から Tab5 への ping は遅く、ばらつく（1〜214 ms）。Tab5 からゲートウェイへの ping は 2 ms 前後。カーネルは C6 の受信をスケジューラの回り目でしか見ないので、何もしていないときはタイマの割り込みまで気づかないと考える（推測）。SDIO のカードの割り込みで起こすようにすれば縮むはず。

## 8. まだ
- WPA3 の AP で最初の 2 回の接続に失敗することがあった（202 → 205）原因。2026-11-01 には再現しなかった。
- 待ち受けと UDP を Wi-Fi 側でも使えるようにする。
- ポーリングをやめて SDIO のカードの割り込み（SDMMC の `sdio_int_mask`）でカーネルを起こす。
- 接続先を SD カードの設定ファイルから読む。
- C6 の電源を入れた・入っていた状態を、M5Stack の元のファームウェア（バックアップ）がどう扱うかは確かめていない。
