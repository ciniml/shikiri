# 73. LTE Cat-M のモデムを `net` のリンクに（検討）

利用者の依頼（2026-11-16）: LTE Cat-M のモデムのような「もう 1 つの上流のネットワークインターフェース」を足す構成を調べ、設計の案と段取りを考える。この文書は調査と設計だけ。コード、実機、配線には触れていない。

- 書き方: doc/71 と同じ。**（確認）** は、この文書を書くときに一次資料（このリポジトリのソース、データシート、AT コマンドマニュアル）で確かめたこと。**（推測）** は確かめていないこと。どちらも付けていないものは、このリポジトリの既存の文書に書いてあること。
- 調べたソース: research/initial-survey の d10f1a2（2026-11-16）。

## 1. 結論（先に）
- **推し: 案 A（PPP を UART で）**。M5Stack の Unit CatM（SIMCom SIM7080G）を Grove の UART でつなぎ、新しいアプリ `cell` が AT コマンドで接続し、PPP の IP パケットを `svc:netlink` で `net` の新しいリンク `cell0` に渡す。形 B の `ts0`（doc/72）と同じ形で、`net` から見ると「IP のパケットを運ぶ相手がいるリンク」がもう 1 つ増えるだけ。フィルタ、転送、NAT、ソケット、`ts` がそのまま `cell0` の上で動く。
- **速さを決めるのは UART**。Cat-M の下りは 589 kbps（確認、§3.1）だが、115200 bps の UART では約 11 KB/s（約 90 kbps）で頭打ち。Grove の 4 ピンには RTS/CTS がないので、ボーレートを上げるときは取りこぼしを測りながら（§4.3）。
- **新しく要るもの**: カーネルの UART のオブジェクト（今はない、確認 §2.4）、`cell` アプリ（AT、PPP、後で CMUX）、`net` の `svc:netlink` を複数のリンクに広げること、出口（`Stack::uplink`）の切り替えと Wi-Fi の切断を `net` に知らせること、従量課金のための許可（`svc:net/cell`、案）。
- **案 B（モジュール内蔵の TCP/IP を AT で）** は、1 つのアプリが HTTP や MQTT を少し送るだけなら一番小さいが、`net` を通らないのでガードの規則、転送、`ts` が効かない。汎用の上流にはならない。
- **案 C（USB の ECM など）** は速さとデータの道筋は一番きれいだが、USB ホストのドライバがなく（doc/06 で ★5）、USB-A の 5V は IO エクスパンダ #2 の bit3 で、今の安全の約束（#2 は bit0 だけ）の外。今回は勧めない。
- 決めてほしい点は §8。

## 2. 今の構成（確認）

### 2.1 `net` のリンク
- `net`（`user/apps/net`）は smoltcp を U モードで持ち、リンクごとに smoltcp の `Interface` と `SocketSet` を持つ（`Stack::eth: [Option<EthLink>; IFACES - 1]`、`main.rs`）。リンクは `fwd.rs` の定数で決め打ち: `host`(0)、`wlan0`(1)、`test0`(2)、`eth0`(3)、`ap0`(4)、`ts0`(5)、`IFACES = 6`、`LINK_NAMES`。アプリから見える番号は `t5_abi::net::link`（`WLAN0 = 3`…`TS0 = 7`）。
- ソケットは作るときに宛て先で出口のリンクを選び（`TCP_CONNECT` → `out_iface(a)`）、そのリンクの smoltcp に属す。出口が後で変わっても、開いているソケットは元のリンクに残る。
- 出口: `Stack::uplink` は `UPLINKS = [ETH, WLAN]` のうち DHCP がルーターを教えたもの、なければ `wlan0`（doc/58 R3a）。既定の経路、`RESOLVE` の DNS、下流に配る DNS、DNS の中継の上流がこれに従う。
- Wi-Fi の切断は `net` に届かない。`wifi` は `wifi.disconnected` をログに出すだけで、`net` には別の AP に入り直したときの `LINK_RENEW` だけを送る。`net` は `wlan0` のルーターを DHCP の貸し出しが続く間持ち続ける（推測: smoltcp の dhcpv4 は期限まで設定を消さない）。
- `eth0` は `netdev::LINK_CHANGED` でリンクの上下を受け取る（`net.link_state`）。

### 2.2 Wi-Fi のつながり（前例 1: カーネルの netdev）
- ESP32-C6 は SDIO（slot1）でつながり、esp_hosted の転送はカーネルにある（`kernel/src/wlan.rs`、doc/22）。カーネルは `wlan:ctl`（RPC、`wifi` アプリが使う）、`netdev:wlan0`（Ethernet の枠、`net` が使う）、`bt:hci` の 3 つのオブジェクトに分けて中身を見ずに運ぶ。
- つまり Wi-Fi は「カーネルのドライバ ＋ 制御のアプリ（`wifi`）＋ `net` のリンク」の 3 つ組。

### 2.3 形 B の `ts0`（前例 2: プロセスが運ぶリンク）
- `t5_abi::netlink`: 相手のプロセスが共有メモリ（`netdev::ring` と同じ配置の環 2 本 ＋ 名前の表 4 KiB）を用意して `net` に渡し、チャネルで `KICK`、`ADDR`（`[ADDR, prefix, 0, 0, IPv4]`）、`NAMES` を送る。
- `net` の `link_accept` は **許可 `svc:netlink/ts0` のある 1 つだけ** を受け、決め打ちで `stack.eth[TS0 - 1]` に `Port::Link` を作る。`Port::Link` は smoltcp の Ethernet の枠から IP を取り出して環に入れ、逆は枠を付け、ARP にはどのアドレスにも相手の作り物の MAC で答える。`ADDR` はルーターを持たない（`set_addr(a, prefix, None)`）。
- 相手が去ればリンクは残してアドレスを外す。1 つのプロセスが写せる共有メモリの窓は 2 つ（`Process::shm_mapped`、doc/72 §8）。
- リンクを 1 つ足したとき `Stack` が大きくなり、`net` のスタックを 64→96 KiB にした（doc/72 §8）。

### 2.4 UART とデバイスの許可
- カーネルに ESP32-P4 の UART のドライバはない（確認: `kernel/src/platform/esp32p4v1/` に UART のファイルがなく、`esp32p4v1.rs` には ROM が UART0 の受信割り込みを線 5 に残す件の後始末だけ）。
- デバイスはカーネルがオブジェクトとして出し、アプリは許可で開く（例 `i2c:int/0x68`、doc/69）。`i2c:` で始まる許可は強い許可（システムの鍵だけ）。UART も同じ形にするのが自然（§5.1）。

### 2.5 Tab5 のピンと電源
- Port.A（Grove、HY2.0-4P）: 黒 GND、赤 5V、黄 G53、白 G54（確認、M5Stack の Tab5 の頁）。カーネルは G53/G54 を使っていない（確認: grep）。キーボードは別の Ext.Port の I2C0（GPIO0/1、`kbd.rs`）。
- M5-Bus: 15 番 G7（PC_RX）、16 番 G6（PC_TX）、13/14 番 G38/G37（UART0、ROM のコンソール）、28 番 5V（確認、同じ頁）。
- Grove と M5-Bus の 5V は IO エクスパンダ #1 の bit2（EXT5V_EN）で、カーネルが画面の立ち上げで既に立てている（`tab5.rs` の `0x44 | spk`、`kbd.rs` のコメント、確認）。**モジュールの電源のためにエクスパンダへ新しく書く必要はない**。
- USB-A の VBUS は #2 の bit3（USB5V_EN）。安全の約束では #2 は bit0（WLAN_PWR_EN）しか触らない。
- ESP32-P4 の UART は GPIO マトリクスでどのピンにも出せる（推測: doc/06 の「簡単な周辺」の記述と ESP32 系の通例。P4 の TRM で要確認）。Port.A の黄・白を UART1 の RX・TX にすれば、Grove の Unit をそのまま挿せる（Port.A の外部 I2C は使えなくなる）。

## 3. 想定するモジュール

### 3.1 M5Stack Unit CatM（SIM7080G）
- SIM7080G、Cat-M と NB-IoT、UART 115200 8N1 の AT コマンド、Grove は **PORT.C** 向け（GND、5V、UART_RX、UART_TX）、nano SIM、SMA のアンテナ（確認、M5Stack docs）。待機 49.4 mA、接続時のピーク 249 mA（確認、同）。
- 速さ: Cat-M 上り 1119 kbps、下り 589 kbps。NB-IoT は 150 / 136 kbps 程度（確認、M5Stack docs と SIMCom の頁）。
- SIM7080G の電力（モジュール単体）: PSM 3.2 µA、sleep 0.6 mA、idle 10 mA（確認、SIMCom の頁）。Unit の 49.4 mA は基板の電源回路を含む値と見られる（推測）。
- 認証: JATE、Telec、docomo、KDDI、SoftBank を含む（確認、M5Stack の SKU U128 の頁）。日本の LTE-M の SIM で使える見込み（推測。SIM と APN は利用者のもの）。
- Unit には PWRKEY、DTR、RI、USB が出ていない（推測: M5Stack の頁に記載なし。回路図で要確認）。DTR で眠らせる（`AT+CSCLK`）形や USB は使えない前提で考える。
- Tab5 には PORT.C の Grove がない。挿し方は 2 つ（§8 の問い 2）:
  - Port.A に挿し、G53/G54 を UART1 にする（ケーブル 1 本。TX/RX の向きは要確認）。
  - M5-Bus の G7/G6（PC_RX/PC_TX）と 5V、GND に配線する（Port.A は空く。配線か Bus の基板が要る）。
- 同じ SIM7080G の仲間: Unit CatM+GNSS（U137）、IoT Base CatM（M119）。AT は同じ（推測）。

### 3.2 SIM7080G の AT コマンド（SIM7080 Series AT Command Manual V1.02 で確認）
| 用途 | コマンド | メモ |
|---|---|---|
| ボーレート | `AT+IPR`（§2.2.32） | 115200〜921600、さらに 2M〜4M bps まで挙がっている |
| 流れ制御 | `AT+IFC`（§2.2.31） | RTS/CTS と XON/XOFF。Grove には RTS/CTS の線がない。PPP は XON/XOFF と相性が悪い（バイナリ）ので、流れ制御なしで使うことになる（推測） |
| 網 | `AT+CNMP`（§5.2.16）、`AT+CMNB`（§5.2.17）、`AT+CGDCONT`（§6.2.2）、`AT+CGACT`（§6.2.3） | Cat-M 固定、APN の設定 |
| データモード | `ATD`（§2.2.2）、`ATO`（§2.2.9）、`+++`（§2.2.8「Data Mode or PPP Online Mode」から戻る） | PPP の「online mode」がマニュアルにある。`*99#` そのものの記載は見つけていない（3GPP の通例。推測） |
| 多重化 | `AT+CMUX`（§3.2.24） | basic option だけ、UIH だけ、N1（枠の大きさ）1〜1500、既定 118 |
| 省電力 | `AT+CPSMS`（§5.2.18）、`AT+CEDRXS`（§5.2.42）、`AT+CSCLK`（§5.2.7） | |
| 内蔵の IP | `AT+CNACT`（§7.2.1）、`AT+CACID`（§12.2.1、cid 0〜12）、`AT+CAOPEN`（§12.2.3、TCP/UDP）、`AT+CASERVER`（§12.2.4）、`AT+CSSLCFG`（§11.2.1、TLS） | 案 B |

- Zephyr の汎用のモデムドライバで SIM7080G（M5Stack のもの）を PPP と CMUX で動かした例がある（確認、Golioth の記事。UART 115200、CMUX の枠 4096）。PPP が実際に使えることの裏付けになる。

### 3.3 ほかの候補
- Quectel BG95/BG96: UART と USB 2.0。USB では PPP と ECM が使え、QMI は使えない、と Quectel のフォーラムで社員が答えている（確認、ただしフォーラムで公式文書ではない）。UART の AT と PPP は SIM7080G と同じ形で扱える見込み（推測）。M5Stack の Unit にはなく、基板を用意する必要がある。
- どのモジュールでも、`cell` の AT の部分（初期化の列）だけがモジュールごとに変わり、PPP から先は同じにできる（推測）。

## 4. IP の載せ方

### 4.1 案 A: PPP（推し）
```
 アプリ ──svc:net──▶ net（smoltcp、フィルタ、転送、NAT）
                       ├─ wlan0（netdev:wlan0）
                       ├─ ts0 ◀──環──▶ ts
                       └─ cell0 ◀──環──▶ cell（AT、PPP、CMUX）──uart:… ──▶ カーネルの UART1 ──▶ SIM7080G
```
- `cell` が AT で網に入り（`AT+CGDCONT` で APN、`AT+CEREG?` で登録を待つ）、`ATD*99#`（または `AT+CGDATA`）でデータモードにし、PPP（LCP → 認証（要るなら PAP/CHAP）→ IPCP）を張る。IPCP でアドレスと DNS（RFC 1877 の 129/131）を受け、`net` に知らせる。
- PPP の中身は IP パケットそのものなので、`ts0` と同じ `Port::Link`（IP ⇔ 作り物の Ethernet の枠、ARP は `net` が答える）がそのまま使える。違うのは「既定の経路になりうる」「DNS を持つ」の 2 点（§5.2）。
- PPP の実装:
  - `ppproto`（embassy-rs、no_std、alloc なし、MIT/Apache-2.0、0.2.1 が 2024-11-14）（確認、crates.io）。README は RFC 1661（LCP）、1332（IPCP）、1334（PAP/CHAP）を挙げ、試験は pppd を相手にしている（確認）。何が実装済みか（CHAP、DNS の選択肢、IPv6CP）は README に書いてない（確認）ので、ソースを読んで決める。
  - 自前: HDLC 風の枠（0x7E、0x7D の逃がし、FCS-16）、LCP と IPCP の最小の交渉（MRU、ACCM、magic、アドレス、DNS）で、数百行の見込み（推測）。tsclient と同じく sans-IO の核にしてホストで pppd と試せる。
- CMUX: PPP の間も AT（電波の強さ `AT+CSQ`、登録の状態、切れたときの立て直し）を使うには要る。CMUX なしでは、PPP の間は `+++` で抜けるか、何も聞けない。最初の段は CMUX なしで始め、後の段で足す（§7）。
- MTU: PPP の MRU は LCP で決まる（既定 1500）。携帯網の中の MTU はキャリアで 1500 より小さいことがある（推測）。`cell0` は 1500 から始め、TCP の MSS を測って必要なら下げる。`netlink` の 1 枠は 1536 バイトで足りる（`SLOT`）。
- 速さ: UART の bps ÷ 10 が上限（115200 で 11.5 KB/s）。HDLC の逃がしで数 % 減る（推測）。CMUX を挟むと枠の分さらに減る。921600 にすれば Cat-M の下り（約 74 KB/s）を上回る。
- 良い点: `net` の全部（ガードの規則、`masquerade on cell0`、ソケット、`ts`、DNS の中継）が効く。モジュールを替えても AT の部分だけ。
- 悪い点: PPP（と CMUX）を書く。UART のドライバを書く。流れ制御がないので高いボーレートで取りこぼしうる。

### 4.2 案 B: モジュール内蔵の TCP/IP（AT+CAOPEN など）
- `AT+CNACT` で網に入り、`AT+CAOPEN` で TCP/UDP を最大 13 本（cid 0〜12）、`AT+CASERVER` で待ち受け、`AT+CSSLCFG` で TLS もモジュールの中でできる（確認、AT マニュアル）。
- `net` への組み込み方は 2 通りあるが、どちらも筋が悪い:
  - B-1: `cell` が `svc:cell`（`svc:net` と同じ要求）を出し、アプリが選ぶ。形 A の `svc:ts`（doc/72 §1）と同じ困りごと（アプリが網を使い分ける、フィルタが効かない、転送できない）がそのまま出る。
  - B-2: `net` の中で、出口が `cell0` のソケットの要求を `cell` に回す（代理）。`net` に「smoltcp を通らないソケット」の種類を足すことになり、`TCP_STATE`、`SEND`、`RECV`、`UDP_WAIT` を全部二重に持つ。大きい。
- ガードの規則も NAT も効かず、`ts` の WireGuard（UDP）や DERP（TCP 443 の上の TLS は `ts` が自分で持つ）を通すには B-2 が要る。ping も別のコマンド。
- 速さ: `AT+CASEND` の長さ付きの送信と `AT+CARECV` の取り出しで、AT の往復が毎回挟まる（推測: PPP より遅い）。
- 使いどころ: 「LTE で 1 つのアプリが MQTT を少し送るだけ」なら最小。汎用の上流としてはとらない。

### 4.3 案 C: USB（ECM、RNDIS、QMI）
- SIM7080G には USB がある（確認、SIMCom の頁。中身は FW 更新が主に挙がっている）。ECM などのネットワークの機能があるかは確認できなかった。BG95 は ECM が使える（§3.3）。
- ECM なら Ethernet の枠なので、カーネルに `netdev:usb0` を作れば `wlan0` と同じ形で `net` に入る（前例 1）。
- ただし:
  - Tab5 の USB-A は OTG HS（DWC2）のホストで、ドライバもホストスタックもない（doc/06 §4 で ★5）。
  - USB-A の 5V は #2 の bit3。**今の安全の約束の外**なので、利用者の許可が要る。
  - Unit CatM は USB を出していない（推測）。別のモジュールと基板が要る。
- 速さは UART の頭打ちがなく Cat-M の速さそのまま。実装量は 3 案で一番大きい。

### 4.4 比べ
| | A: PPP（UART） | B: 内蔵の TCP/IP | C: USB ECM |
|---|---|---|---|
| `net` への入り方 | `svc:netlink` の `cell0`（形 B と同じ） | `net` の外（`svc:cell`）か代理 | カーネルの `netdev:usb0`（Wi-Fi と同じ） |
| フィルタ、NAT、転送、`ts` | 効く | 効かない | 効く |
| MTU | LCP の MRU（1500 から） | モジュール任せ | 1500（推測） |
| 速さの上限 | UART（115200 で約 90 kbps、921600 で Cat-M 並み） | UART と AT の往復 | Cat-M の速さ（上り約 1.1 Mbps） |
| 消費電力・PSM/eDRX | モジュールの設定で使える。ただし DTR がなく、`ts` などの keepalive があると眠れない（§6.4） | 同じ | 同じ ＋ USB の分 |
| 新しく書くもの | UART のドライバ、`cell`（AT、PPP、後で CMUX）、`net` の一般化 | UART のドライバ、AT のソケット、`svc:cell` か `net` の代理 | DWC2 のホスト、CDC-ECM、`netdev:usb0` |
| 実装量（推測） | 中 | 小（1 アプリ用）〜大（代理） | 大 |
| 安全の約束 | 触るのは UART1 と 2 本の GPIO だけ | 同じ | #2 の bit3 に許可が要る |

## 5. 案 A の作り

### 5.1 カーネル: UART のオブジェクト
- `uart:1`（ESP32-P4 の UART1。ピンはボードの定義で Port.A の G53/G54 か M5-Bus の G7/G6）。許可は `uart:` で始まる強い許可（システムの鍵だけ、`i2c:` と同じ扱い）。
- `read`: 受けたバイト（割り込みで内部の環、例えば 4 KiB に貯める）。`write`: 送るバイト（TX FIFO が空くたびに割り込みで詰める）。`ctl`: ボーレート、取りこぼしの数。`wait` で受信を待てる。
- FIFO は 128 バイト程度（推測、ESP32 系の通例）。921600 bps なら約 1.4 ms で埋まるので、受信の割り込みのしきい値とタイムアウト（RX_TOUT）を使う。足りなければ UHCI の DMA（推測）。
- UART0（G37/G38、M5-Bus）は ROM のコンソールとダウンロードに使うので使わない。
- エミュレータ（t5emu）にも UART1 の型が要る。相手は、ホストの pty につないで AT に答える小さな偽モデムと、その後に pppd を立てる形にすると、p4emu で端から端まで試せる（推測）。

### 5.2 `net`: `svc:netlink` を複数のリンクに
- 今の `link_accept` は `ts0` 決め打ち。許可の名前でリンクを選ぶ形に広げる: `svc:netlink/ts0` → `TS0`、`svc:netlink/cell0` → `CELL0`。`fwd.rs` に `CELL0 = 6`、`IFACES = 7`、`LINK_NAMES` に `cell0`、`t5_abi::net::link::CELL0 = 8`。ガードの規則はリンクの名前で書き、名前は `filter.rs` が `LINK_NAMES` から引く（確認）ので、`cell0` は名前の表に足せばそのまま書ける。
- `netlink` の通知を足す（ABI の小版を上げる）:
  - `ADDR` にルーター（相手のアドレス。PPP では IPCP の相手か、`Port::Link` が答える作り物でよい）を足す。ルーターのあるリンクは既定の経路の候補になる。
  - `DNS`（IPv4 を 2 つまで）。
  - `UP`/`DOWN`（PPP が切れた、張り直した）。
- `cell0` の `NAMES`（名前の表）は使わない。
- 共有メモリの窓: `cell` は `uart:1`（共有メモリなし）と環の 1 窓。`svc:net` を使っても 2 窓で収まる。
- リンクが増えるので `net` のスタックをまた確かめる（doc/72 §8 では黙ってあふれた）。p4emu の全部を `cell` のない構成で通す。

### 5.3 出口の選び方（フェイルオーバー）
- `UPLINKS` を `[ETH, WLAN, CELL0]` にし、「上がっていて、ルーターがあるもの」のうち先のものを出口にする。順番は設定（例 `net.uplinks eth0,wlan0,cell0`）で変えられるようにする（案）。
- **Wi-Fi が切れたことを `net` が知る必要がある**（§2.1）。案: `wifi` が切断と接続で `svc:net` の新しい要求（`LINK_STATE`、許可 `link`、今の `LINK_RENEW` と同じ扱い）を送り、`net` は `wlan0` を「下がった」として出口から外す。加えて、出口のルーターへの ARP が答えなくなったら下がったとみなす（推測: 補助）。
- 出口が変わったとき:
  - 既定の経路、`RESOLVE` の DNS、DNS の中継の上流は今の仕組み（R3a）で付いてくる。DNS の中継は作り直される。
  - **開いているソケットは元のリンクに残る**（§2.1）。Wi-Fi が切れたなら、そのソケットは相手に届かなくなり、アプリのタイムアウトで閉じる。`ts` は TLS の読み出しの 15 秒（`READ_STALL_MS`）、UDP の作り直し（`ts.udp_rebound`）で張り直すので、そのまま追従する見込み（推測）。
  - 出口の切り替えを `net.uplink from=… to=…` のイベントで出す。アプリが早く張り直せるよう、出口が変わったら `UDP_WAIT` などで知らせる形も考えられる（後で）。
- Wi-Fi が戻ったら Wi-Fi に戻す（順番が上なので）。行ったり来たりを避けるため、上がってから数十秒待って戻す（案）。

### 5.4 ガードの規則
- `cell0` の既定は「外から始まるものは何も受けない」: `pass in on cell0 state established,related` のみ（今の `wlan0` の守りの規則と同じ考え、doc/24）。携帯網は CGNAT で外から届かないことが多いが、届くキャリアもある（推測）ので規則で閉じる。
- `ap0` や `ts0`（subnet router）の転送を LTE に出すかは持ち主が決める: `pass forward on ap0 out cell0` と `masquerade on cell0`。既定は出さない（従量課金のため）。

### 5.5 従量課金の制御
- 許可で絞る（案）: `svc:net/cell`（または `svc:net/metered`）を持つアプリだけが、出口が `cell0` になるソケットを開ける。持たないアプリの `TCP_CONNECT`、`UDP_SEND`、`RESOLVE` は、出口が `cell0` のとき `UNREACHABLE`（`net.denied op=cell`）。`refused` に 1 つ条件を足すだけで、ABI は変えない。
  - どのアプリに許すかは像の署名した許可（doc/52）なので、利用者が決めて署名する。最初は `ts`、`ota` の確認だけ、など。
- `net` がリンクごとの送受のバイト数を数え（`net.stats` に足す）、設定 `cell.quota_mb`（月の上限、案）を超えたら `cell0` を出口から外す。日付が要るので、時刻の元（doc/71 §4.7 の時刻の扱い）に頼る。
- `ota` の大きな取得は、`cell0` では既定で断る（案）。

### 5.6 `ts` を LTE 越しに
- 形 B の `ts` は `svc:net` で TCP 443（DERP、制御）と UDP 41641（disco、WireGuard）を開くので、出口が `cell0` なら何も変えずに LTE を通る（推測。ソケットは宛て先で出口を選ぶ）。
- 携帯網はほぼ CGNAT で、直の経路（UDP の穴あけ）は通りにくく、DERP 経由になる見込み（推測）。DERP は TLS の上なので PPP の MTU の影響は受けにくい。
- データ量: DERP の keepalive、制御の map の long-poll、disco の ping、WireGuard の keepalive が常に流れる（doc/72 §12 では 15 秒ごとのハンドシェイクを調べた）。月に数十 MB の桁になりうる（推測。C3 で測る）。`ts` に「出口が `cell0` のときは disco と STUN を控え、keepalive を延ばす」形（`IF_INFO` で出口を見る）を足すとよい（案）。
- `ts` を LTE で動かすかは利用者が決める（§8）。動かすなら `ts` に `svc:net/cell` を許す。

### 5.7 設定と秘密
- `cell.apn`、`cell.user`、秘密の `cell.pass`（`wifi.psk` と同じく `svc:cfgsec`、doc/50）、`cell.baud`、`cell.mode`（catm / nbiot）。
- IMEI、IMSI、ICCID、電話番号は個人と結びつくので、ログ（`event!`）にも文書にも出さない。出すなら末尾数桁だけ。

## 6. 危ないところ
1. **UART の取りこぼし**: 流れ制御なしで 921600 bps。PPP は FCS で壊れた枠を捨てるので壊れはしないが、TCP の再送で遅くなる。数を `ctl` で取り、ボーレートを決める。
2. **5V の電流**: Unit のピーク 249 mA（確認）。Tab5 の EXT5V の上限は公開の頁に書いてない（確認）。キーボードと同じ 5V に載る。モジュールの送信の瞬間の電流で 5V が落ちないか（推測。実機で INA226 と `wifi`/画面の挙動を見る）。
3. **`net` を触る**: `net` が落ちると全部が止まる（doc/72 §6）。`cell` がいない構成で今と同じに動くことを毎段で確かめる。
4. **PSM と常時接続**: PSM の間はモジュールが網から見えず、PPP の上の接続は止まる。`ts` などの keepalive があると PSM に入れない。DTR がないので UART を眠らせる手段もない（推測）。省電力は「LTE を使わないときは `AT+CFUN=0` か PSM」の粗い形からになる。
5. **Port.A の取り合い**: Port.A を UART にすると、同じポートの I2C の Unit は使えない。kumiki が Port.A を使っているかは未確認。

## 7. 段取り（案）
| 段 | 中身 | 確かめ |
|---|---|---|
| **C0** UART と AT | カーネルの `uart:1`（割り込みの受信の環、送信、ボーレート）。`cell` は AT を送り、`cell.modem`（型番、FW の版）、`cell.reg`（登録の状態、電波の強さ）を出す。網には入るがデータはまだ | t5emu: 偽モデムに AT が往復。実機: Cat-M で登録（`+CEREG: 0,1` か `0,5`） |
| **C1** `net` の一般化 | `svc:netlink` を許可の名前でリンクに。`cell0`（`CELL0 = 6`）、`ADDR` のルーター、`DNS`、`UP`/`DOWN`。`UPLINKS` に `cell0`。`nettest` が `cell0` の相手を名乗る | p4emu: `nettest` の `cell0` が出口になり、既定の経路と DNS が移る。`cell0` なしで p4emu の全部 |
| **C2** PPP | `cell` に PPP（`ppproto` か自前、§8）。`ATD*99#`、IPCP のアドレスと DNS を `net` へ。CMUX なし | ホスト: pppd を相手に PPP の核。実機: Wi-Fi を切って（`cfg` で）`ping 1.1.1.1` と `RESOLVE` が `cell0` を通る |
| **C3** 切り替えと課金 | `wifi` → `net` の `LINK_STATE`、出口の切り替えと戻し、`svc:net/cell` の許可、リンクごとのバイト数、`cell.quota_mb`、`cell0` の守りの規則 | 実機: AP を落として `cell0` へ、戻して `wlan0` へ。許可のないアプリが `cell0` で断られる。`ts` が LTE で DERP 経由でつながり、1 時間のデータ量を測る |
| **C4** CMUX と速さ | `AT+CMUX`、AT と PPP を並べる（PPP の間も電波と登録を見て、切れたら張り直す）。ボーレートを上げて取りこぼしと速さを測る | 実機: `tsget` などで取得の速さ。`uart` の取りこぼしの数 |
| **C5** 省電力（必要なら） | 使わない間の `AT+CFUN=0`、PSM/eDRX（`AT+CPSMS`、`AT+CEDRXS`）、`cell.idle` の設定 | 実機: INA226 で電流 |
| **C6** USB ECM（必要なら） | DWC2 のホスト、CDC-ECM、`netdev:usb0`。#2 の bit3 の許可が要る | — |

- C0 と C1 は互いに関係なく進められる。C2 は両方の後。

## 8. 決めてほしい点
1. **モジュール**: Unit CatM（SIM7080G）でよいか。手元にあるか。使う SIM（キャリア）と、Cat-M か NB-IoT か。
2. **挿し方**: Port.A に挿して G53/G54 を UART1 にする（Port.A の I2C を諦める）か、M5-Bus の G7/G6 に配線するか。
3. **新しいハードに触る許可**: UART1 と、その 2 本の GPIO の設定。モジュールの電源の EXT5V は既に立っているので、エクスパンダへの書き込みは要らない。USB の案（#2 の bit3）は今回は求めない。
4. **載せ方**: 案 A（PPP）でよいか。PPP は `ppproto` を使うか、自前で書くか。
5. **LTE の使い方**: Wi-Fi がないときだけの予備か、ほかの用途（LTE を常用、Tab5 を LTE のルーターにする）もあるか。出口の順番の既定（eth0 > wlan0 > cell0 でよいか）。
6. **従量課金**: `svc:net/cell` の許可で絞る形でよいか。最初にどのアプリに許すか（`ts`、`ota`、`matter` など）。月の上限を設けるか。
7. **`ts` を LTE で動かすか**（データ量、§5.6）。
8. **IPv6**: 最初は IPv4 だけ（IPCP だけ）でよいか。

## 9. 調べたもの
- このリポジトリ（d10f1a2）: `user/apps/net/src/main.rs`（`link_accept`、`link_messages`、`TCP_CONNECT`、`refused`、`net.link_state`）、`fwd.rs`（`IFACES`、`LINK_NAMES`、`UPLINKS`、`uplink`、`route`）、`link.rs`、`abi/src/lib.rs`（`netlink`）、`abi/src/net.rs`（要求の表、`link`）、`user/apps/wifi/src/main.rs`（切断のイベント）、`kernel/src/wlan.rs`、`kernel/src/platform/esp32p4v1/{tab5.rs,kbd.rs}`、`kernel/src/platform/esp32p4v1.rs`、doc/06、doc/22、doc/24、doc/52、doc/58、doc/69、doc/71、doc/72。
- M5Stack Unit CatM（U128）: https://docs.m5stack.com/en/unit/Unit_CatM 、https://docs.m5stack.com/ja/unit/Unit_CatM 、https://docs.m5stack.com/en/products/sku/U128
- M5Stack Tab5（Port.A、M5-Bus、EXT_5V_BUS、USB5V_EN）: https://docs.m5stack.com/en/core/Tab5
- SIMCom SIM7080G（インターフェース、速さ、電力、プロトコル）: https://www.simcom.com/product/SIM7080G.html
- SIM7080 Series AT Command Manual V1.02（2020-02-26）: https://files.waveshare.com/upload/3/39/SIM7080_Series_AT_Command_Manual_V1.02.pdf （§2.2.2、2.2.8、2.2.9、2.2.31、2.2.32、3.2.24、5.2.7、5.2.16〜18、5.2.42、6.2.2〜3、7.2.1、11.2.1、12.2.1〜4。pdftotext で読んだ）。新しい V1.05 は SIMCom の頁にあるが読んでいない。
- Golioth、SIM7080G を Zephyr の PPP と CMUX で: https://blog.golioth.io/bringing-the-sim7080g-modem-module-to-golioth/
- ppproto: https://crates.io/crates/ppproto 、https://github.com/embassy-rs/ppproto
- Quectel BG95 の USB（PPP と ECM、QMI なし。フォーラムの回答）: https://forums.quectel.com/t/bg95-m3-in-embedded-linux-environment/51581 、https://forums.quectel.com/t/bg95-m3-linux-qmi-driver/5641
