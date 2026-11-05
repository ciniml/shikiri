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

## 6. 推測と未確認
- EMAC が DesignWare GMAC 3.x と同じ形というのは推測。TRM の章で確かめてから書く。
- PHY のアドレス（1）、PSRAM の量、C6 の出荷時のファームウェアは未確認（R0 で読む）。
- チップ v1.3 で今のカーネルがそのまま動くかは未確認。PCN は「ソフトウェアに影響なし」としている。
