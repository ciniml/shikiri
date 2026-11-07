# 58. ESP32-P4 EVB と簡易ルーターの方針（検討）

利用者の指示（2026-11-06）:「もともとのOS開発の目的としては、Linuxを使わない比較的まともなネットワークスタックと保護機構を持った簡易ルーター向けOSの開発がありました。現在はTab5をターゲットとしていますが、ESP32-P4 EVBもターゲットとし、Ethernetや無線LANのあいだでのフィルタリングやルーティングを試したいです。方針を検討してください。」

## 1. 今あるもの（ルーターとして）
`net`（U モードのサービス、smoltcp の上に自作の転送面、doc/research/net-stack.md §3.3）は、ルーターの部品をすでに持つ。

| 部品 | 状態 | 文書 |
|---|---|---|
| リンクの間の転送（経路表、ARP、TTL、ICMP のエラー） | 実機 | doc/23 |
| フィルタ（in/forward/out、接続の追跡、`netctl`、SD の `NET.RUL`） | 実機 | doc/24 |
| NAT（masquerade）、DHCP サーバ、DNS の中継 | 実機（下流は `net` の中の試験用 LAN `test0`） | doc/25、doc/29 |
| IPv6（SLAAC、転送、NDP proxy、RDNSS、一時アドレス） | 実機 | doc/26、doc/27、doc/46 |
| 保護（PMP、署名した許可、サービスの中の grant、鍵 2 つ） | 実機 | doc/52 |
| 上限、常駐の立て直し、許可を手放す | 実機 | doc/56 |
| CPU の取り分（`net` に 40%） | 実装中 | doc/57 |

**足りないのは物理のリンク**。今のリンクは次の 3 つだけで、Tab5 には Ethernet がない。
- `host`: USB の上の IP、PC だけ。
- `wlan0`: Wi-Fi の STA。
- `test0`: `net` の中の仮の LAN。

## 2. ESP32-P4-Function-EV-Board について分かったこと
調べた日: 2026-11-06。出典は Espressif のボードのガイド（v1.5.2、v1.4、P4X）、回路図 v1.52、TRM（v0.7）、チップの errata、esp-eth-drivers、ESP-IDF の Kconfig、esp-hosted-mcu。

- **版とチップ**:
  - ボードの版は、チップの版とは別に見分ける必要がある。ボードの版とチップの版の対応は、一次資料にない。
  - PCN20250501 で、このボードのチップは v1.0 から v1.3 に変わった（出荷 2025-07 頃から。「ソフトウェアに影響なし」）。
  - 後継の **ESP32-P4X-Function-EV-Board** はチップ **v3.1 以降**。ESP-IDF は「v3.x は 0.x/1.x と互換でない」としていて、v1.x には `CONFIG_ESP32P4_SELECTS_REV_LESS_V3` が要る。
  - 見分け方: eFuse の `EFUSE_RD_MAC_SPI_SYS_2_REG`、またはチップの刻印（v1.0「X C XX」、v1.3「X E XX」、v3.1「X G XX」）。
  - ボードは EOL の扱い。
  - <https://docs.espressif.com/projects/esp-dev-kits/en/latest/esp32p4/esp32-p4-function-ev-board/user_guide.html>、<https://documentation.espressif.com/en/PCN20250501_Upgrade_of_ESP32-P4_Series_Products_to_Chip_Revision_v1.3.html>、<https://docs.espressif.com/projects/esp-chip-errata/en/latest/esp32p4/01-chip-identification/index.html>
- **Ethernet**（回路図 v1.52 の 5 ページ）:
  - PHY: **IP101GRI**、RMII、10/100。
  - ピン:

    | 信号 | GPIO |
    |---|---|
    | MDC | 31 |
    | MDIO | 52 |
    | PHY_RSTN | 51（L で 10 ms 以上） |
    | TX_EN | 49 |
    | TXD0、TXD1 | 34、35（どちらもストラップのピン） |
    | CRS_DV | 28 |
    | RXD0、RXD1 | 29、30 |
    | REF_CLK | 50（入力） |

  - REF_CLK の 50 MHz は PHY が出し、P4 が受ける（`EMAC_CLK_EXT_IN`）。
  - PHY のアドレスは、IDF の例の既定が 1（確かめていない）。
- **EMAC**:
  - TRM 第 55 章。EMAC_CORE、MTL、DMA の 3 つでできている。
  - 送受信の記述子は、リングまたは鎖で並べる。IEEE 1588 の時刻と、宛先のフィルタも持つ。
  - Synopsys DesignWare GMAC 3.x の形に見えるが、TRM はそう書いていない（推測）。
  - errata に EMAC の項目はない。APM-560（許されない AHB のアクセスの後、PSRAM・フラッシュのアクセスが止まりうる）は v1.x に当てはまる。
- **Wi-Fi/BLE**:
  - ESP32-C6-MINI-1 が SDIO の 4 ビットでつながる。ピンは CLK 18、CMD 19、D0〜D3 が 14〜17。
  - C6_EN（C6 の CHIP_PU）は GPIO54、C6_WAKEUP は GPIO6。
  - Tab5 は CLK 12、CMD 13、D 11/10/9/8、EN 15 で、電源は IO エキスパンダ。
  - C6 に工場で入っているファームウェアと版は、一次資料で確かめられなかった。書き換えは、ボードの C6 用の書き込みコネクタからか、esp-hosted の OTA で P4 から。
- **そのほか**:
  - USB Serial/JTAG は v1.5.x にだけ、専用の Type-C がある。v1.4 は CP2102N の UART。
  - フラッシュは 16 MB（80 MHz まで）。PSRAM の量はガイドにない（チップは 32 MB まで）。
  - microSD は GPIO39〜44、電源は GPIO45。
  - IO エキスパンダも PMIC もない（Tab5 との大きな違い）。ユーザ用の LED はない。

## 3. 方針
### 3.1 プラットフォームを「チップ」と「ボード」に分ける
- 今の `platform/esp32p4v1` は、チップ（クロック、PSRAM、SDIO、SD、乱数、PPA）と Tab5（`tab5.rs`: エキスパンダ、PMIC、画面、タッチ、キーボード、C6 の電源）が混ざっている。
- **チップ** `esp32p4v1` の上に、**ボード** `tab5` と `p4evb` を置く。
  - ボードが持つもの:
    - SDIO のピン
    - C6 の電源と EN
    - SD の電源
    - 画面とタッチ（`p4evb` はなし）
    - Ethernet の PHY（`tab5` はなし）
  - カーネルは 1 つのソースから、ボードごとに像を作る（`platform-esp32p4v1` に `board-tab5` / `board-p4evb` の feature）。
  - 起動時にボードを見分ける方法はないので、ビルド時に決める（判断）。
- 自己試験とアプリは同じもの。画面のないボードでは、画面の試験を飛ばす（今の `selftest.skipped` と同じ）。
- **チップ v3.x（P4X）は別の段**にする。v1.0 と v1.3 は同じコードで動く見込み（PCN の「影響なし」、確かめる）。

### 3.2 Ethernet のドライバは、まずカーネルに置く（判断）
- `netdev:eth0` として、Wi-Fi の `netdev:wlan0` と同じ ABI にする。`read`/`write`、フレームの輪 `MAP_RING`/`KICK`（doc/35）。`net` は、リンクの種類を 1 つ足すだけで済む。
- EMAC の DMA の記述子とバッファは、カーネルの内蔵 RAM か PSRAM に置く。
  - 受信は、記述子の輪から `net` のフレームの輪へ写す（まずはコピー 1 回）。
  - 送信は、`net` の輪からそのまま DMA に渡す形も後で考える。
- PHY（IP101）は MDIO で扱う。リンクの上下と速度は `netdev` のイベントにし、`net` は上がったら DHCP をやり直す（Wi-Fi の `LINK_RENEW` と同じ）。
- **ユーザ空間のドライバ**（QNX の io-pkt のドライバと同じ形）は次の段で検討する。
  - Ethernet は外からのデータを直接受けるので、本当は外に出したい（doc/56 の「カーネルが大きい」）。
  - そのためには、カーネルに次の 3 つが要り、どれも新しい仕組みになる:
    - MMIO の窓をプロセスに貸す。
    - 割り込みをイベントにして渡す。
    - DMA が書ける範囲を APM で縛る（P4 の APM、errata APM-560 に注意）。
  - Ethernet は小さく独立しているので、移す最初の候補にする。

### 3.3 ルーターとしての設定
- **リンクの役割**を設定（`cfg`）に持つ。例:
  - `net.wan = wlan0`、`net.lan = eth0`
  - `net.lan.addr = 192.168.60.1/24`
  - `net.lan.dhcp = 100-199`
- 今は `test0` に決め打ちの次のものを、役割から作る:
  - DHCP サーバ
  - NAT（WAN へ出るものを masquerade）
  - DNS の中継
  - IPv6 の NDP proxy
- **既定のフィルタ**（ゾーンの考え方。リンクの名前ではなく役割で書く）:

  | 方向 | 既定 |
  |---|---|
  | WAN → 自分 | 落とす（返りの通信を除く） |
  | WAN → LAN | 落とす（返りの通信を除く） |
  | LAN → WAN、LAN → 自分 | 通す |
  | 管理（`netctl`、シェル） | USB の `host` だけ |

  - `NET.RUL` の書き方に `wan`/`lan` を足し、規則の中で役割をリンクに置き換える。
- **管理の口**は当面 USB（`host`）のシリアルとシェルだけにする。ネットワークからの管理（Web や SSH）は入れない（判断）。

### 3.4 段取り
| 段 | 内容 | 確かめ方 |
|---|---|---|
| R0 | ボードの確認: 版、チップの版（eFuse を読むだけ）、PSRAM の量、C6 のファームウェアの版（esp_hosted の読みだけ） | 読み取りだけ |
| R1 | プラットフォームをチップとボードに分ける。Tab5 は前のとおり（同じ試験）。t5emu にボード `p4evb`。EVB で起動、自己試験、SD、Wi-Fi の STA | Tab5 の全試験、EVB の自己試験と Wi-Fi |
| R2 | EMAC と IP101 のドライバ、`netdev:eth0`、リンクの上下。t5emu に EMAC（記述子の DMA と MDIO）を足し、ホストの TAP か内部の相手につなぐ | t5emu で DHCP と ping、実機で PC と直結 |
| R3 | ルーターの設定（役割、§3.3）、`test0` の決め打ちを外す。**場面 B**: WAN = Wi-Fi の STA、LAN = Ethernet（PC を有線でつなぐ） | 有線の PC から外の Web、フィルタの既定、IPv6 |
| R4 | C6 の softAP（esp_hosted の AP の設定、子機の出入りのイベント）。**場面 A**: WAN = Ethernet、LAN = Wi-Fi の AP | スマホを AP につなぎ外の Web |
| R5 | 性能と頑丈さ: 転送の Mbps と `net` の CPU、接続の追跡の上限、SYN・ICMP の洪水、アプリの負荷の下での転送（doc/57）、`net` の立て直しの後のつながり（doc/56） | iperf3 を通す、負荷の試験 |
| R6 | 守りの強化: DMA の範囲を APM で縛る、Ethernet のドライバをユーザ空間へ（§3.2） | 自己試験に DMA の範囲の外への書き込み |

- 場面 B を先にする（判断）:
  - 今の Wi-Fi の STA と、`test0` で試した DHCP・NAT がそのまま使える。
  - 新しいのは Ethernet だけになる。
  - 場面 A は、C6 の AP という別の未知を足す。
- Tab5 は引き続き主のターゲット（画面、Matter）。EVB はルーターの試験台。

## 4. 利用者に決めてもらうこと
1. **ボードの版とチップの版**:
   - 手元の EVB は v1.5.x か（USB Serial/JTAG の Type-C があるか）。
   - チップは v1.0 / v1.3 か、v3.x（P4X）か。
   - v3.x なら、R1 の前に「チップ v3 への対応」の段が要る。
2. **EVB の C6 のファームウェア**:
   - 今の `wifi` は esp_hosted 2.12.7 に合わせてある。EVB の C6 が別の版なら、次の 2 つのどちらかになる。
     - EVB の C6 に 2.12.7 を書く（Tab5 の C6 は書き換えない、という今の決まりは Tab5 のもの）。
     - ドライバを合わせる。
   - EVB の C6 を書き換えてよいか。
3. **場面の順番**: B（Wi-Fi の STA → 有線の LAN）を先にしてよいか。
4. **Ethernet のドライバの置き場所**: まずカーネル、ユーザ空間は R6、でよいか。
5. **EVB の実機の扱い**: Tab5 と同じ決まりでよいか。
   - 書き込みは確認の後で、プローブを指定する。
   - ストラップのピン（GPIO34/35 は TXD0/TXD1）には、起動の間は触らない。

### 利用者の判断（2026-11-06）
「EVBは1.5.2 (v1.0/1.3) と1.6.0 (v3.x) の両方があります。両方ためせます。C6のファームウェア更新OKです。のこりは提案通りでよいです。」
- **ボード**: v1.5.2（チップ v1.0/v1.3）と v1.6.0（チップ v3.x）の 2 枚。
  - v1.5.2 で R1〜R3 を進める。
  - v1.6.0 は「チップ v3 への対応」の段（R1b）で扱う。v3.x の違い（メモリの配置、キャッシュ、ROM、ESP-IDF の bootloader の設定）は、その段の初めに調べる。
- **C6**: EVB の C6 は更新してよい。Tab5 の C6 は今までどおり書き換えない。
- 3〜5 は提案どおり:
  - 場面 B を先にする。
  - Ethernet のドライバは、まずカーネルに置く。
  - 実機の扱いは Tab5 と同じ。

## 5. 進み具合
- **R1（実機なしの部分、2026-11-06）**:
  - `kernel/src/platform/esp32p4v1/` の Tab5 の部分（`tab5.rs`）に、ボードの定数を足した。
    - `NAME`
    - `I2C`、`KBD`
    - `SDIO` のピン
    - `sd_power_on`
  - EVB の `p4evb.rs` を足した。`esp32p4v1.rs` は feature `board-p4evb` で `board` を選ぶ。
  - SDIO は、ボードのピンを使う。GPIO32 以上（EVB の C6_EN は GPIO54）の出力と入力のレジスタも足した（gpio_reg.h、hw_ver1 と hw_ver3 で同じ）。
  - EVB では I2C とキーボードに触らない。Tab5 の I2C の GPIO31 と、キーボードの INT の GPIO50 は、EVB では Ethernet の MDC と REF_CLK のため。
  - EVB の SD の電源（GPIO45 の SD_PWRn、IO の電源に LDO VO4 が関わるように見える）は、極性と配線を実機で確かめるまで何も駆動しない（R0）。
  - `make image-p4evb` で `build/t5-kernel-p4evb.{elf,bin,t5k}` ができる。
  - `make test-p4emu` に、EVB のカーネルが t5emu で自己試験を通ることを足した（t5emu は Tab5 の I2C の機器を模すが、EVB のカーネルは触らない）。
- **R0（2026-11-06、読み取りだけ）**:
  - **v1.6.0**（USB のシリアル E8:F6:0A:E7:55:79）:
    - チップ v3.2（ROM eco7）、PSRAM 32 MB（AP の第 4 世代、250 MHz）、フラッシュ 16 MB。セキュアブートとフラッシュの暗号化はどちらも無効。
    - 出荷時のデモ（esp_brookesia_demo 1.2.0、ESP-IDF v5.5.2）は「LDO Channel 4 を 3.3V」と出す。SD の IO の電源に VO4 を使う。
    - 画面がないので GT911 の初期化で止まり、再起動をくり返す。
    - espflash 4.4 の `board-info` は `--no-stub` でないとつながらなかった。
  - **v1.5.2**（30:ED:A0:E2:78:76）:
    - チップ v1.3、PSRAM 32 MB（AP の第 4 世代、200 MHz、Tab5 と同じ）、フラッシュ 16 MB。
    - 出荷時のデモ（1.0.1、ESP-IDF v5.5-dev）は同じく GT911 で止まる。
    - フラッシュ全体を `build/evb152-flash-backup.bin`（sha256 d1efef50…）に読み出した。
  - C6 のファームウェアの版は、出荷時のデモから読めなかった。
- **R1 の実機（v1.5.2、2026-11-06）**: 利用者の確認の後、Tab5 と同じ配置で書いた（ブートローダ、パーティション表、otadata の消去、`t5-kernel-p4evb.bin`、ストア）。
  - **PSRAM**:
    - 最初は `boot.no_psram`（VO2 setting out of range）だった。この版の eFuse の VO2 の値（dref 6、mul 6）は、補正のない式では 2000 mV になり、カーネルの守り（1700〜1950 mV）が断っていた。
    - ESP-IDF は、blk_version ≥ 1 のとき、この eFuse の組を 1.8 V の補正値としてそのまま使う（`ldo_ll.h:123-136`）。この基板の出荷時のデモも、その値で PSRAM を動かしている。
    - そこで、eFuse の値のときだけ 2050 mV まで許すようにした（`psram.rs`）。PSRAM 32 MB の調整とメモリの試験（32 MB 全体、誤りなし）が通った。
  - **2 つ目のコア**:
    - 1 度、Core1 が 100 ms の内に上がらず（`smp.started cores=1`）、`selftest.partitions` が 1 コアの値（200‰、433‰）で落ちた。
    - ESP-IDF の `start_other_core` と同じく、ソフトウェアの stall を解く書き込み（PMU の CPU_SW_STALL の Core1 を 0xFF）を足した。その後は 5 回の起動すべてで 2 コア、自己試験が通った（`partitions` は 400‰ → 799‰）。
    - ただし、そのとき読んだ stall のコードは 255（止まっていない）だった。この書き込みが直したのかは分かっていない（最初の 1 回だけの現象かもしれない）。
  - **自己試験**: すべて通った。画面の試験（m6c）は `no display` で飛ばした。
  - **自動起動**: ストアの像には自動起動の印がないので、`fs` と `init` の自動起動の版をシリアルから入れた（`install`）。
  - **SD**: `sd.absent`（応答なし）。カードがないか、電源（GPIO45、VO4）が入っていない。
  - **Wi-Fi**:
    - C6 は SDIO で応え（`wlan.up`、chip_id 13）、`wifi` は MAC を読んだ。
    - `iotap`（WPA3）につながり、DHCP で 192.168.2.135/24 を得た。
    - `wifi.bt_failed step=init` と `request_failed esp_err=12290` が出た。C6 のファームウェアの版の違いと推測している（後で調べる）。
  - **常駐の見張り**（doc/56）: 次のものは起動し直しの後であきらめた。どれも EVB のための起動のリストがないためで、見張りは正しく動いた。
    - `term`（画面なし）
    - `matter`（SD のストアなし）
    - `netctl`（`NET.RUL` なし）
- **SD（v1.5.2）**:
  - 利用者が SD を挿しても `sd.absent` のままだった。SD のピン（GPIO39〜44）の電源が入っていなかった。
  - 利用者の許可（「入れてよい（EVB だけ）」）の後、EVB のボードのコード（`p4evb::sd_power_on`）で LDO VO4 を 3.3 V の素通し（TIEH）にした。順番は ESP-IDF の `esp_ldo_acquire_channel` と同じ。
  - GPIO45（SD_PWRn）には触らない。Tab5 は今までどおり VO4 に触らない。
  - 32 GB のカード（SK32G）が 4 ビット、40 MHz で応え、`fs.ready`、`init` は `T5OS/START.CFG` を読んだ。
- **R2: Ethernet（v1.5.2、2026-11-06）**:
  - 利用者が EVB の LAN をルーターにつないだ。
  - **作り**:
    - `kernel/src/platform/esp32p4v1/emac.rs`: EMAC を RMII で動かす。REF_CLK は PHY が出す 50 MHz を GPIO50 で受け、PLL は使わない。
      - ピン、PHY のリセット（GPIO51）、MDIO、IP101 のリセットと自動交渉。
      - DMA の記述子（鎖、64 バイト）とバッファ（1536 バイト、受信 12、送信 6）は、PSRAM のあるときに空く内蔵 SRAM（0x4FF9_0000）に置く。L1 の書き戻しと無効化を、DMA に渡す前と読む前に行う。
      - 割り込みは CPU の線 7。受信でコアを起こすだけで、読むのはスケジューラのループ。
      - レジスタの手順は ESP-IDF（esp_hal_emac、esp_eth、v5.4 の ip101）と hw_ver1 のヘッダから。
    - `kernel/src/eth.rs`: `netdev:eth0`。フレームの読み書き、MAC（eFuse の工場の MAC + 3、ESP-IDF の `ESP_MAC_ETH` と同じ）、`LINK`（PHY の状態を 0.5 秒ごとに読み、変わったら `LINK_CHANGED`）、`eth.stats`。
    - ボードの `ETH`（Tab5 は false、EVB は true）。
    - `net`: リンク `eth0`（4 つ目のインタフェース）、DHCP のクライアント。リンクが上がるたびに DHCP をやり直す。`proto::link::ETH0`、許可 `netdev:eth0`。
  - **結果**:
    - PHY はアドレス 1（ID 0x0243/0x0C54）、自動交渉で 100 Mbit/s の全二重。
    - DHCP で 192.168.2.136/24 を得た。
  - **割り込みの間違い**:
    - 最初は割り込みの元を 84 にしていた。`interrupts.h` の列挙を数えるときに、別名の行で数を誤っていた。
    - そのため受信でコアが起きず、ping の往復が 3〜708 ms と揺れた（`eth.stats irqs=0`）。
    - 割り込みの表（`interrupt_core0_reg.h` の SBD_INT_MAP が +0x170）から 92 に直すと、`irqs` が受信の数に合い、往復は 0.2〜0.4 ms でそろった（40 回中 39。最初の 1 回は ARP と推測）。
  - **PC からの ping**:
    - SD の守りの規則（`policy in drop`）が落とすので、測る間だけ `netctl 3` にした。終わりに `netctl 2`（`rules=6`）へ戻すことを、スクリプトの `finally` で必ず通した。
    - 1400 バイトの ping も通った（最初の版で 20 回中 18）。
  - **未解決**: Wi-Fi（192.168.2.135）にも同じ /24 のアドレスがあるとき、PC から Wi-Fi のアドレスへの ping は返らなかった。同じサブネットに 2 つのリンクがある形は、ルーターの使い方ではない。R3 で WAN と LAN を分ける。
- **R3a: 出口を有線にも（2026-11-06）**:
  - **`net`**:
    - 出口のリンク（`Stack::uplink`）を足した。有線の DHCP がルーターを教えていれば `eth0`、そうでなければ `wlan0` を使う。Tab5 はいつも `wlan0`。
    - これに従うもの:
      - 既定の経路
      - 名前の解決（`RESOLVE`）
      - 下流に配る DNS サーバ
      - DNS の中継の上流
    - DNS の中継は `test0` に限らず、出口でないどのリンクにも置ける。出口が変わったら作り直す（有線の DHCP は Wi-Fi のより後に終わることがある）。
    - 規則に `address on LINK A.B.C.D/N` を足した。そのリンクの DHCP クライアントを止めて固定のアドレスにする。規則から消えたら DHCP に戻す。
  - **`netctl`**:
    - モード 8: 試験用 LAN と PC を `eth0` の陰に masquerade、試験用 LAN に DHCP サーバ。
    - モード 9: 場面 B のルーター。`eth0` を 192.168.60.1/24 の LAN にして DHCP サーバを置き、Wi-Fi の陰に masquerade。守りの規則つき。
  - **実機（v1.5.2、Ethernet はルーターに）**: 試験のスクリプトは、最後に必ず `reboot` する（`net` を普通に起動し直し、SD の守りの規則を読み直す）。手順は次のとおり。
    1. `net` を止めて `run net 3`（試験用 LAN と、DHCP でアドレスを取る相手）にした。
    2. `netctl 8` を入れた。
    3. 相手は EVB の DHCP サーバから 192.168.50.100 を借りた。
    4. `http://1.1.1.1/` の応答（301）が 27 ms で届いた。DNS の中継で `one.one.one.one` も引けた。
    5. その間、Wi-Fi の送信の数（`wlan.stats sta_tx`）は変わらず、外へは `eth0` から出た。`net.fwd forwarded=15`。
    6. 再起動の後、`netctl.done mode=0 rules=6`（守りの規則）に戻ったことを確かめた。
- **R3b: 有線の LAN に本物の端末（2026-11-06）**:
  - **つなぎ方**: 利用者が、EVB の Ethernet を PC の USB Ethernet（`enx0090feb4eff8`）につないだ。
    - PC 側は NetworkManager の固定アドレス 192.168.10.1/24（DHCP なし）で、PC の設定は変えない（判断）。
    - そこで `netctl` にモード 10 を足した。モード 9 と同じルーターの規則を、PC の網の 192.168.10.0/24 で行う。EVB は 192.168.10.254、DHCP は .100〜.149。
  - **結果**:
    - `net.addr_fixed link=eth0`（192.168.10.254/24）。PC の USB Ethernet から EVB への ping は 10 回中 10 回、0.3〜0.7 ms。
    - PC の ARP は EVB の MAC `30:ed:a0:e2:78:79`（工場の MAC + 3）を引いた。
    - **転送**: 利用者が root で 1.1.1.1 への経路（/32）だけを足し、`curl http://1.1.1.1/` を実行して経路を消した。EVB で次のことを確かめた。
      - `pass forward on eth0 out wlan0` が 6 回、戻りの `pass forward state established,related` が 4 回。
      - `net.nat mapped=1`（Wi-Fi の陰に masquerade）。
      - PC の USB Ethernet の送信が 10、受信が 8 増えた。
    - **DHCP**: `udhcpc` の要求は EVB に届かなかった（`pass in on eth0` の数が変わらない）。PC の側で要求が出ていないとみられる（出力は未確認）。EVB の DHCP サーバは、R3a の試験用 LAN で貸し出しを確かめた同じもの。
  - 試験の間、EVB は `netctl 10`（守りの規則つき）。再起動で SD の規則（守りの組）に戻る。
  - **残り**: 本物の端末への DHCP の貸し出し、スマホなどの端末での確かめ。
- **R4: C6 の soft-AP（2026-11-06、実装と起動まで）**:
  - **カーネル**: `netdev:ap0`（`wlan.rs` の `ApDev`）。
    - esp_hosted の if_type 2（ESP_AP_IF）のフレームを、別の受信の列（4）で受ける。
    - 送るのは STA と同じく Core0（送信の列 4、`flush_tx`）。
    - `wlan:ctl` の `SET_AP_MAC` で、`wifi` が AP の MAC を教える（0 = AP なし）。
  - **`wifi`**: `run wifi` の引数の bit 3 で、AP と STA の両方（`WIFI_MODE_APSTA`）。
    - AP の設定（`Req_WifiSetConfig` の iface 1）: SSID とパスフレーズ、WPA2-PSK、子機は 4 台まで、PMF capable。チャネルは STA に従う。
    - AP の MAC を `Req_GetMacAddress` の iface 1 で読む。
    - 子機の出入り（イベント 771/772）を `wifi.ap_joined`/`wifi.ap_left`（MAC と aid）でログに出す。
    - SSID とパスフレーズは、ビルドのときにリポジトリの外のファイル（`$T5_WIFI_AP_FILE`、既定 `~/.config/tab5-os/wifi-ap.txt`）から読む。STA と同じ扱い。パスフレーズは乱数で作り、ログにも doc にも出さない。
  - **`net`**: リンク `ap0`（5 つ目のインタフェース、`proto::link::AP0`）。
    - `wifi` が AP を始めた後に開く（1 秒ごとに試す）。DHCP のクライアントはない。
    - 開いたら規則を当て直す（`address on ap0 …`）。
    - 許可 `netdev:ap0`。
  - **`netctl` 11**: AP のルーター。`ap0` を 192.168.70.1/24 の LAN にして DHCP サーバを置き、Wi-Fi の STA か有線の陰に masquerade。守りの規則つき。
  - **実機（v1.5.2）**:
    - `run wifi 9`（BT と AP）で `wifi.ap ssid=t5evb-ap mac=b4:3a:45:99:be:11`（STA の MAC + 1）。STA は `iotap`（チャネル 11）につながったまま。
    - `net.link link=ap0`、`netctl 11` で `net.addr_fixed link=ap0`（192.168.70.1/24、`rules=7`）。
    - PC からの Wi-Fi の探索は NetworkManager が許さなかった（not authorized）。
    - **子機（利用者の端末）**: 利用者が `t5evb-ap` につないだ。`netctl 4` の数は次のとおり。
      - `pass in on ap0` 70（DHCP と DNS の中継）
      - `pass forward on ap0 out wlan0` 850（AP から STA の `iotap` へ転送）
      - 戻りの `pass forward state established,related` 1051
      - `net.nat mapped=54`（STA のアドレスの陰へ masquerade した接続）
      - 子機が出入りしたときと DHCP の貸し出しのログは、つなぐ前にコンソールの列からあふれて残らなかった（`log.dropped`）。
    - **場面 A に当たる形**（LAN = AP）が、STA を出口にして動いた。有線を出口にする場合は `pass forward on ap0 out eth0` で同じ規則が働く（未試験）。
- **R5: 有線の速さと負荷（2026-11-06、その 1）**:
  - **測り方**: PC の USB Ethernet（192.168.10.1）と EVB の `eth0`（192.168.10.254、`netctl 10`）を直結し、`nettest 1`（TCP の echo）に PC から送り、送り返させる。経路を足さないので root は要らない。試験のスクリプトは最後に必ず再起動する。
  - **最初は 0.1 Mbps しか出なかった**:
    - PC の TCP は再送が 89〜112 回。EVB の `net` は 1 コアの 995‰ を使っていた。
    - 切り分けのために `eth.stats` に数を足した。DMA が落としたフレーム（記述子なし、受信 FIFO）、状態の誤りで捨てたフレーム（CRC、PHY の受信の誤り、分割）、記述子が古く見えた回数。
    - 受信のリングを 12 → 32、送信を 6 → 8 にして、PSRAM のあるときに空く内蔵 SRAM の 0x4FF8_D000 に移した。記述子なしで落ちるのは 0 になったが、遅いままだった。
    - CRC の誤りも 0。PC の側でチェックサムの誤りもない（`nstat`）。
    - プロファイラ（`prof`）で、`net` 自身は 15 秒で 236 標本しか使っていなかった。
  - **原因**:
    - スケジューラのループは、眠らない条件に `eth::pending()`（`net` が読むべきフレームがある）を入れていた。その間、鍵を持ったまま回り続けた。
    - `net` がもう一方のコアで動いていると、そのシステムコールは鍵を待ち、その待ちが `net` の CPU に数えられた。
    - Wi-Fi の `wlan::pending()` は、ループ自身がする仕事（SDIO からの読み）なので正しい。Ethernet のは `net` の仕事なので、眠らない理由にならない。
    - 受信の割り込みが眠ったコアを起こし、ループは眠る前に待っている `net` を起こすので、条件から外した。
  - **直した後**（8 MiB の echo、片道）:

    | 負荷 | 取り分（doc/57） | Mbps | `net`（全コアの ‰） |
    |---|---|---|---|
    | なし | — | 15.5〜15.7 | 501（1 コアの 940） |
    | spin 2 つ（apps） | なし / あり | 17.3〜17.4 / 17.5〜17.6 | 406〜411 / 414〜415 |
    | spin 4 つ（apps） | なし | 14.5、15.5 | 344、365 |
    | spin 4 つ（apps） | **あり** | **16.6、16.8** | **394、400** |

    - 有線の echo は `net` の 1 コアで頭打ち（100 Mbit/s の線ではない）。
    - spin が 4 つのとき、取り分があれば `net` はちょうど 40%（400‰）を得て、速さを保った。なければ 34〜37% で、速さも落ちた。doc/57 の自己試験と同じ効き方が、本物のネットワークの負荷で見えた。
- **R5 その 2: 有線のフレームの輪（2026-11-06）**:
  - **作り**:
    - `netdev:eth0` に `MAP_RING`/`KICK`（Wi-Fi と同じ輪の形、`wlan.rs` の `KRings` を共にした）。
    - スケジューラのループが、EMAC の受信を輪へ写し、輪の送信を EMAC へ渡す。
    - 「眠らない」のは、受信が待っていて、かつ輪に空きがあるときだけ。`net` が取っていないフレームは、ループの仕事ではない（その 1 の誤りをくり返さない）。
    - `net` は変えていない（`Port::open` が輪を試す）。
  - **PMP**: プロセスは輪を 2 組持てる（エントリ 9〜10 と 13〜14）。`net` が `wlan0` と `eth0` の両方を使うため。Tab5 のカーネルも同じ表で、t5emu の試験は通った（Tab5 の実機では、まだ見ていない）。
  - **輪の置き場所**:

    | 置き場所 | なし | spin 4（取り分なし） | spin 4（取り分あり） |
    |---|---|---|---|
    | 輪なし（フレームごとの `read`/`write`） | 15.5〜15.7 | 14.5、15.5 | 16.6、16.8 |
    | PSRAM（`shm` の池） | 13.7 | 12.0、12.8 | 13.9、14.0 |
    | **内蔵 SRAM**（EMAC のバッファの後） | **16.1** | 13.4、13.9 | **15.9、16.3** |

    - PSRAM では遅くなった。内蔵 SRAM にして、EMAC の受信を 12、送信を 4 に戻した（受信の溢れは、その 1 の誤りが原因だった）。
    - 輪の効き目は約 3% と小さい。`net` の頭打ちは、システムコールより `net` の中の TCP の処理と写しにある（推測、次はプロファイラで見る）。
    - 8 MiB × 5 回で PC の再送は 69（約 0.2%）。
- **R5 その 3: `net` の中のプロファイル（2026-11-06、EVB v1.5.2）**:
  - **測り方**: 16 MiB の echo を 1 回ずつ（`prof` は 1 ms ごと、echo の間だけ）。比べるときは、同じ手順で起動ごとに 2〜3 回測った。
  - **`net` の時間の内訳**（標本 7,375 個）:
    - U モード 63%:
      - `Stack::poll` 自身 11.5%（smoltcp の `poll` が、リンクごとに最大 20 のソケットの枠を毎回なめる。目立つ一か所はない）
      - `memcpy` 11%
      - NAT の入口の検査 3.6%、チェックサム 3.2%、`memset` 1.9%
    - カーネル 37%:
      - `KICK` 11%
      - `TIME` 8.8%
      - `READ` 6.7%
      - `nettest` への `send` 5.8%
  - **システムコールあたりのカーネルの中のサイクル**（`mcycle`）:
    - `TIME` 2,150（10 秒で 28,000 回）
    - `WAIT` 7,100
    - `KICK` 13,000
    - 標本での時間は、この 4 倍ほどになる。差は鍵の待ちと、トラップの出入り。
  - **鍵の待ち**: echo の間、Core1（主に `net`）は 10 秒のうち 1.2 秒、鍵を待っていた。Core0 は、スケジューラのループで機器を見るたび（`wlan`、`eth`、ウォッチドッグ）に鍵を持つ。
  - **`mcycle` は当てにならない**:
    - 区間ごとのサイクルを数える仮の計測を入れたところ、SYSTIMER の読み出しが 1 回約 210 サイクルと出た。doc/34 の時計で測った値（約 2.2 µs、約 790 サイクル）と合わない。バスを待つ間は数えないとみている（推測）。周辺機器やメモリを待つ処理は少なく出る。
    - この仮の計測そのものも、速さを約 1 割落とした（鍵の待ちが 0.6 → 1.5 秒）。計測は外した。
  - **入れたもの**:
    - **`wait` が時刻も返す**（ABI minor 21）。a4/a5 は、もとからどのシステムコールでも壊れる扱いなので、古いアプリには影響しない。時刻は、`wait` がもともと読んだ値を使う。もう一度 SYSTIMER を読むと、1 回約 2 µs かかる。
      - `net` は 1 巡で `TIME` を 2 回読んでいた。これをやめ、`wait` の返す時刻を使う。次の待ち時間は前の `wait` の時刻で決めるので、1 巡の仕事の分だけ長めに出る。
      - 待ちの前に時刻を読み直す版（毎回、または待ち時間が 1 ms 未満のときだけ）も試したが、どちらも遅かった（15.5、15.0 Mbps）。
    - **EMAC の DMA の場所はデータのキャッシュだけ無効にする**（`dcache_invalidate`）。これまでは命令のキャッシュも無効にしていたので、ROM の呼び出しが 2 倍だった。受信のバッファは、フレームの長さの分の行だけにした。外すと 15.4 Mbps（+2% ほど効いている）。
    - **ウォッチドッグへの給餌を 100 ms ごとに**（`sched.rs` の `FEED_MS`）。これまではループの 1 巡ごとに、LP の領域へ 3 回書いていた（鍵を持ったまま、負荷の下では毎秒数万回）。
  - **結果**（16 MiB の echo、片道）:

    | 版 | 片道 | カーネル（全コアの‰） | Core1 の鍵の待ち（1 回） | `net`（1 コアの‰） |
    |---|---|---|---|---|
    | 50b8e1b（3 回の起動、7 回） | 15.8〜16.4 Mbps | 255〜268 | 1.15〜1.22 秒 | 895〜915 |
    | **この版** | **15.7〜15.8 Mbps** | **194〜201** | **0.60〜0.63 秒** | 849〜864 |
    | この版のカーネルと 50b8e1b の `net` | 15.7〜15.8 Mbps | 232〜249 | 1.0〜1.13 秒 | 881〜887 |

    - カーネルの時間は約 25% 減り、全体の CPU は同じ量のデータで約 11% 減った（両コアの空きが 24% → 33%）。
    - 速さは約 3% 落ちた。古い `net` でも同じ値なので、カーネルの側の差。ただし `nettest`（変えていない）の CPU も 75‰ → 85〜95‰ に増えている。カーネルのコードはフラッシュから動く（XIP）ので、小さな変更でもコードの並びとキャッシュの当たり方が変わる。それが効いているとみている（推測）。1 つの変更に数 % を割り当てることは、この測り方ではできない。
    - 試験: virt と p4emu が通った。p4emu の Matter の試験（26）は、最初の依頼が `matter` の準備（約 3.2 秒、CA の鍵）を 3 秒の待ちで追い越して落ちた。t5emu の時間は命令の数で進むので、カーネルの小さな変更でも時間がずれる。待ちを 5 秒にした。
    - 試してやめたもの: SDIO のカードの割り込みを、レジスタでなく割り込みの印で見る。測った差が出ず、割り込みを取り損ねたときの Wi-Fi の遅れ（20 ms の見回りまで）が増えるので戻した。
  - **次の候補**:
    - **カーネルの熱いコードを内蔵 RAM へ**: 次の「R5 その 4」で行った。
    - **大きな鍵を分ける**: 機器の見回り（`eth` の写し、`wlan`）を、鍵を放して、または機器ごとの鍵で行う。
    - **smoltcp の 1 巡**: 何も来ていないリンクの `poll` を省く。
- **R5 その 4: カーネルのよく通るコードを内蔵 RAM へ**（2026-11-06）:
  - **理由**: カーネルの `.text`（181 KB）はフラッシュから動き（XIP）、`net` のコードとデータ（PSRAM）と同じ 128 KB の L2 キャッシュを通る。トラップ 1 回のカーネルの中の時間が長いのは、カーネルのコードが追い出されるためとみた（推測）。
  - **配置**（`kernel-common.ld`、`link-esp32p4v1.ld`）:
    - **`.hot_text`**（約 51 KB）: トラップの出入り、`arch`、`sched`、`process`（システムコール）、`handle`、`channel`、`shm`、`smp`、`time`、`eth`、`wlan`、EMAC と SDIO のドライバ、`Platform` の `eth_*` とアラーム、`memcpy` などと compiler_builtins。関数の区画の名前（v0 のマングリングの `9t5_kernel5sched` など）で選ぶ。`.text` より前に書いて、こちらが先に取る。
    - **`.hot_rodata`**（約 1.2 KB）: 同じモジュールの読み出し専用のデータ（`match` の飛び先の表など）。`.rodata` より前に書く。フラッシュの区画は今までどおり 2 つ（ブートローダの条件）。
    - 置き場所は RAM_HI（0x4FF4_0000）の先頭。ブートローダがロードする。`.bss` はその後ろへずれる。
    - 場所を空けるため、Core1 のスタック（`.stack1`、64 KiB）を 0x4FF2_9000 へ移した。ブートローダが動いていた所で、カーネルが動き始めた後は空く（ROM の Core1 の起動用のスタック 0x4FF3_AFC0 より下）。ロードされない区画なので、ブートローダとは重ならない。
    - 念のため、Core1 を起こす前に、ROM のフラッシュの設定（0x4FF3_FFE8 の指す先、書き込みで使う）がこの範囲にないことを確かめる（`smp.stack1_in_use`。あれば Core1 を起こさない）。
    - QEMU virt は、`.hot_text` を RODATA の領域に置く（TEXT の先頭は `_start` でなければならない）。
  - **結果**（EVB v1.5.2、16 MiB の echo、片道）:

    | 版 | 片道 | カーネル（全コアの‰） | `net`（1 コアの‰） |
    |---|---|---|---|
    | R5 その 3（4bfb872） | 15.7〜15.8 Mbps | 194〜201 | 849〜864 |
    | `.hot_text` だけ | 27.5〜27.7 Mbps | 307〜309 | 924〜930 |
    | **`.hot_text` と `.hot_rodata`** | **28.7〜29.3 Mbps** | 307〜318 | 908〜935 |

    - 速さは約 1.85 倍。同じ量のデータで使う CPU は、カーネルが約 1 割、`net`（システムコールを含む）が約 4 割減った。フラッシュの XIP とキャッシュの取り合いが、カーネルの中の時間の大きな部分だったことになる。
    - Wi-Fi の DHCP、アプリの書き込み（ストアへの install、フラッシュの消去と書き込み）、Core1 の起動を実機で確かめた。virt と p4emu が通った。
    - **Tab5 は未確認**: 同じリンカスクリプトを使う（Tab5 の版で `.hot_text` 約 49 KB）。同じチップ（v1.0）と同じブートローダなので動く見込みだが、実機ではまだ試していない。
- **R5 その 5: 表の上限と洪水**（2026-11-06）:
  - **表の上限**（`net`）: 接続の追跡は 512（ハッシュから 8 つの枠を見る）、NAT の対応づけは 256。満ちると新しい接続は記録されない。
    - 守りの規則では、戻りの向きは `state established,related` でしか通らない。表が満ちている間は、新しい転送の接続や、`net` が新しく外へ出す接続の答えが落ちる。答えのない項目は UDP で 30 秒、TCP の SYN で 60 秒残る。
    - PC から、送り元の port を変えた UDP を 1500 個送ると、表は 512 で満ちた（`table_full` 840）。
    - DNS の中継は満ちた後も答えた。中継の上流への流れは 1 つの port で、満ちる前に記録済みだったため。
  - **洪水**（PC の USB Ethernet から、64 バイトの UDP を閉じた port へ全速で 10 秒）:
    - PC は 96,000 個/秒を出したつもりだが、EVB に届いたのは約 18,600〜31,000 個/秒（PC の USB Ethernet が上限とみられる）。
    - `net` は 1 つずつ ICMP の port unreachable を返していた（受信と送信が同じ数）。
    - 洪水の間、ping は 90% 落ち、DNS は答えなかった。受信の DMA が記述子なしで落とした（`missed`）。洪水が止むと元に戻った。
  - **入れたもの**（`filter.rs`、`nat.rs`）:
    - **ICMP の誤りの送信の上限**: `net` が自分で出す ICMP の誤り（out の向きで、引用を含むもの）は、20 個まで続けて、その後は 1 秒 100 個まで。Linux の `icmp_msgs_per_sec` と同じ考え。数は `net.conntrack` の `icmp_limited`。
    - **満ちたときの早めの捨て**: 接続の追跡で、見る 8 つの枠に空きがなければ、答えのない項目のうち一番早く切れるものを捨てて入れる（Linux の early drop と同じ考え）。答えのある項目は残す。NAT も同じく、答えのない対応づけのうち一番早く切れるものを捨てる。数は `evicted`。
  - **結果**（同じ試験）:

    | | 前 | 後 |
    |---|---|---|
    | 表を満たした後の `table_full` | 840 | 0（`evicted` 559） |
    | 洪水の 10 秒に `net` が受けたフレーム | 約 129,000 | 約 240,000 |
    | 洪水の間の送信 | 約 129,000 | 約 500（`icmp_limited` 240,384） |
    | 洪水の間の ping | 10 回中 1 回 | 10 回中 2 回 |

    - 処理できるフレームは約 2 倍になったが、届くフレームのほうが多く、ping と DNS は洪水の間は通らないまま。受信の DMA の手前で選り分ける手段（ハードウェアのフィルタ、`net` 以外での前処理）がないため。100 Mbit/s の最小のフレームの洪水（約 148,000 個/秒）には、この方式では追いつかない。
    - 答えのない項目を捨てるので、洪水の間に始めた本物の接続も、答えが来る前に捨てられることがある。項目を捨てるのは、見る 8 つの枠の中で一番古い答えのないものからなので、新しい接続はすぐには捨てられない（完全ではない）。
    - 試験の後は、どれも元に戻った（ping、DNS、表は 8 項目）。
- **R5 その 6: 転送の速さと NAT の表**（2026-11-06）:
  - **つなぎ方**: 利用者が root で、Cloudflare の速度試験（162.159.140.220）と 1.1.1.1 への経路（/32）だけを EVB（192.168.10.254）に向けた。PC → USB Ethernet → EVB の `eth0` → 転送 → Wi-Fi（`iotap`、masquerade）→ 外。EVB は `netctl 10`。
  - **転送の速さ**（Cloudflare の速度試験、HTTPS）:
    - 受信 25 MB: 20.5〜21.8 Mbps（3 回、表が空いているとき）
    - 送信 10 MB: 18.0〜18.8 Mbps
    - PC から直接（EVB を通さない）は受信 約 570 Mbps、送信 約 350 Mbps なので、上限は EVB と Wi-Fi の側。受信の間、`net` は 1 コアの約 290‰、カーネルは全コアの 186‰。Core1 はほぼ空いていた。有線どうしの echo（約 29 Mbps）より遅いので、Wi-Fi（C6 と SDIO）が先に効いているとみる（推測）。
  - **NAT の表が満ちる**: 1.1.1.1 へ、送り元の port を変えた DNS の問い合わせを流れごとに 1 つ送った。
    - 256 項目の版: 10 ms おきの 300 個で答えは 256（表の大きさちょうど）。すぐ続けた 300 個は 4 しか答えなかった。答えのあった UDP の対応が 120 秒残り、どれも捨てられないため。
    - Linux は、1 往復だけの UDP（DNS）を 30 秒で消し、流れが続くものだけ 120 秒（`nf_conntrack_udp_timeout` と `_stream`）。
  - **入れたもの**（`nat.rs`、`filter.rs`）:
    - **UDP の寿命**: 接続の追跡と NAT の両方で、答えの後にも最初の側から送る「流れ」だけ 120 秒、それ以外は 30 秒。
    - **NAT の表を 2048 項目に**、探し方をハッシュに。これまでは、パケットごとに表の全部（256）を見ていた。今は、内側の端と相手のハッシュ（起動時の乱数が鍵）から 16 の枠を見る。外側のポートを 30000 + 枠の番号にしたので、返事は表を探さずに対応にたどり着く。鍵があるので、外からポートを当てられない。
    - **接続の追跡を 2048 項目に**、見る枠を 8 → 16 に。512 では、NAT より先に満ちた（`table_full` 658）。`net` の .bss は約 1.28 MB に増えた（2 MB の領域の中）。
  - **結果**（2048 の版、2 ms おき）:
    - 300 個と 600 個は全部答えた（その時点で 1182 項目）。続けて 1000 個は 848 個（約 2000 項目で満ちた。`table_full` 79、NAT の `full` 81）。
    - 満ちている間に始めた HTTPS は、最初のバイトまで 7.2 秒かかった（SYN が記録されず、答えが落ちて再送）。答えのある項目ばかりで満ちると、捨てられるものがない。30 秒で空いた後の 300 個は全部答えた。
    - 起動の直後の 1 回だけ、10 ms おきの 300 個で答えが 23 だった。同じ条件でやり直すと 298〜300 だった。原因は分からない（外の側の制限かもしれない、推測）。
  - **経路**: 試験の後、利用者が消す。EVB は再起動して SD の守りの規則に戻した。
- **R5 その 7: Wi-Fi の側の上限の切り分け**（2026-11-06）:
  - **測り方**: PC の `enp5s0`（192.168.2.14）と EVB の `wlan0`（192.168.2.112）は、同じ家の LAN（AP の `iotap` の向こう）にいる。経路を足さずに、Wi-Fi だけを通る試験ができる。
    - 試験の間だけ `netctl 3`（規則なし）にし、終わりに必ず再起動して SD の守りの規則に戻す。
    - `nettest 1` に、受けるだけ（`arg` 1）と送るだけ（2）を足し、向きごとに測る。
  - **Wi-Fi だけ（TCP、8 MiB、それまでの `net`）**:
    - echo（両方向を同時に）: 片道 12.3〜13.8 Mbps（Tab5 の doc/35 と同じ）
    - PC → EVB: 22.0〜22.5 Mbps
    - EVB → PC: 18.5〜20.8 Mbps
    - ping は負荷の下でも 2〜5 ms。`net` は 1 コアの 40〜50%、Core0 は 20〜37% 空き。CPU は上限でない。
    - 転送（その 6、受信 21、送信 18 Mbps）とほぼ同じ値。
  - **UDP（PC → EVB、1400 バイト、決まった速さで 3 秒）**: 10〜40 Mbps は 96〜100% 届き、60 Mbps では 44 Mbps（73%）届いた。Wi-Fi、C6、SDIO、カーネル、`net` の受信の道は、44 Mbps 以上を運べる。
    - 参考: esp_hosted の資料（`docs/performance_optimization.md`）は、P4 と C6、SDIO 4 ビット 40 MHz、2.4 GHz の開けた所で、SDIO だけ 72〜80、iPerf の TCP 30〜32、UDP 約 50 Mbps。C6 のファームウェアは性能向けの設定（バッファ 64、AMPDU の窓 32）、lwIP の窓は 64 KB。
  - **見立て**: TCP が UDP の半分ほどなのは、`net` の TCP の窓（ソケットごとに 16 KiB）。負荷の下の往復 3〜5 ms では、窓の分しか送れない時間ができる（推測。窓を大きくすると速くなったことと合う）。
  - **入れたもの**: `net` の TCP のバッファを 16 → 32 KiB に、ソケットの数（`POOL`）を 16 → 12 に。メモリは 256 KiB 増え（.bss 約 1.46 MB）、スタックを入れて 2 MB の領域の約 1.8 MB。
    - 試しに 48 KiB（ソケット 8）も測った。PC → EVB 23.8〜24.6、EVB → PC 26.8〜30.8 Mbps で、32 KiB とほぼ同じ。ソケットを残すほうを選んだ。
  - **結果**:

    | | 16 KiB（16 個） | **32 KiB（12 個）** |
    |---|---|---|
    | Wi-Fi、PC → EVB | 22.0〜22.5 Mbps | **22.5〜24.8 Mbps** |
    | Wi-Fi、EVB → PC | 18.5〜20.8 Mbps | **25.6〜28.5 Mbps** |
    | Wi-Fi、echo | 12.3〜13.8 Mbps | 13.5〜13.7 Mbps |
    | 有線の echo（16 MiB） | 28.7〜29.3 Mbps | **31.3〜31.4 Mbps** |

    - TCP は UDP の 55〜65%。esp_hosted の資料の比（30/49 = 61%）と同じくらいで、Wi-Fi の上の TCP として普通の値とみる。
    - echo は Wi-Fi の空中を両方向で分け合うので変わらない。
    - 転送（PC → EVB → 外）は、端どうしの TCP の窓で決まるので、この変更とは関係しない。転送の約 20 Mbps は、C6 のファームウェアの設定と電波の状態が効いているとみる（推測）。
  - **さらに上げるなら**: C6 のファームウェアを性能向けの設定で作り直す（EVB の C6 の更新は許可済み）。esp_hosted の資料では TCP 30 Mbps 前後。
- **R5 その 8: EVB の C6 のファームウェアを esp_hosted 2.12.7 に（OTA）**（2026-11-06）:
  - **前の状態**: EVB v1.5.2 の C6 は出荷時の esp_hosted v0.0.6（esp_hosted の資料 `docs/esp32_p4_function_ev_board.md`）。`wifi` は 2.12.7（Tab5 の C6 と同じ）に合わせてあり、Wi-Fi と AP は動いていたが、Bluetooth は `wifi.bt_failed` だった。
  - **利用者の判断**: 書き込みは P4 からの OTA（「P4 から OTA（推奨）」）。
  - **先に試したこと**: Wi-Fi のバッファの数（`wifi` が `Req_WifiInit` で渡す、0.0.6 もその値を使う）を性能向けの値（静的受信 16、動的受信と送信 64、受信の窓 32）にした。速さはばらつきの中で変わらなかったので戻した。
  - **ファームウェアの作り方**（ESP-IDF v5.5.5、esp_hosted 2.12.7 の `slave`）:
    - `sdkconfig.defaults.esp32c6` に、esp_hosted の資料 `docs/performance_optimization.md` の値を C6 の名前で足す: `CONFIG_ESP_WIFI_STATIC_RX_BUFFER_NUM=16`、`CONFIG_ESP_WIFI_DYNAMIC_RX_BUFFER_NUM=64`、`CONFIG_ESP_WIFI_DYNAMIC_TX_BUFFER_NUM=64`、`CONFIG_ESP_WIFI_RX_BA_WIN=32`。
    - `idf.py set-target esp32c6 && idf.py build`。依存の部品は Espressif の部品の登録所から取る。
    - SDIO はストリーミングのモード（既定、Tab5 の C6 と同じ）。像は 1,225,232 バイト。0.0.6 の区画表は OTA の区画が 1536 KiB ずつで、OTA は区画表を書き換えないので、像はそれに収まる必要がある。
  - **OTA の作り**:
    - `wifi`: `svc:wifi/ota` の要求 `OTA_BEGIN`（大きさ）、`OTA_DATA`（中身）、`OTA_END`（`t5_abi::wifi`）。1 KiB ずつ esp_hosted の `Req_OTAWrite` にする（`Req_OTABegin`/`Write`/`End` は 0.0.6 も同じ番号）。像の頭（0xE9、チップの ID 13 = C6）と大きさを確かめる。`Req_OTABegin` は区画を消すので、OTA の要求は 30 秒待つ。
    - `c6ota`（新しいアプリ）: `run c6ota IP PORT` で `http://IP:PORT/c6.bin` を取り、同じ確かめをしてから `wifi` に渡す。許可は `svc:net/tcp-connect` と `svc:wifi/ota`。`svc:wifi/ota` は、システムの鍵の像だけが持てる許可（`t5a::strong`）に足した。
    - `wifi` の不具合を直した: 客（`matter`）がいる間に別の客がつなごうとすると、待ちの集合の受け口がいつも準備済みになり、`wifi` が 1 コアを使い切って回っていた（客の終わりも見えなかった）。客がいる間は受け口を待たない。次の客は、今の客が去ってからつながる。
  - **実機での手順**（PC の USB Ethernet に `python3 -m http.server 8070 --bind 192.168.10.1`、EVB は `netctl 10`、`wifi` の客の席をふさぐ `matter` を止めてから）:
    1. C6 でない像（P4 のカーネル）: `c6ota.bad_image`、何も送らない。
    2. 本物の像の先頭 64 KiB だけ: 最後の `Req_OTAEnd` で C6 が `ESP_ERR_OTA_VALIDATE_FAILED`（5379）を返した。起動する区画は変わらない。道の全体（区画の消去、書き込み）を確かめた。
    3. 本物の像: 1,225,232 バイトを 12 秒で書き、`Req_OTAEnd` が通った。C6 は 5 秒後に新しい版で起動した。
  - **結果**:
    - 起動の後 `wlan.fw_version major=2 minor=12 patch=7`。Wi-Fi（`iotap`）、DHCP、Bluetooth（`wifi.bt_started`。0.0.6 では失敗していた）、AP と STA の同時（`run wifi 9` で `t5evb-ap`）が動いた。
    - 速さはほぼ同じ:

      | | 0.0.6 | 2.12.7 |
      |---|---|---|
      | TCP、PC → EVB | 22.5〜24.8 Mbps | 24.4〜25.1 Mbps |
      | TCP、EVB → PC | 25.6〜28.5 Mbps | 26.8〜27.5 Mbps |
      | UDP、PC → EVB の上限 | 44.0 Mbps | 44.2〜44.6 Mbps |

    - UDP の上限（約 44 Mbps）は、esp_hosted の資料の開けた所の値（約 49 Mbps）の 9 割で、ファームウェアでは変わらなかった。電波（2.4 GHz、チャネル 11、家の中）が効いているとみる（推測）。
    - 0.0.6 に戻すには、同じ OTA で 0.0.6 の像を送る（像は Espressif の部品の登録所の 0.0.6 から作れる）。C6 が起動しなくなったときは、PROG_C6 の端子から書く。
- **R5 その 9: `wifi` の客を同時に 4 つまで**（2026-11-07）:
  - これまでは客が 1 つずつで、`matter` が席を持ち続けるため、`c6ota` を使うには `matter` を止める必要があった。
  - `svc:wifi` の客を 4 つの席に持つ（プロセスのハンドルは 16 まで）。席が全部埋まっている間だけ、受け口を待ちの集合から外す（その 8 の空回りを避けるため）。
  - 答えの宛て先:
    - `JOIN` と `REVERT` の答え（`JOINED`、`FAILED`）は、頼んだ客へ。答えの前に別の客が `JOIN` すると、前の客には `FAILED`（理由 0）を返す。
    - OTA は始めた客のもの。ほかの客の `OTA_BEGIN`/`OTA_END` は段 1 で断り、`OTA_DATA` は捨てる。始めた客が途中で去ったら、OTA をやめる（`wifi.ota_abandoned`。`Req_OTAEnd` を送らないので、C6 の起動する区画は変わらない）。
    - `STATUS`、`SAVE`、`DENIED` は頼んだ客へ。
  - `c6ota` に `run c6ota 0 0` を足した。何も進んでいない OTA の `OTA_END` を送り、段 1 の断りが返れば終わる（C6 には何も送らない）。`wifi` が答えるかを確かめるため。
  - 実機（EVB v1.5.2）: `matter` が席を持ったまま、`run c6ota 0 0` を 1 回、続けて 2 つ。どれも答えを受けて終わった（exit 0）。`wifi` の CPU は 0‰、`matter` は動き続けた。
  - **残り（R5 の続き）**:
    - 洪水の間の取りこぼし（DMA の手前で選り分けられない）。
- **R1b（チップ v3、v1.6.0）の調べ**（ESP-IDF の hw_ver1 と hw_ver3 のヘッダを全部突き合わせた）:
  - **レジスタの配置はほぼ同じ**。違うのは次のとおり。
    - 乱数（v3 は LP_TRNG の 0x5012_6004 と有効化）
    - CLIC（v3 は閾値が CSR `mintthresh`、属性の mode が書ける）
    - DSI の PLL の参照（画面がない EVB では要らない）
  - **止まる理由はメモリと起動**:
    - v3 は L2 キャッシュを L2MEM の下の端に置くので、カーネルの RAM（0x4FF00000 から 0x29000）と重なる。
    - ROM のデータは 0x4FF3Fxxx → 0x4FFBFxxx に移る。カーネルは 0x4FF3_FFE8 を決め打ちにしている。
    - ブートローダは `ESP32P4_SELECTS_REV_LESS_V3=n` で作り直しが要る。
    - CPLL は 400 MHz で、PVT の値も違う。
  - どれも定数と小さな `#[cfg]`、2 つ目のリンカスクリプトで済む見込み。feature `chip-v3` にする。ドライバの書き直しは要らない。PSRAM は 200 MHz の設定のままでも動く見込み（推測）。
- **R1b その 1: チップ v3 のカーネル（EVB v1.6.0、2026-11-07、途中）**:
  - **作ったもの**:
    - feature `chip-v3`（`make image-p4evb3`）と `kernel/link-esp32p4v3.ld`。v3 はブートローダが 128 KiB の L2 キャッシュを L2MEM の下の端（0x4FF0_0000〜0x4FF2_0000）に置く。
      - RAM: 0x4FF2_0000 から 0x25000。
      - RAM_HI: 0x4FF8_0000 まで（v1 と同じく、アプリの領域と Wi-Fi/Ethernet のリングの手前まで）。
      - Core1 のスタックは、ブートローダの領域（0x4FFA_0000〜0x4FFB_AFC0）に置く。
    - ROM の関数の番地は v1 と全部同じだった（esp32p4.rom.ld と rom.eco5.ld を突き合わせた）。違うのは `rom_spiflash_legacy_data`（0x4FFB_FFE8）だけ。
    - CLIC の閾値は、v3 では CSR `mintthresh`（0x347）にもある。両方を 0 にする。
    - CPLL は 400 MHz（v1 は 360 MHz）。分周は同じで、CPU 400、MEM 200、APB 100 MHz。ESP-IDF は v3（eFuse のブロック版が 100 以上）では PVT を使わず、既定の dbias 24 で 400 MHz で動かす。ブートローダもその値のまま渡す。
    - ブートローダは `boot/sdkconfig.v3.defaults`（`make boot-v3`、REV_LESS_V3=n、最小の版 3.0）。`elf2t5a kernel` に `--chip-rev` を足した。
  - **実機（利用者の確認の後）**:
    - フラッシュ全体を `build/evb160-flash-backup.bin`（sha256 727a484f…）に読み出した。espflash の ROM モードでは読めず、ESP-IDF 6.0 の esptool の stub を使った。
    - esptool の `--erase-all` で全体を消してから書いた（確認のときに挙げたのは otadata の消去だけだった。出荷時の中身は上のバックアップにある）。
    - 最初の版（CPU 100 MHz のまま）は 2 回起動した: 2 コア、PSRAM 32 MB（memtest 0 件）、フラッシュの写像、乱数。
    - その後、USB の Serial/JTAG への入力が止まった。電源を入れ直すと列挙もされなかった。ダウンロードモードでも ROM が例外の中で回り、書き込みが `Write timeout` になった。
    - JTAG（OpenOCD）は使え、ota_0 を消すと戻った。続く 400 MHz の版の書き込みも途中で切れたので、USB の線や給電の問題も疑っている。
    - 今の ota_0 は書きかけで、ブートローダが拒む。
  - **残り**: USB が止まる原因の切り分け（線、給電、ブートローダの設定、カーネル）、400 MHz の版、ストアと Ethernet、Wi-Fi。中断してデモ（doc/49 §25）を先にした。
- **R1b その 2: USB が消えたのはブートローダの ESP-IDF が古かったから（2026-11-08）**:
  - **症状**: フラッシュから起動すると USB の Serial/JTAG が消え、戻らない。ROM だけ（フラッシュが空）なら消えない。
  - **esptool**（v5.3.1、v3.2 の最小の 5.3.dev3 は満たす）:
    - stub を使うと書き込みが途中で止まった（37.5%、`No more data to read`）。`--no-stub` では 16 MB を 174 秒で書け、ハッシュも合った。
    - RTS のリセット（`--after hard-reset`、DTR を離して RTS だけを振っても）では ROM がダウンロードモードに戻った（`rst:0x17, boot:0x104`）。esptool の PR #1201（Linux で DTR が残る）と同じ形。
    - PR #1200 の強制ダウンロードのビット（0x5011_0008 の bit 2）は 0 だった。
    - `--after watchdog-reset` ならフラッシュから起動する。
    - **stub が止まるのは esptool 5.3.1 が持つ stub（esp-flasher-stub v1.0.0）の不具合**（2026-11-08）。v3.2 では stub の版 2 の `esp32p4.json` が使われる。
      - 止まった間、OpenOCD では CPU が止められなかった（`Unable to halt`、dmstatus は 2 コアとも running）。ウォッチドッグのリセットではなく、バスで待ち続けている形。LP_WDT は esptool が止め、TIMG の WDT は EN が 0（FLASHBOOT_MOD_EN だけ 1）なので、誰もリセットしない。
      - esptool 5.6.0（stub v1.4.1）では、同じカーネル（211 KB）も 2 MB の乱数（ota_1、書いた後に消した）も stub で書け、ハッシュも合った（1.5 Mbit/s）。
      - 間の P4 の直しは、stub v1.1.0 の「CPLL の root mux の選び方」と v1.2.2 の「PMU の latch があれば ROM のフラッシュの XPD を飛ばす」。どちらが効いたかは確かめていない。
  - **切り分け**（どれも `--no-stub` で書き、watchdog-reset で起動）:

| フラッシュ | 結果 |
|---|---|
| 出荷時の全体（ブートローダは ESP-IDF v5.5.2-710） | 起動し、USB も残る |
| 出荷時の上に、うちのブートローダ（v6.0-beta1-624）だけ | USB が消えて戻らない |
| 出荷時の上に、v6.0.2 で作ったブートローダだけ | 起動し、USB も残る（区画表が合わずに再起動を繰り返すが、USB は消えない） |
| v6.0.2 のブートローダ＋うちの区画表＋カーネル（400 MHz） | カーネルが起動（PSRAM 32 MB、memtest 0 件、CPU 399 MHz）、USB も残る |

  - **原因**: `~/esp-idf/6.0` は 2025-11 の release/v6.0（v6.0-beta1-624）で、v3.2 の ROM（eco7、2026-01）より古い。AR2026-004 は v6.0 系の最小を 6.0.1 としている。何が USB を止めたのかまでは見ていない（推測: v3.2 の ROM との食い違い）。
  - **直し**: `make boot-v3` は `IDF_PATH_V3`（既定は `~/esp-idf/6.0.2`）で作る。Tab5（v1.x）の `make boot` は 6.0 のまま。
  - **残り**: ストア（全体を書き戻したので空、`selftest.missing_app`）とアプリ、Ethernet、Wi-Fi。
- **R1b その 3: 最初のプロセスで止まった・リセットの自動化（EVB v1.6.0、2026-11-08）**:
  - **リセットの自動化**: 外付けの USB-UART（nodemcu 形式、CH9102、1a86:55d4 シリアル 5567028046）の DTR/RTS を EN（TP23）と IO35（TP22）に、TX/RX を U0RX/U0TX（J1）につないだ（利用者）。
    - 回路図（v1.6 は公開されておらず、v1.52 の 3 枚目）で確かめた: EN は TP23（10 kΩ で引き上げ、0.1 µF）、IO35 は TP22（10 kΩ で引き上げ、BOOT ボタン）。IO35 は Ethernet の RMII_TXD1 も兼ねる（R135）ので、外から下げるのはリセットの間だけ。
    - esptool の UART の既定のリセットで EN からダウンロードモード、`--after hard-reset` で通常の起動（`rst:0x1 POWERON`）。UART0 に ROM の表示が出る。esptool 5.6.0 の stub で 921600 bps、ストア（4.4 MB）を 25 秒で書けた。
  - **USB-Serial/JTAG のポートを開くだけでダウンロードモードへリセットされる**（Linux の pyserial は開くときに DTR/RTS を立てる）。うちのカーネルの後では、ROM がキャッシュの書き戻し（`Cache_WriteBack_All` 相当、CACHE_SYNC_CTRL 0x3FF1_0098 の SYNC_DONE を待つ、0x4FC1_12BA）で止まり、書き戻しは SYNC_ERR_CODE 2 で終わっていた。USB の受け側も読まれず esptool は `Write timeout`。v6.0.2 のブートローダにした後に見た `Write timeout` と、OpenOCD のリセットの後に USB が戻らなかったのは、これと同じ形。
    - L2 のラインを 64 バイトにしても変わらなかった（戻した）。カーネルの `reboot`（ROM のソフトウェアリセット、`rst` は CORE_SW）なら起動し直せる。原因は未確認。USJ のポートは開かない。
  - **最初のプロセスで止まった**: 自己試験の hello を `proc.start` したところで止まり、MWDT のコアリセット（LP_CLKRST_RESET_CAUSE 0x07）になった。
    - JTAG で見ると、core 0 は hello の入口（mepc 0x4800_0000）で割り込み 21（IPI の線）を受け続け、1 命令も進まなかった。core 1 は wfi で寝ていた。
    - 割り込みマトリクスで、IPI（FROM_CPU_INTR0）のほかに UART0 の割り込みも線 21 に向いていた。カーネルは UART0 に触れない。ROM が UART0 の受信割り込み（INT_ENA=1）を線 5 に向けたまま残し、外付けの USB-UART をつないだので立ちっぱなしになった。
    - 直し: 初期化で、各コアの割り込みマトリクスの全部の行き先を外してから、使うものだけ向ける（v1.x は 128 個、v3.x は 136 個）。
    - あわせて、TIMG の WDT は EN だけでなく FLASHBOOT_MOD_EN も落とし、CONF_UPDATE_EN で反映させる（ESP-IDF の mwdt_ll と同じ）。実機では WDTCONFIG0 が 2 つとも 0 になった。MWDT が動いた理由そのものは確かめていない。
  - **実機（EVB v1.6.0）**: 自己試験が全部通り（`selftest.result failures=0 pass=true`）、RTT の `ps`・`kinfo` に答えた。カーネルの `reboot` の後も起動した。
  - **残り**: USJ からのリセットで ROM が止まる原因（→ その 5）、ストアの残り（Ethernet、Wi-Fi → その 4）。
- **R1b その 4: Ethernet と Wi-Fi（EVB v1.6.0、2026-11-08）**:
  - `fs` と `init` の自動起動の版を RTT で入れた（`install`、v1.5.2 の R1 と同じ）。起動すると `init` は START.CFG なしの既定の組（cfg、wifi、net、netctl、font、term、matter）を起こした。
  - **SD**: `sd.absent`（応答なし）。カードの有無は未確認。そのため `netctl`（`NET.RUL` なし）、`matter`（SD のストアなし）、`term`（画面なし）は見張りがあきらめた（v1.5.2 と同じ形）。
  - **Ethernet**: PHY はアドレス 1（ID 0x0243/0x0C54）、100 Mbit/s 全二重。PC の USB Ethernet（192.168.10.1/24、DHCP なし）につながっていた。`run netctl 10`（eth0 を 192.168.10.254/24 に）の後、PC からの ping は 10 回中 10 回、0.3〜0.6 ms。試験の後は `reboot`。
  - **Wi-Fi**: C6 は SDIO で応え（chip_id 13）、`iotap` に 3 回目でつながった（1 回目 reason 202、2 回目 205、authmode 7）。DHCP で 192.168.2.112/24。`wifi.bt_failed step=init` と `request_failed esp_err=12290` は v1.5.2 の出荷時の C6 と同じ。
  - **SD**（利用者がカードを挿した後）: SK32G（32 GB）が 4 ビット、40 MHz で応え、`fs.ready`、`init` は `T5OS/START.CFG` を読んだ。`netctl` は守りの規則（`rules=6`）、`matter` は SD のストアで起動した。
  - **C6 のファームウェア**: R5 その 8 と同じ像（esp_hosted 2.12.7、1,225,232 バイト）を同じ手順（PC の `http.server`、`netctl 10`、`run c6ota 3232238081 8070`）で OTA した。12 秒で書け、`wifi.ota_end ok=true`。EN で起動し直すと `wlan.fw_version 2.12.7`、`wifi.bt_started`、`iotap` に 2 回目でつながり DHCP。
    - 試験の後の `reboot` を、スクリプトの誤り（trap の中の引数）で飛ばした。気づいてすぐ起動し直した（`netctl 10` の間は数分）。
  - **probe の `--reset` を使わない**: t5log の `--reset` の後、少しして MWDT のコアリセットになり、ROM が USJ のときと同じキャッシュの書き戻しで止まった。EVB v1.6.0 の起動し直しは、外付けの USB-UART の EN か、カーネルの `reboot` で行う。
  - **残り**: コアリセット（USJ、MWDT）の後に ROM が止まる原因（→ その 5 で直した）。
- **R1b その 5: コアリセットの後に ROM が止まった原因（EVB v1.6.0、2026-11-08）**:
  - **症状**（その 3、その 4）: USJ のポートを開く（DTR/RTS のリセット）、esptool の USJ のリセット、probe の `--reset`（MWDT のコアリセット）の後、ROM がキャッシュの同期（CACHE_SYNC_CTRL の SYNC_DONE、0x4FC1_12BA）を待ち続けた。EN のリセットとカーネルの `reboot` では起きない。
  - **見たこと**:
    - ROM のキャッシュの関数の表（0x4FFB_FFD8 → 0x4FC1_FCF0）の +16 が同期の本体（0x4FC1_1284）。止まった所では、その前の書き戻し（MAP 0x30、ADDR 0x4FC0_0000、SIZE 0x40）が SYNC_ERR_CODE 2 で終わり、次の要求（全体の無効化）が受け付けられないまま DONE を待っていた。EN のリセットからのダウンロードモードでは、同じ書き戻しが終わる。
    - キャッシュのレジスタ（0x3FF1_0000〜0x3FF1_03FF）は、同期の結果のほかは止まった時とよい時で同じ。
  - **切り分け**（どれも試しの版、コミットしていない）:
    - L2 のラインを 64 バイト（設定は 0x8 を確かめた）、PSRAM の autoload を止める、idle のたびに L1 D と L2 を書き戻す、PSRAM を使わない: どれも止まった。
    - `kmain` の頭、`Current::init` の後、`pmp::reset` の後で止めて回すと止まらず、`smp::start` の後で止めると止まった。
  - **原因**: core 1 の入口。カーネルは ROM の `ets_set_appcpu_boot_addr` で core 1 の入口を渡し、そのまま残していた。`reset` を通らないリセットの後、core 1 は ROM でその入口を見て、ブートローダがキャッシュとフラッシュを整える前の古いカーネルへ飛び込み、core 0 の ROM のキャッシュの同期が終わらなくなった（推測を含む: core 1 が何をしたかまでは見ていない）。カーネルの `reset` は前から入口を消していたので、`reboot` では起きなかった。
  - **直し**: core 1 が上がったらすぐ（`init_secondary` の頭で）入口を 0 に戻す。
  - **実機（EVB v1.6.0）**: USJ のポートを開く（`reset_cause=23`）、esptool の USJ からのダウンロードモードと watchdog-reset、probe の `--reset`（`reset_cause=24`）の後、どれもカーネルが起動し直した。
- **R1b その 6: 有線の速さ（EVB v1.6.0、2026-11-08）**:
  - R5 と同じ測り方（PC の USB Ethernet 192.168.10.1 と `eth0` 192.168.10.254、`netctl 10`、`nettest 1` の TCP echo、8 MiB）。命令は USJ の `t5log serial` でなく RTT で送る。最後は必ず EN で起動し直す。
  - 片道 27.3〜28.0 Mbps（3 回）。v1.5.2（チップ v1.3）の 28.7〜29.3 Mbps（R5 その 4）とほぼ同じ。
  - `ps` の値: `net` は 1 コアの 680〜689‰、カーネルは全コアの 195〜201‰、取り分 net は 340〜344‰。ただし `ps` の窓は前の `ps` からで、RTT でつなぐ時間も入るので、echo の間の値より薄い（v1.5.2 の 908〜935‰ とは比べられない）。
  - 64 MiB の echo（約 16 秒、`ps` の窓がほぼ echo になる）: 片道 33.9 Mbps（2 回とも）、`net` は 1 コアの 921〜925‰（頭打ち）、カーネルは全コアの 315〜319‰、取り分 net は 460〜462‰。v1.5.2 は 8 MiB でしか測っていなかったので、同じ版（1512d83 の後）のカーネルとストアを v1.5.2 にも書いて測った（`t5log serial`、最後は `reboot`）:

    | | v1.5.2（チップ v1.3、CPU 360 MHz） | v1.6.0（チップ v3.2、400 MHz） |
    |---|---|---|
    | 16 MiB | 32.1〜32.4 Mbps（`net` 917〜943‰） | — |
    | 64 MiB | 32.2〜32.3 Mbps（`net` 970〜972‰） | 33.9 Mbps（`net` 921〜925‰） |

    - どちらも `net` の 1 コアで頭打ち。v1.6.0 は約 5% 速く、`net` の CPU は少し低い。CPU の比（400/360 = 1.11）ほどは伸びない（メモリの待ちなど、推測）。
    - v1.5.2 に書いたストアの像で、自動起動の `fs` と `init` が消えたので入れ直した（起動の直後の 1 回目は `install_begin` が断られた）。
  - **`net` のプロファイル**（v1.5.2、16 MiB の echo、31.5 Mbps、`prof` 1 ms、標本 7,056 個、`tools/prof.py`）:
    - U モード 78.4%、カーネル 21.6%（R5 その 3 は 63% と 37%。カーネルの熱いコードを内蔵 RAM に移した後）。
    - U モード: `memcpy` 15.9%、`Stack::poll` 自身 15.7%、チェックサム 5.8%、`memset` 3.8%、64 ビットの割り算（`u64_div_rem`）1.9%、`process_tcp` 1.9%、`filter::check` 1.4%。
    - カーネル（呼んだ側で数えた）: `read` 7.6%、`Channel::send` 5.0%、`ctl` 4.9%、`wait` 4.1%。
    - `memcpy` と `memset` は `t5_abi` の手書き（8 語ずつ、doc/34 §13）なので、速くするには写しの回数を減らすしかない。フレームは、カーネルのリング → `net` → smoltcp → ソケット → チャネル → `nettest` と写される。
    - `Stack::poll` 自身の時間は、インライン化された smoltcp と core の多くの行に散っていて、目立つ一か所はない（R5 その 3 と同じ）。
  - **Wi-Fi だけ**（R5 その 7 の測り方: PC の `enp5s0` 192.168.2.14 と EVB の `wlan0` 192.168.2.112、`netctl 3` の間だけ、最後は EN で起動し直す。16 MiB、2 回ずつ、C6 は 2.12.7）:

    | | v1.5.2（2.12.7、8 MiB、その 8） | v1.6.0（16 MiB） |
    |---|---|---|
    | PC → EVB | 24.4〜25.1 Mbps | 28.9〜29.5 Mbps |
    | EVB → PC | 26.8〜27.5 Mbps | 28.4〜29.3 Mbps |
    | echo | 13.5〜13.7 Mbps（その 7） | 12.8〜14.3 Mbps |

    - 負荷の下の ping は 2〜16 ms（10 回中 10 回）。`net` は 1 コアの 46〜54%、Core1 は 51〜83% 空き。CPU は上限でない。
    - 大きさ（8 と 16 MiB）と電波の日が違うので、v1.6.0 が速いとは言い切れない。
- **R5 その 10: ルーターを替えたら Tab5 がつなぎ直さなかった**（2026-11-07）:
  - 症状: デモの前（10:05 ごろ）に matter-test のルーターを替えた後、Tab5 がネットワークに戻らなかった。`wifi` は動いていたが、40 秒の間イベントを 1 つも出さなかった。
  - 原因: `wifi` は、C6 から切断のイベントが来たときだけ次の再接続を予約する（1 秒を 2 回、その後 5 秒ごと、回数の上限はない）。ルーターがない間の再接続の要求に C6 が何も返さず、次の予約が入らないまま止まった。
  - 直し: つながっておらず、再接続も JOIN も OTA もないときは、30 秒後にもう一度つなぐ（`SILENT_RETRY_NS`）。
  - 実機（Tab5）: 入れて再起動するとすぐつながり（192.168.8.181）、matter の 4 台も購読を張り直した。AP が消えて戻る場面での 30 秒ごとの再試行は、まだ実機で見ていない。virt・p4emu が通った。

## 6. 推測と未確認
- EMAC が DesignWare GMAC 3.x と同じ形というのは推測。TRM の章で確かめてから書く。
- PHY のアドレス（1）、PSRAM の量、C6 の出荷時のファームウェアは未確認（R0 で読む）。
- チップ v1.3 で今のカーネルがそのまま動くかは未確認。PCN は「ソフトウェアに影響なし」としている。
