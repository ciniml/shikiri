# ネットワークスタックの調査: リンク層、スタックの選択、ルーティング・転送・フィルタ

調査日: 2026-10-31。設計の段階の調査で、コードはまだ書かない。実機には触れていない。
利用者の要望: 「ネットワークスタックの実装も並行して考えたい。最初は lwIP 程度でよいが、ルーティング、フォワーディング、フィルタリングができるようにしたい」。

出典の表記:
- `TCC/` = `~/repos/tab5_claude_client/`（doc/06 と同じ）。
- `HOSTED/` = `TCC/managed_components/espressif__esp_hosted/`（esp_hosted 2.12.9。`idf_component.yml:11`）。
- `HOSTED14/` = `TCC/c6_updater/managed_components/espressif__esp_hosted/`（esp_hosted 1.4.0。`idf_component.yml:11`）。
- `IDF/` = `~/esp-idf/6.0/components/`、`R1/` = `IDF/soc/esp32p4/register/hw_ver1/soc/`。
- `SMOL/` = crates.io の smoltcp 0.14.0 のソース。`EHOSTED/` = embassy-net-esp-hosted 0.3.0 のソース（どちらも 2026-10-31 に取得。リポジトリには入れない）。
- `SCH` = 公式回路図を pdftotext にかけたテキスト（doc/06 と同じ PDF）。
- （推測）は推測、（判断）は判断を表す。

---

## 0. 結論

- **スタック**: **smoltcp 0.14.0**（2026-08-07、0BSD）を、**U モードのネットワークサービス `net`** の中で使う。カーネルには入れない。lwIP は C の FFI と PIE のビルドの手間に対して、得るもの（IP_FORWARD とフック）が小さい（§2）。
- **smoltcp だけではルータにならない**。smoltcp の `Interface` は 1 つのデバイスに 1 つで、自分あての以外の IP パケットは捨てる（`SMOL/src/iface/interface/ipv4.rs:185-210`）。転送の機能はない。そこで、**リンクごとに smoltcp の `Interface` を 1 つ置き（ホストとしての機能: ARP、DHCP クライアント、ICMP、TCP/UDP）、その手前に自作の「転送面」を挟む**。転送面は、経路表、転送（TTL、チェックサム、ICMP エラー）、フィルタ、コネクション追跡、（後で）NAT を受け持つ（§3.3、判断）。
- **最初のリンク**: **USB Serial/JTAG（USB-C の CDC-ACM）の上の「ホストリンク」**。今のログと同じ COBS のフレームに、種類の違うフレームとして IP パケットを載せる。PC 側は `t5log` が TUN デバイスにつなぐ。カーネルの追加は USJ の受信だけで、IO エクスパンダ #2 にも触れない（§1.4）。
- **本命のリンク**: **ESP32-C6 の Wi-Fi（SDIO + esp_hosted-MCU）**。データの経路は 12 バイトのヘッダ＋**Ethernet II のフレーム**なので、smoltcp の `Medium::Ethernet` にそのまま渡せる。制御（接続、鍵）は protobuf の RPC で、U モードの `wifi` サービスが受け持つ。重いのは SDMMC の slot1（SD カードと同じコントローラ）と、C6 の電源（**IO エクスパンダ #2 の bit0。書く前に利用者の承認が要る**）（§1.1）。
- **カーネルに足すもの**: (1) 共有メモリ `shm`（PMP の NAPOT 1 本）、(2) 1 つの名前に複数のクライアントがつながる「サービス」のチャネル、(3) リンクの生フレームを共有メモリのリングで渡す `netdev` オブジェクト。今のチャネル（256 バイト × 4）は通知と小さい要求だけに使う（§3.2）。
- **段階**: N0（カーネルの IPC）→ N1（ループバックと socket API）→ N2a（ホストリンク）→ N2b（Wi-Fi）→ N3（2 リンク間の転送）→ N4（フィルタと状態の追跡）→ N5（NAT と DHCP サーバ）（§4）。
- **大きいリスク**: C6 のファームウェアの版（出荷時は 1.4.1、手元はおそらく 2.x に書き換え済み）、SDMMC を SD カードと共有すること、割り込みなしのポーリング（今は 10ms）での遅延、360MHz・1 コアでのコピーの回数（§5）。

---

## 1. Tab5 で使えるリンク層

| リンク | 速さ（推測） | カーネルの作業 | 危険 | 判断 |
|---|---|---|---|---|
| (a) Wi-Fi（C6、SDIO、esp_hosted） | 10〜30 Mbps | SDMMC slot1、SDIO の CMD52/53、esp_hosted の転送層、割り込み | **エクスパンダ #2** の書き込み、C6 の FW の版 | 本命。N2b |
| (b) USB デバイス（CDC-NCM/ECM） | 〜8 Mbps（FS） | DWC2 の OTG FS をデバイスで。USB-C の PHY を USJ から奪う | **USJ のログと JTAG（RTT）が使えなくなる** | 後回し |
| (c) Ethernet（EMAC/RMII） | – | – | – | **Tab5 には配線がない** |
| (d) USJ 上のホストリンク（SLIP 相当） | 2〜6 Mbps | USJ の受信（OUT エンドポイント）だけ | ほぼなし | **最初の実リンク。N2a** |
| (d') RTT 上のホストリンク | 低い | RTT の down/up チャネルを足す | LP SRAM の空きが少ない | 使わない |

### 1.1 (a) Wi-Fi: ESP32-C6 + SDIO + esp_hosted

**配線と電源**（doc/06 §1.7）
- SDIO: CLK=GPIO12、CMD=GPIO13、D0〜D3=GPIO11/10/9/8。SDMMC の **slot1**（GPIO マトリクス経由）、4 ビット、20MHz（40MHz では CMD5 がタイムアウトした）（`TCC/sdkconfig.defaults.esp32p4:49-60`）。
- C6 の CHIP_EN = GPIO15（High で動作）。esp_hosted 2.x は High→Low→High のパルスでリセットする（`TCC/M5_IDF6_PATCHES.md:78-116`）。
- C6 の電源 = **IO エクスパンダ #2（0x44）の bit0 WLAN_PWR_EN**。TCC は、電源を切って 300ms、入れて 1500ms 待ってから SDIO を始める（`TCC/main/wifi_setup.cpp:117-121`）。
- **約束**: #2 は利用者の承認なしに書かない（bit4 を 10 回トグルすると電源断。doc/06 §1.3、doc/17 §2）。
  - TCC は #2 をチップリセット（reg 0x01 に 0xFF）してから全レジスタを書き直している（`wifi_setup.cpp:68-82`）。**これは真似しない**。
  - 案（判断）: まず #2 の OUT_SET（0x05）と IO_DIR（0x03）を**読むだけ**にして、bit0 の状態をログに出す。電源が入っていなければ、利用者の承認を得てから、**bit0 だけを変える読み・変更・書き**を 1 回行う。bit4 の値と向きは変えない。
  - 電源を入れたままなら、C6 のリセットは GPIO15 だけでできる（エクスパンダに触れない）。
- SDMMC のコントローラは **SD カード（slot0、M7）と共有**する。分周器の選択（CLKSRC）、バス幅（CTYPE）、コマンドのカード番号はスロットごと（doc/research/m7-sdcard.md §2）。今の `sd.rs` を「ホスト＋スロット」に分け、カーネルの中で 1 度に 1 コマンドに直列化する（判断）。
- SDIO のカード割り込み（C6 の新しいパケットの通知）は、SDMMC の `sdio_int_mask` bit[17:16]（スロットごと）で受ける（`R1/sdmmc_struct.h:338-343`）。

**C6 のファームウェアの版**
- 出荷時は M5Stack の `ESP32C6-WiFi-SDIO-Interface-V1.4.1`（esp_hosted 1.4.x）（`TCC/slave_c6_fw/README.md:3-8`）。
- TCC は esp_hosted 2.12.x のホストを使い、C6 を 2.x のスレーブに書き換える（`TCC/CLAUDE.md:89-132`）。書き換えはカバーを開けずに SDIO の OTA でできる（`TCC/c6_updater/`）。
- TCC のログ（2026-06-09）では、2.x のホストで `Identified slave [esp32c6]`、`SDIO mode: slave: streaming`、`capabilities: 0xd`、`got IP 192.168.2.133` になっていた（`TCC/logs/isolate.log:99-155`）。**手元の Tab5 の C6 は、すでに 2.x に書き換えてある可能性が高い**（推測。利用者に確認する）。
- 1.4.0 と 2.12.9 は、ペイロードのヘッダ（12 バイト）と RPC のメッセージ名（`Rpc`、`RpcId`）が同じ形（`HOSTED14/common/esp_hosted_header.h:7-30`、`HOSTED14/common/proto/esp_hosted_rpc.proto:1577`、`HOSTED/common/proto/esp_hosted_rpc.proto:2153`）。.proto は 1830 行から 2468 行に増えている。どこが互換でないかは未確認。TCC の記録は「IDF 6.0 では 2.x のホストでないときれいに動かない」とだけ書いている（`TCC/slave_c6_fw/README.md:5-8`）。**最初は 2.x のスレーブだけを対象にする**（判断）。

**フレームの形（esp_hosted 2.12.9）**
- すべての転送は 12 バイトのヘッダ `esp_payload_header` から始まる: `if_type:4 | if_num:4`、`flags`（bit0 MORE_FRAGMENT）、`len`、`offset`、`checksum`、`seq_num`、`throttle_cmd:2`、`priv_pkt_type`（`HOSTED/common/esp_hosted_header.h:13-34`）。
- `if_type`: STA=1、AP=2、SERIAL=3（RPC）、HCI=4、PRIV=5（初期化イベント）（`HOSTED/common/esp_hosted_interface.h:14-23`）。
- **STA のデータは Ethernet II のフレーム**。スレーブは受け取ったデータをそのまま `esp_wifi_internal_tx(WIFI_IF_STA, …)` に渡し、Wi-Fi の受信コールバック（`esp_wifi_internal_reg_rxcb`）のフレームをそのまま送る（`HOSTED/slave/main/esp_hosted_coprocessor.c:708-728`、`HOSTED/slave/main/slave_wifi_std.c:2225`）。esp_wifi の内部 API は 802.3 のフレームを扱う（推測。ESP-IDF の lwIP のポートと同じ）。**ホストから見ると Ethernet の NIC と同じ**で、smoltcp の `Medium::Ethernet` に渡せる。
- 受信バッファは 1536 バイト、SDIO のブロックは 512 バイト（`HOSTED/host/drivers/transport/sdio/sdio_reg.h:39-41`）。12 + 1514 が入るので MTU は 1500。
- SDIO の手順（2.x、ストリーミング受信）: C6 の SLC ホストのレジスタ（`INT_RAW` 0x50、`PACKET_LEN` 0x60、`TOKEN_RDATA` 0x44 など）を CMD52/53 で読み、データは CMD53 のブロック転送で、アドレス `0x1F800 − 残り` から読み書きする（`sdio_reg.h:37,52-67`、`sdio_drv.c:729-734,1221-1275`）。新しいパケットは割り込みの bit23、送信の抑制は bit7/bit6（`sdio_reg.h:43-47`）。
- チェックサムは `H_SDIO_CHECKSUM` のときだけ（`sdio_drv.c:693,784,823`）。
- ESP_ETH_IF（=7）もある。有線の Ethernet 用で、Tab5 では使わない（推測）。
- 2.x には「network split」（C6 側の lwIP が一部のポートを横取りする）機能があるが、既定は無効（`HOSTED/slave/main/Kconfig.projbuild:1149-1153`）。出荷時や TCC のビルドで有効かは未確認。

**制御の経路（RPC）**
- RPC は SERIAL インタフェースの上の TLV（型 0x01 = 端点名 `RPCRsp`/`RPCEvt`、型 0x02 = データ）で、データは protobuf の `Rpc` メッセージ（`HOSTED/host/drivers/virtual_serial_if/serial_if.c:20-62`、`HOSTED/common/transport/esp_hosted_transport.h:21-22`）。
- 起動時に C6 から PRIV のイベント `ESP_PRIV_EVENT_INIT`（0x22）が来て、チップ ID と能力を知らせる（`esp_hosted_transport.h:28-34`、`isolate.log:114-129`）。
- 最低限の RPC（推測）: `GetMACAddress`（257）、`WifiInit`（278）、`WifiSetMode`、`WifiSetConfig`（284）、`WifiStart`、`WifiConnect`（282）、`GetCoprocessorFwVersion`（350）、イベント（接続、切断）（`HOSTED/common/proto/esp_hosted_rpc.proto:93-215`）。
- protobuf は no_std のクレートで書ける: `micropb` 0.7.0（2026-10-24）、`femtopb` 0.9.0（2026-08-23）（crates.io、2026-10-31 に照会）。必要なメッセージだけを手で符号化してもよい（判断）。
- **既存の Rust 実装は使えない**: `embassy-net-esp-hosted` 0.3.0 は **esp-hosted-fg** の `CtrlMsg`（`esp_hosted_config.proto`）を話す（`EHOSTED/src/lib.rs:13,75`、`EHOSTED/src/esp_hosted_config.proto:533`、CHANGELOG「Update protos to latest esp-hosted-fg」）。Tab5 の C6 は **esp-hosted-MCU** の `Rpc` なので、制御は合わない。転送は SPI 向け（0.3.0 で `Interface` trait が入った）。ペイロードのヘッダの扱いは参考になる。
- ライセンス: esp_hosted は Apache-2.0（`HOSTED/LICENSE`）。.proto を写して使うのは問題ない（推測）。

**作業量（推測）**: SDMMC の slot 化と SDIO（CMD5/CMD52/CMD53、CCCR、ブロック長、割り込み）で 600〜900 行、esp_hosted の転送（ヘッダ、ストリーム受信、抑制）で 400 行、`wifi` サービスの RPC と状態機械で 800〜1200 行。doc/06 §6 の ★5 のとおり重い。

### 1.2 (b) USB デバイスのネットワーク（CDC-NCM/ECM、RNDIS）
- P4 には DWC2 の USB OTG が 2 つある（`IDF/soc/esp32p4/include/soc/soc_caps.h:461`）。HS（0x5000_0000、専用の HS PHY）と FS（0x5004_0000）（`R1/reg_base.h:39-41`）。
- **USB-C** は FSLS PHY0（GPIO24/25）で、既定は USJ（`R1/io_mux_reg.h:167-176`、doc/06 §1.8）。`LP_SYS.usb_ctrl.sw_usb_phy_sel` で PHY0 を OTG FS（USB_WRAP）に切り替えられる（`IDF/hal/esp32p4/include/hal/usb_serial_jtag_ll.h:216-240`）。**切り替えると、USJ のログ（`make monitor`）も JTAG（RTT、`make rtt`）も使えなくなる**。ログをほかへ逃がす手段（UART0 の M5-Bus 13/14 番ピン、doc/06 §3.2）が要る。
- **USB-A** は HS の OTG だが、ホスト用のコネクタで、VBUS はエクスパンダ #2 の bit3（doc/06 §1.8）。デバイスとして使うには A-A ケーブルが要り、VBUS がぶつかる。使わない（判断）。
- 作業量（推測）: DWC2 のデバイスモード（EP0 の列挙、バルク IN/OUT、FIFO の割り当て）で 800〜1200 行。CDC-NCM のクラスは 300〜500 行。Linux と macOS は NCM を標準で扱う。Windows 10 以降も NCM のドライバを持つ（推測）。RNDIS は使わない（Microsoft が非推奨にしている。推測）。
- FS なので 12 Mbps が上限。USJ のホストリンク（§1.4）より速いが、デバッグの経路と引き換えになる。**後回し**（判断）。
- 利用者の判断（2026-11-01）: CDC-NCM は後回しにする。USB-C を NCM にすると USJ のコンソールと JTAG（RTT、インストール、kupdate）が使えなくなり、ログを UART0 へ逃がす準備が要るため（doc/31 の後の相談）。

### 1.3 (c) Ethernet（EMAC）
- P4 は EMAC を持つ（0x5009_8000。`R1/reg_base.h:57`、RMII の信号は `IDF/soc/esp32p4/include/soc/gpio_sig_map.h:208-337`）。
- **Tab5 には PHY も RJ45 もない**。回路図のテキストに RMII、MDIO、PHY の部品の記述はなく（`SCH` を grep）、ブロック図の接続先にも Ethernet はない（`BLK`）。M5-Bus に RMII を出す外付けの基板は想定しない（判断）。

### 1.4 (d) もっと簡単なもの: USJ の上のホストリンク
- USJ（USB-C の CDC-ACM）は、今はカーネルからの送信だけに使っている（`kernel/src/platform/esp32p4v1.rs:453-470`）。受信（PC → Tab5）は使っていない。制御コマンドは RTT の down チャネルから来る（doc/14 §3）。
- 案（判断）: 今の COBS のフレームの先頭に「種類」の 1 バイトを足し、ログと **IP パケット**を同じ線に混ぜる。PC 側は `t5log` が `/dev/ttyACM*` を読み、IP のフレームを Linux の TUN（`tun0`）に書く。逆向きも同じ。SLIP そのものではなく COBS にするのは、今のフレーム化と解読をそのまま使えるため。
  - TUN を作るには CAP_NET_ADMIN が要る（`ip tuntap add … user $USER` で一度作っておけば、以後は普通の権限で開ける）。
  - `Medium::Ip` のリンクになる。ARP は要らない。
- 速さ: USJ は FS のバルクで 64 バイトずつ。カーネルは FIFO をポーリングで読み書きする。数百 KB/s 程度（推測）。ログと帯域を分け合う。
- RTT（d'）は使わない。制御ブロックとバッファは LP SRAM（約 31KiB）にあり、ログのリング 16KiB、up 8KiB、down 2KiB でほぼ埋まっている（doc/12 §2、`kernel/src/log.rs:35-37`）。L2MEM に置くと probe-rs がコアを止めて読む（doc/12 §6）。
- RS485（GPIO20/21/34、`SCH`）や UART0 の上の SLIP/PPP もできるが、相手の機器が要る。

---

## 2. no_std の Rust で使えるスタック

### 2.1 smoltcp
- 版: **0.14.0（2026-08-07）**。0.13.0（2026-03-20）で SLAAC、TCP の改善、Edition 2024、MSRV 1.91。0.14.0 で TCP の輻輳制御の作り直し、チェックサムの高速化、TSO（`SMOL/CHANGELOG.md:11-60`）。ライセンスは **0BSD**（`SMOL/Cargo.toml:39`）。
- 機能（`SMOL/Cargo.toml` の `[features]`）: `proto-ipv4`、`proto-ipv6`（SLAAC）、`proto-ipv4-fragmentation`、`socket-tcp`/`udp`/`icmp`/`raw`/`dhcpv4`/`dns`/`mdns`、`medium-ethernet`/`medium-ip`/`medium-ieee802154`、`multicast`、`proto-ipsec`、`proto-rpl`（6LoWPAN の RPL）。`alloc` なしで、固定長のバッファで動く。表の大きさは `iface-max-addr-count-N`（既定 2）、`iface-max-route-count-N`（既定 2）などのフィーチャで決める（`SMOL/build.rs:9-13`）。
- **インタフェース**: `Interface::new(config, device, now)` は 1 つの `Device` に結びつく（`SMOL/src/iface/interface/mod.rs:207`）。複数のリンクには、`Interface` と `Device` を複数作る。
- **経路**: `Routes` は「宛先の範囲 → ゲートウェイ」の表で、`add_default_ipv4_route` と `update` がある（`SMOL/src/iface/route.rs:85-134`）。これは**自分が送るパケットの次ホップを決めるだけ**。
- **転送はしない**。受信したパケットが自分のアドレス・マルチキャスト・ブロードキャストでなければ捨てる（`SMOL/src/iface/interface/ipv4.rs:185-210`）。`set_any_ip(true)` は「経路のゲートウェイが自分のアドレスの範囲あてを**自分で受け取る**」ためのもので、転送ではない（`mod.rs:425-442`）。RPL は 6LoWPAN のメッシュ用で、IPv4 のルータの代わりにはならない（推測）。
- **ARP の表は外から見えない**（`InterfaceInner` の中、`mod.rs:1047-1078`）。転送面は自分で ARP の表を持つ必要がある。
- `Medium::Ip` では近隣の解決をしない（`has_neighbor` は常に真、`mod.rs:1055`）。ただし送り先への経路は要る（`route()`、`mod.rs:1035-1045`）。
- TCP のソケットは、ローカルアドレスがそのインタフェースから消えると閉じる（0.13.0、#1113）。**1 つの `SocketSet` を複数の `Interface` で poll しない**。リンクごとに `SocketSet` を分ける（判断）。
- `wire` モジュール（`EthernetFrame`、`ArpPacket`、`Ipv4Packet`、`TcpPacket`、`UdpPacket`、`Icmpv4Packet`、`checksum`）は公開されていて、転送面のパケットの解析と書き換えに使える（0.14.0 で `checksum` も公開。`CHANGELOG.md:13`）。
- `phy::Loopback` がある（`SMOL/src/phy/loopback.rs:10`）。

### 2.2 embassy-net
- 版 0.9.1（2026-04-16、MIT/Apache-2.0）。中身は smoltcp 0.13.0（`embassy-net-0.9.1/Cargo.toml:510-520`）。
- smoltcp を async の API で包み、`Stack` 1 つにリンク 1 つ。embassy の executor と `embassy-time` を前提にする。
- 本 OS では、サービスはイベントのループ（`wait`）で書くので、async の層は要らない。転送の機能も足されていない。**使わない**（判断）。ドライバの trait（`embassy-net-driver`）の形は参考になる。

### 2.3 lwIP（C、FFI）
- 最新は **2.2.1（2025-02-06）**（https://savannah.nongnu.org/news/?id=10724 ）。ESP-IDF 6.0 の同梱は 2.2.0 に Espressif の変更を足したもの（`IDF/lwip/lwip/src/include/lwip/init.h:53-57`）。ライセンスは BSD-3-Clause。
- 転送とフィルタの仕組み:
  - `IP_FORWARD`（netif 間の IPv4 転送。`opt.h:743`）。
  - `LWIP_HOOK_IP4_INPUT(pbuf, netif)`（受けたすべてのパケットを見て、捨てられる。`opt.h:3008`）。
  - `LWIP_HOOK_IP4_CANFORWARD(src, dest)`（転送の可否。アドレスしか見ない。`opt.h:3062`）。
  - `LWIP_HOOK_IP4_ROUTE_SRC(src, dest)`（送信元を使う経路の選択。`opt.h:3041`）。
  - NAPT（`IP_NAPT`）は **Espressif の版だけ**にある（`IDF/lwip/port/include/lwipopts.h:208-226`、`ip4.c:334,410`）。上流にはない。
  - コネクションの追跡はない。
- 本 OS で使うときの作業（推測）:
  - C のコードを U モードの静的 PIE に入れる。`-fPIE` で、再配置が `R_RISCV_RELATIVE` だけになることを確かめる（doc/07 §8-5 と同じ確かめ）。ESP-IDF の riscv32 の gcc がある。
  - `NO_SYS=1` の移植層（時刻、乱数、`sys_now`）と、`mem`/`memp` の設定。
  - Rust からは bindgen と unsafe で包む。
  - 転送とフィルタはフックの中に自分で書く（ポートを見るには IP ヘッダの先を自分で解析する）。
- 判断: **フックがあっても、フィルタと状態の追跡は自分で書くことになる**。それなら smoltcp の上に転送面を自作するのと手間は大差なく、C と FFI の分だけ損になる。lwIP は「TCP の挙動が smoltcp で困ったとき」の予備にする。

### 2.4 ほかの Rust のスタック（設計の参考）
- **Fuchsia netstack3**（Rust、std 前提、Fuchsia 専用）。フィルタは別のクレート `netstack3_filter` で、IP 版ごとに「フックに付けた routine、conntrack の表、NAT を使っているかの旗」を持つ。NAT の規則が一度も入っていなければ NAT の処理を飛ばす。conntrack は向き（元の向きか返事か）をパケットのメタデータに入れて、NAT の書き換えのあとも向きを取り違えないようにしている（https://docs.fuchsia.dev/rust/netstack3_filter/index.html 、https://docs.fuchsia.dev/rust/netstack3_filter/struct.State.html 、https://fuchsia.googlesource.com/integration/+/e071617daf97be8be3be7ad3f198ad69acde9bd4 ）。**フックの分け方（入力、ローカルへ、転送、ローカルから、出力）と conntrack の設計を借りる**（判断）。
- **Hubris の `net` タスク**（Oxide）。smoltcp の上に作った小さいスタックで、IP と UDP だけ（IPv6 中心）。ソケットは `app.toml` で静的に決め、送受のパケット数とバイト数の上限を持つ。クライアントは `send_packet`/`recv_packet` の IPC を使い、ソケットに動きがあると通知を受ける。VLAN ごとに smoltcp の実体を 1 つ置く。MAC の DMA バッファから、smoltcp で中を見てソケットのバッファへコピーする（https://github.com/oxidecomputer/hubris/tree/master/task/net 、`task/net/README.md`）。**「リンクごとに smoltcp を 1 つ」「ソケットの資源を静的に決める」「通知＋コピー」の形は、本 OS の案とほぼ同じ**。
- **Redox の smolnetd**: smoltcp を U モードのデーモンで動かし、`tcp:`、`udp:` などのスキームでアプリに見せる（https://gitlab.redox-os.org/redox-os/netstack 。中身は今回は読んでいない）。
- **Tock**: カーネルのカプセルとして IPv6/6LoWPAN と UDP を持つ（`capsules/extra/src/net`。推測、今回は読んでいない）。TCP とルータの機能はない。参考にしない。

---

## 3. この OS での構成

### 3.1 どこに置くか
- **スタックは U モードのサービス `net`**（判断）。理由:
  - カーネルにはアロケータがなく、syscall は割り込み禁止で短く動かす方針（doc/07 §4.3）。TCP の再送やタイマを抱えると、この方針に合わない。
  - smoltcp と転送面、フィルタのコードは大きい（数万行規模の依存）。カーネルの外に置けば、落ちてもカーネルは止まらず、サービスを起動し直せる（M5c のチャネルは「片方が閉じても、もう一度開けばつながる」。doc/16 §5.1）。
  - アプリの A/B 更新（M5b）で、カーネルと別に更新できる。
- **リンクのドライバはカーネル**（doc/07 §1-6 の Tock 型を続ける）。SDMMC は SD カードと共有で、DMA とキャッシュの操作は M モードでしかできない。カーネルはフレームを運ぶだけで、中身は見ない。
- **Wi-Fi の制御（RPC）は U モードのサービス `wifi`**。カーネルの `netdev:wlan0` は、SERIAL（RPC）のフレームを `wifi` に、STA のフレームを `net` に振り分ける。鍵（PSK）は `wifi` だけが持ち、ログに出さない。

```
 アプリ ──(svc:net の要求、共有メモリのソケットバッファ)──> [net サービス: 転送面 + smoltcp × リンク数]
                                                              │ netdev のリング（共有メモリ）
 [wifi サービス] ──(RPC)──┐                                    │
                          v                                    v
 [カーネル: netdev:wlan0（SDIO + esp_hosted） / netdev:host0（USJ） / 後で netdev:usb0]
```

### 3.2 フレームの渡し方と、カーネルに要る IPC
今あるもの: チャネルは 8 本、端ごとにメッセージ 4 つ、1 つ 256 バイト（doc/16 §5.1）。PMP は、プロセスに 1〜3、共有メモリの予定に 4（NAPOT）、フレームバッファに 5〜6、7〜15 は予約（doc/07 §3、doc/17 §6）。`shm_map` は作らずに `display` の `ctl(MAP)` で代えた（doc/17 §6）。

256 バイトのメッセージで 1500 バイトのフレームを運ぶと、1 フレームに 6 往復とコピーが 2 回かかる。帯域の点で無理がある（判断）。そこで次の 3 つを足す。

1. **共有メモリ `shm`**（ABI に追加）
   - `open("shm:NAME")` で、PSRAM のプールから 2 のべき乗の大きさの領域を取る。名前と大きさは Permissions TLV で与える。`ctl(MAP)` で、そのプロセスの PMP エントリ 4 に NAPOT の RW を足す（`display` の MAP と同じ作り）。
   - `net` は **1 つの大きい領域（例: 1 MiB の「ネットのプール」）**だけを写像する。カーネルのドライバはプールに直接 DMA する（カーネルは PMP を使わないので、どこでも書ける）。
   - クライアントのアプリには、プールの中の**自分の分の切れ端（例: 64 KiB）**だけを NAPOT で見せる。これなら PMP エントリはプロセスごとに 1 本で済む（判断）。
2. **リングと通知**
   - フレームは共有メモリの中の固定長のスロット（2 KiB × N）に置き、記述子のリング（送り、受け、空き）でやりとりする。リングの添字が正で、チャネルのメッセージは「何か入った」の通知（中身なし）にする。キューが一杯（`Busy`）でも、リングを見れば取りこぼしはない。
   - 今の `wait` に、`netdev` の `RX`（値 = 新しいフレームの数）と `TX_DONE` のイベントを足す。
   - DMA とキャッシュ: カーネルは DMA の前後で L1 D と L2 を書き戻す・無効にする（M6、M7 と同じ。ROM の `Cache_WriteBack_Addr` など）。U モードは何もしない。
3. **サービスのチャネル**（1 つの名前に複数のクライアント）
   - 今のチャネルは端が 2 つだけなので、`net` が 3 つ目のアプリの相手をできない（3 つ目の `open` は `Busy`。doc/16 §5.4）。
   - 案: `open("svc:net")` の最初の 1 つをサーバ（待ち受け）とし、以後の `open` ごとに新しいチャネルの組を作る。サーバは `wait` の `ACCEPT` で新しいハンドルを受け取る。M7c のファイルサービス（`chan:fs`）にも同じものが要る。
   - チャネルの数（8 本）は足りなくなるので、16〜32 本にする（.bss で 32〜64 KiB。内部 SRAM の空きと相談）。

### 3.3 net サービスの中（smoltcp + 自作の転送面）
各リンク `L` について:

```
 netdev の受信 → [転送面] ─ 自分あて ─> フィルタ(INPUT) → smoltcp Interface(L) → ソケット
                    │
                    └ 転送する → フィルタ(FORWARD) → 経路表 → TTL−1、チェックサム → ARP(出口) → netdev の送信(出口)
 smoltcp Interface(L) の送信 → フィルタ(OUTPUT) → netdev の送信(L)
```

- **転送面は smoltcp の `Device` の包み**として作る。`receive()` で 1 フレームずつ見て、自分あて（リンク L のアドレス、ブロードキャスト、マルチキャスト、自分の MAC あての ARP）なら smoltcp に渡す。そうでなければ転送の経路に回す。
- **経路表**（自作）: 接頭辞の最長一致。項目は「宛先/長さ、出口のリンク、ゲートウェイ（なしは直結）、メトリック」。直結の経路は各リンクのアドレスから自動で作り、DHCP で得た既定の経路を足す。数十項目なので線形探索でよい（判断）。smoltcp 側の `Routes` には、そのリンクの既定のゲートウェイだけを入れる。
- **ローカルのソケットの出口**: アプリが `connect` したとき、経路表で出口のリンクを決め、そのリンクの `SocketSet` にソケットを作る。`bind(0.0.0.0)` の待ち受けは、各リンクに 1 つずつ作る（判断）。
- **ループバック**: 127.0.0.0/8 は、`phy::Loopback` の `Interface`（`Medium::Ip`）を 1 つ置く。リンク間で自分のアドレスにつなぐ場合（Wi-Fi のアドレスに USB 側から）は、転送面が「自分の別のリンクのアドレスあて」を見つけて、その `Interface` へ回す。
- **ARP（転送用）**: リンクごとに自前の表（例: 64 項目、寿命 60 秒）。通るフレームの ARP を覗いて覚え、知らなければ自分で要求を出し、フレームを少しの間（1 宛先あたり 1〜2 個）待たせる。
- **転送の処理**: TTL を 1 減らし、IP のチェックサムを差分で直す（RFC 1624）。TTL が尽きたら ICMP Time Exceeded、経路がなければ ICMP Destination Unreachable を返す（量は制限する）。MTU は全リンク 1500 にそろえ、断片化はしない（DF なら ICMP Fragmentation Needed、それ以外も捨てる。判断）。
- **フィルタ**（pf/nftables を小さくしたもの、判断）
  - フック: PREROUTING（受信の直後）、INPUT、FORWARD、OUTPUT、POSTROUTING。netstack3 と同じ分け方。
  - 規則: `動作（pass/drop/reject） 向き 入口/出口のリンク プロトコル 送信元/宛先の接頭辞 ポートの範囲 TCP の旗 状態（new/established/related/invalid） [keep state] [log]`。上から順に見て最初に一致したもので決める。既定の動作はフックごと。
  - 規則は固定長の配列（例: 128 個）で、`net` の起動時に設定ファイル（M7c のファイルサービス経由）か、制御用のチャネル `svc:netctl` から入れる。差し替えは丸ごと（途中の状態を見せない）。
  - **コネクションの追跡**: 5 組（プロトコル、送信元/宛先のアドレスとポート）をキーにしたハッシュ表、固定の大きさ（例: 1024 項目 × 約 64 バイト = 64 KiB）。TCP は SYN_SENT/ESTABLISHED/FIN_WAIT/CLOSED 程度の簡略な状態と、状態ごとの寿命。UDP と ICMP（echo の id）は寿命だけ。ICMP のエラーは中に入っている元のヘッダで既存の接続にひもづけ、`related` にする。
  - **NAT（N5）**: 出口のリンクでの送信元の書き換え（masquerade）。conntrack の項目に「書き換えた後の 5 組」を持たせ、チェックサムは差分で直す。ポートは範囲から割り当てる。
  - 記録: 捨てたパケットは、規則の番号と 5 組を構造化ログのイベント（例: `net.drop`）で出す。量を制限する。カウンタは `ps` と同じく制御コマンドで読む。テキストのログは出さない（構造化ログの方針。doc/12）。
- **DHCP**: クライアントは smoltcp の `socket-dhcpv4` をリンクごとに使う。サーバ（下流のリンクに PC をつなぐとき）は smoltcp にないので自作する（300〜500 行、推測）。DNS は smoltcp の `socket-dns`。

### 3.4 アプリから見た socket の API
- アプリは `svc:net` を開き、要求と応答をメッセージ（256 バイト以下）でやりとりする: `SOCKET(tcp|udp)`、`BIND`、`CONNECT`、`LISTEN`、`ACCEPT`、`SEND`、`RECV`、`CLOSE`、`SETOPT`、`RESOLVE`（DNS）。
- データは、アプリの共有メモリの切れ端の中の、ソケットごとの送受のリングに置く。`SEND`/`RECV` のメッセージは「どこまで書いた/読んだ」だけを運ぶ。`net` は smoltcp のソケットのバッファとこのリングの間で 1 回コピーする。
  - smoltcp のソケットのバッファそのものを共有メモリに置けばコピーはなくなる。しかし、アプリが書き換えている最中の領域を smoltcp が読むことになり、TCP の再送の中身が変わりうる。最初はコピーにする（判断）。
- 共有メモリができる前（N1 の最初）は、小さいデータ（約 230 バイト以下）をメッセージに直接入れる。
- `t5-rt` に `TcpStream`/`TcpListener`/`UdpSocket` の形の薄い包みを作る（`std::net` に似せる。判断）。
- 権限: `svc:net` を Permissions に持つアプリだけが使える。待ち受けのポートの制限（1024 未満など）や、どのアプリがどのリンクを使えるかは、`net` が規則で決める（判断）。

### 3.5 smoltcp で済むもの、書くもの（行数は推測）

| 部分 | smoltcp で済む | 自作（行数） |
|---|---|---|
| Ethernet、ARP（自分のアドレス）、IPv4、ICMP echo、TCP、UDP、DHCP クライアント、DNS | ○ | – |
| パケットの解析と組み立て | ○（`wire`） | – |
| ループバック | ○（`phy::Loopback`） | 数十 |
| 複数リンクとソケットの振り分け | △（リンクごとに `Interface`） | 300 |
| 経路表（リンクをまたぐ） | × | 200 |
| 転送（TTL、ICMP エラー、ARP の表） | × | 500 |
| フィルタと conntrack | × | 1000〜1500 |
| NAT | × | 500 |
| DHCP サーバ | × | 300〜500 |
| socket の IPC の API（サービス側とランタイム側） | × | 800〜1200 |
| カーネル: shm、svc、netdev | × | 600〜900 |
| カーネル: USJ のホストリンク／t5log の TUN | × | 200 ／ 300 |
| カーネル: SDIO と esp_hosted、`wifi` サービス | × | 1800〜2500 |

---

## 4. 段階の計画

| 段階 | 内容 | 完了の条件 | 実機の操作 |
|---|---|---|---|
| **N0** | カーネルの IPC: `shm`（PMP エントリ 4 に NAPOT）、`svc:` の待ち受けと `ACCEPT`、チャネルの数を増やす | 自己試験: 1 つのサーバに 3 つのクライアントがつながって往復する。2 つのプロセスが同じ `shm` を読み書きできる。写像していない 3 つ目のプロセスが触ると store access fault。QEMU・t5emu・実機で同じ | アプリのインストールだけ |
| **N1** | `net` サービス（smoltcp、ループバックだけ）と socket の API、`t5-rt` の包み。試験アプリ `nettest` | 自己試験: 127.0.0.1 で UDP の往復 100 個と、TCP で 1 MiB を送って SHA-256 が一致。かかった時間をイベントで出す。`net` を kill して起動し直すと、クライアントは `PEER_CLOSED` を受けて、つなぎ直せる | アプリのインストールだけ |
| **N2a** | USJ のホストリンク（カーネル: USJ の受信と、フレームの種類の多重化。`t5log`: TUN） | PC から `ping 10.55.0.2` が通る。PC の `nc` から Tab5 の TCP の echo に 1 MiB を送って一致する。ログは同じ線で欠けずに読める | なし（USB の CDC だけ） |
| **N2b** | Wi-Fi。(1) #2 は読むだけ、(2) **承認のあとで** bit0 だけ立てる、(3) SDMMC slot1 と CMD5、(4) esp_hosted の INIT イベント、(5) `wifi` サービスの RPC、(6) STA のフレームを `netdev:wlan0` へ | `GetCoprocessorFwVersion` で C6 の版が分かる。AP に接続し、DHCP で得たアドレス、ゲートウェイへの ping、DNS の解決、外への TCP（例: HTTP の HEAD）が通る。SD カード（M7）との並行の読み書きで両方とも壊れない | **エクスパンダ #2 の書き込み（要承認）**。C6 の FW を書き換えるなら別に承認 |
| **N3** | 2 リンクの間の転送（host0 ↔ wlan0）、経路表、ICMP のエラー | PC（10.55.0.1）から、PC に静的な経路を入れて、Wi-Fi 側の機器に ping が届く（相手側にも戻りの経路が要る。なければ N5 を待つ）。TTL=1 で Time Exceeded、経路のない宛先で Unreachable。t5emu では 2 本の仮想リンクの間で同じことを確かめる | N2b と同じ |
| **N4** | フィルタと conntrack、規則の読み込み、`net.drop` のイベント | t5emu の台本: wlan0 から来る新しい接続は捨て、こちらから始めた TCP の返事と ICMP のエラーは通す。規則の差し替えの途中でパケットを取りこぼさない。実機: 外からの ping に答えず、こちらからの通信はできる | なし |
| **N5** | NAT（masquerade）、下流の DHCP サーバ | PC を USB 側につなぎ、DHCP でアドレスを得て、Tab5 を通して外の Web が見える | なし |
| 後で | USB の CDC-NCM（ログの逃がし先とセット）、IPv6、割り込みでの受信、Core1 で `net` を動かす | – | – |

### 4.1 t5emu に要るもの
- N0/N1: 新しい周辺はない。カーネルの IPC と PMP の試験だけ（今の t5emu の範囲）。
- N2a: USJ の受信（ホストからの OUT のデータ）を足す。t5emu の中に**相手役のホスト**を置く。smoltcp をホスト側で動かして ping と TCP の echo に答える小さいもの（判断）。Linux の TUN につなぐ選択肢は任意（root が要るので CI では使わない）。
- N2b: SDMMC の slot1 と、**偽の ESP32-C6**: CMD5/CMD52/CMD53 への応答、CCCR、SLC ホストのレジスタ（`INT_RAW`、`PACKET_LEN`、`TOKEN_RDATA`）、INIT イベント、最低限の RPC（版、MAC、init、config、connect と接続のイベント）。その先に「AP と DHCP サーバと ping に答える相手」を置く。SD のモデル（M7）とコントローラを共有する形にする。
- N3/N4: 仮想リンクを 2 本にし、台本（決めたパケットの列を流し、出てきたパケットを確かめる）で転送とフィルタを試す。pcap で書き出せると調べやすい（判断）。

---

## 5. 未確認事項とリスク

**メモリ**
- `net` の見積もり（推測）: コードとデータで 200〜400 KiB、ネットのプール 1 MiB、TCP のソケット 16 個 × 送受 16 KiB = 512 KiB、conntrack 64 KiB。アプリのスロット（7 MiB、doc/17 §3）に収まる。PSRAM で問題ない。
- カーネル: チャネルを増やす分（数十 KiB）は内部 SRAM。`netdev` の記述子とバウンスバッファも内部 SRAM に置くか（IDMAC のアドレスの制約。m7-sdcard.md §8）。

**性能（360MHz、1 コア）**
- 1 フレームの経路でコピーが 2 回（ドライバ → プール は DMA、smoltcp ↔ アプリのリングで 1 回、場合により SDIO のバウンスで 1 回）。チェックサムはソフトウェア。PSRAM はキャッシュ経由。10〜30 Mbps なら 1 コアで足りる（推測）。実測は N1 のループバックの 1 MiB で取る。
- **割り込み**: 今のカーネルは 10ms ごとの見回り（`kernel/src/sched.rs:75` の `POLL_MS`）。このままだと往復の遅延が 10〜20ms 増え、TCP の速さが落ちる。SDIO のカード割り込み（SDMMC の IRQ）と USJ の受信の割り込みを CLIC に足す（M6 の DMA 完了の割り込みと同じ作り）。
- syscall とコンテキストスイッチの回数: フレームごとではなく、リングの通知をまとめる。
- `net` を Core1 に置く案は、Core1 を起動してからの話（doc/07 §1-7）。

**C6 のファームウェア**
- 手元の C6 が 1.4.1（出荷時）か 2.x（TCC で書き換え）かを、利用者に確認する。2.x の何版かも。
- 1.4.x と 2.x の非互換の中身（RPC の番号、INIT イベントの TLV、ストリーム受信の有無）を、両方のソースの差分で確かめる。
- C6 を書き換える場合の手順（`TCC/c6_updater/` は IDF 5.4 のアプリで、P4 のカーネルを一時的に上書きする）は、本 OS の A/B と回復の手順に合わせて別に決める。
- network split が有効なスレーブだと、一部のポートのパケットが P4 に来ない。

**ハードウェアと安全**
- エクスパンダ #2 の bit0 の初期状態（電源投入の直後、ブートローダの後）。読むだけで分かる。
- SDMMC を slot0（SD）と slot1（C6）で共有するときの、スロットの切り替え（クロックの再設定、`card_number`）の手順。IDF は 2 スロットを同時に扱えるので可能なはず（推測）。
- C6 の SDIO を 40MHz にできるか（TCC では CMD5 がタイムアウトした）。
- USJ の受信の FIFO の扱いと、ログの送信との帯域の分け方。

**設計**
- PMP エントリの余り: 共有メモリは 1 プロセスに 1 本（エントリ 4）の前提。`display` と `shm` を両方使うアプリでも足りる（5〜6 が表示、4 が shm）。
- `svc:` の待ち受けの API は、ファイルサービス（M7c）と合わせて決める。
- フィルタの規則の書き方（テキストの設定ファイルか、ホストツールで固めたバイナリか）。
- Wi-Fi の PSK の置き場所（userdata か SD か）と、ログに出さないことの確かめ（`t5log` のスキーマの検査で、鍵のフィールドを禁止する）。
- IPv6 をいつ入れるか（smoltcp は対応済み。転送面とフィルタの分が増える）。

---

## 6. 参考

**ローカル**
- `TCC/main/wifi_setup.cpp:60-121`、`TCC/sdkconfig.defaults.esp32p4:26-60`、`TCC/M5_IDF6_PATCHES.md:78-130`、`TCC/CLAUDE.md:89-132`、`TCC/slave_c6_fw/README.md`、`TCC/logs/isolate.log:97-155`
- `HOSTED/common/esp_hosted_header.h`、`HOSTED/common/esp_hosted_interface.h`、`HOSTED/common/transport/esp_hosted_transport.h`、`HOSTED/common/proto/esp_hosted_rpc.proto`、`HOSTED/host/drivers/transport/sdio/{sdio_reg.h,sdio_drv.c}`、`HOSTED/host/drivers/virtual_serial_if/serial_if.c`、`HOSTED/slave/main/{esp_hosted_coprocessor.c,slave_wifi_std.c,Kconfig.projbuild}`
- `HOSTED14/common/esp_hosted_header.h`、`HOSTED14/common/proto/esp_hosted_rpc.proto`
- `IDF/hal/esp32p4/include/hal/usb_serial_jtag_ll.h:200-240`、`R1/io_mux_reg.h:167-176`、`R1/reg_base.h:39-58`、`R1/sdmmc_struct.h:338-343,570-576`、`IDF/lwip/lwip/src/include/lwip/{init.h,opt.h}`、`IDF/lwip/port/include/lwipopts.h:208-226`
- 本リポジトリ: doc/06 §1.3・§1.7・§1.8・§6、doc/07 §3・§4、doc/12 §2・§6、doc/14 §3、doc/16 §5、doc/17 §2・§3・§6、doc/research/m7-sdcard.md、`kernel/src/log.rs:35-37`、`kernel/src/sched.rs:75`、`kernel/src/platform/esp32p4v1.rs:453-470`

**Web（2026-10-31 に参照）**
- smoltcp: https://crates.io/crates/smoltcp （0.14.0）、https://github.com/smoltcp-rs/smoltcp
- embassy-net: https://crates.io/crates/embassy-net （0.9.1）、embassy-net-esp-hosted: https://crates.io/crates/embassy-net-esp-hosted （0.3.0）
- micropb: https://crates.io/crates/micropb （0.7.0）、femtopb: https://crates.io/crates/femtopb （0.9.0）
- lwIP 2.2.1: https://savannah.nongnu.org/news/?id=10724
- esp-hosted-mcu: https://github.com/espressif/esp-hosted-mcu
- Fuchsia netstack3_filter: https://docs.fuchsia.dev/rust/netstack3_filter/index.html
- Hubris net: https://github.com/oxidecomputer/hubris/tree/master/task/net
- Redox netstack: https://gitlab.redox-os.org/redox-os/netstack
