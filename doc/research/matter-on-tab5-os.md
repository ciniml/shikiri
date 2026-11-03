# tab5-os で simple-matter を使う方針（Matter のコントローラとデバイス）

> 2026-11-04 の調査の記録（調べた時点の状態のまま）。その後の変更: プロセスの上限は 12 に、スロットは 2 MiB になった（doc/30 §4）。`ble` は M2〜M4 ができた（doc/48）。進み具合は doc/49。

調べた日: 2026-11-04。対象: `~/repos/simple_matter`（a163275）、`~/repos/tab5-os`（8d6cc50。作業ツリーには、まだコミットしていない `user/apps/ble/` と `user/Cargo.toml` の変更がある）。両方のリポジトリとも変更していない。ビルドの出力はスクラッチパッドの下にだけ置いた。

「未確認」と書いたところは、コードや文書を読んで推測しただけで、動かして確かめていない。

---

## 0. 結論

- **`matter` サービスを 1 つ作る**（U モードのプロセス、`listen:matter`）。デバイスとコントローラの両方の役をこの 1 つのプロセスに入れる。アプリとシェルは `svc:matter` を通して使う。
  - 理由 1: mDNS の UDP 5353 は、1 つのプロセスしか結びつけられない（`net` の `bind` は、同じポートの 2 つ目を `ADDR_IN_USE` で断る。user/apps/net/src/main.rs `in_use`）。デバイスの応答（responder）とコントローラのブラウズは、同じソケットで受けた mDNS を振り分ける必要がある。
  - 理由 2: fabric、CA、ノード帳、セッションを持つのは 1 か所にしたい。その持ち主が SD のファイルも 1 人で書く。
  - 理由 3: simple-matter のコアは sans-IO で同期型（`handle_rx`、`poll`、`next_deadline` で動く）。tab5-os の `wait`/`poll` のイベントループにそのまま載る。async は要らない。BLE だけは `ble` サービス（async）の向こう側に置く。
- **順序**: on-network（Wi-Fi の UDP と mDNS）を先にする。BLE でのコミッショニングは、doc/48 の M3（セントラル）と M4（ペリフェラル）の後にする。
- **実績**: この組み合わせは、ESP-IDF の上ではもう動いている。simple-matter の `ports/esp-idf/examples/tab5_ctrl_app` は、同じ Tab5（P4 と、esp_hosted 経由の C6）の上で、`riscv32imafc-unknown-none-elf` 向けにビルドした simple-matter（C FFI の staticlib）をコントローラとして動かした。Wi-Fi の on-network でのコミッショニングは約 5〜7 秒、BLE の ble-wifi も実機で通っている（docs/design/p4-thread-controller.md §8〜§12、§10.6、§11.5）。プロトコルの部分については危険が小さい。新しく作るのは、OS のサービスとの間をつなぐ部分になる。
- **コンパイルの確認**: コア（`rustcrypto,controller,ble`）は、tab5-os のツールチェーン（nightly-2026-09-10、`-Zbuild-std=core`、`relocation-model=pie`、riscv32imafc）で `--offline` でもエラーなくビルドできた（§9）。

---

## 1. 両者の形

### 1.1 simple-matter
- コアは `crates/simple-matter`。`#![no_std]`、sans-IO で、alloc は使わない（`alloc` feature は既定では無効）。features は `rustcrypto`（既定）、`controller`、`ble`、`factory-data`（crates/simple-matter/Cargo.toml）。
- 外から動かす API:
  - デバイス: `stack::MatterStack`
    - `handle_rx(datagram, PeerAddr, now_ms, tx_out) -> Option<SendDirective>`
    - `poll(now_ms, tx_out)`
    - `next_deadline(now_ms)`
  - mDNS: `discovery::MdnsResponder`（`handle_query`、`poll_announce`、`set_commissionable`、`add_operational`）と `discovery::client::MdnsClient`（ブラウズや解決のパケットを作り、答えを解析する）。どちらもパケットのバイト列だけを扱い、ソケットは呼び出し側が持つ。
  - コントローラ: `controller::ControllerStack`（`start_pase`、`start_case`、`start_read`、`start_write`、`start_invoke`、`start_subscribe`、`handle_rx`、`poll`）、`controller::commissioner::Commissioner`（`drive` で段階を進める。`set_wifi_credentials`）、`controller::ca::Ca`（`generate`、`restore`、`issue_noc`）、`controller::nodes`（ノード帳）。
  - BTP: `btp::Btp`（`process_incoming`、`process_outgoing`、`send`、`recv`、`next_deadline`）は sans-IO。`btp::gatt::GattPeripheral` と `GattCentral` は async の trait（PC の bluer/btleplug と ESP の TrouBLE がこれを実装している）。
- ポート（OS とのつなぎ方）:
  - ports/esp32: esp-hal、esp-radio、TrouBLE 0.6 のペリフェラル。KVS は flash。
  - ports/esp32s3: TrouBLE のセントラルを使うスタンドアロンのハブ。
  - ports/esp-idf: `crates/simple-matter-cffi`。`sm_udp_rx`、`sm_poll`、`sm_mdns_rx`、`sm_mdns_poll`、`sm_set_addrs`、`sm_ble_*`、`sm_ctrl_*`、KVS、RNG、DAC 署名のコールバック。合成モード（compose.rs）とカスタムクラスタ（custom.rs）もある。
- 持っているもの: 32 種のサーバクラスタ、17 種のデバイスタイプ、PASE/CASE（resumption を含む）、Subscribe、ACL、groupcast の受信、attestation の検証、OCW（README.md）。

### 1.2 tab5-os
- アプリは U モード、`no_std` で、`build-std=core` を使う。`alloc` も global allocator もない（`user/` に `global_allocator` は一つもない）。
  - スロットは 3 MiB（PSRAM 24 MiB ÷ `MAX_PROCS` 8。doc/30 §4）。スタックは `elf2t5a --stack`（例: `net` は 64 KiB）。
  - 1 プロセスのハンドルは 16 まで（`abi::MAX_HANDLES`）。
- イベントループは `wait(&[handles], timeout)` で回す。タイマ（`Timer`）、時刻（`time_ns`）、乱数（`random_fill`。doc/47 の SAR ADC のエントロピー源とヘルステスト付き）がある。
- `svc:net`（user/rt/src/lib.rs の `Net`、abi/src/net.rs）:
  - UDP の BIND、SEND、RECV が使える。IPv4 と IPv6 の両方で、送り元の `IpAddr` も返る。
  - 要求 1 つに返事 1 つ。データは 64 KiB の共有メモリの切れ端を通る。サービスの中では待たず、クライアントが 1 ms ごとにやり直す。
  - **マルチキャストのグループに入る操作はない**。**受けたリンクと宛先アドレスも返さない**。
  - クライアントは 4 人まで、ソケットは 8 個まで、UDP の受信は 16 パケット・8 KiB（user/apps/net/src/main.rs:48-56）。
- `svc:fs`（abi/src/fs.rs）: FAT（embedded-sdmmc）。作れる名前は 8.3 だけ。OPEN は CREATE（中身を空にする）か APPEND。READ、WRITE、SEEK、DELETE、MKDIR がある。**rename はない**。
- `bt:hci`（doc/48 M1）はできている。`ble` サービス（TrouBLE、async は中だけ）は M2 で、いま作業中（コミットしていない `user/apps/ble/`。trouble-host =0.8.0 の features は `central`、`scan`、`gatt`）。
- `shell`（doc/39）: 組み込みのコマンドと `run APP [A0 [A1]]`。`proc:spawn` が渡せるのは**整数 2 つだけ**。

---

## 2. プラットフォームが用意するものと、今あるもの

| 要件 | simple-matter の受け口 | tab5-os の今 | 足すもの |
|---|---|---|---|
| UDP（5540、IPv4 と IPv6） | `PeerAddr::Udp(SocketAddr)` に datagram を渡し、`SendDirective` を送る | `udp_bind_at(0, link::WLAN0, 5540)` で、1 つのソケットが両方のファミリを受ける（smoltcp の束縛でアドレスを指定しないとき）。`udp_recv_ip` と `udp_send_ip` | なし（MT1 はこれで動く）。待ち方は下の「受信の通知」 |
| リンクローカル（fe80） | `SocketAddr::V6` のスコープは使わない（ポートの中で解決する） | `route6` は、リンクローカルを Wi-Fi に出す（Wi-Fi があるとき）。受けたリンクは返さない | Wi-Fi 1 本なら困らない。ホストリンクや試験用 LAN でも使うなら、UDP_RECV にリンクの番号を足す |
| マルチキャストの受信（ff02::fb、224.0.0.251） | mDNS のパケットを `MdnsResponder::handle_query` と `CommissionableSet::ingest` に渡す | smoltcp の `multicast` feature は有効だが、**グループに入っていない**。join の操作もない | `net` に `UDP_JOIN`（`Interface::join_multicast_group`。MLD と IGMP の報告を出す）を足す。C6（esp_hosted）が ff02::fb あてのフレームを上げてくるかは**未確認**（ff02::1 と要請ノードのマルチキャストが届いているので、たぶん届く） |
| 宛先がマルチキャストかユニキャストか | QU の応答の判定は、パケットの中身（`query_wants_unicast`）でする | 返さない | なくてもよい。あれば mDNS の応答の宛先を正しく選べる（UDP_RECV に `local_address` を足す。smoltcp の `UdpMetadata` が持っている） |
| 送り元アドレスの選び方 | 運用アドレスは mDNS の AAAA に出す | doc/46 の RFC 6724 の規則 7 で、**一時アドレスを選ぶことがある** | Matter のソケットは、安定したアドレス（IID がリンクローカルと同じもの、main.rs:274）か、要求が来た宛先を送り元にする。そうしないと、応答の送り元がずれる（相手の実装によっては捨てる。**未確認**） |
| 受信の通知 | `next_deadline` までに次の `handle_rx` や `poll` があればよい | 通知はない。1 ms ごとの問い合わせ（1 回ごとに IPC の往復） | MT1 は、`timer` を 10〜20 ms にして UDP_RECV を空になるまで呼ぶ（MRP の初期値は数百 ms なので足りる）。後で、`net` が読めることを知らせる仕組み（チャネルに通知を送る `UDP_NOTIFY`、またはソケットごとの `wait` できるハンドル）を検討する |
| フィルタ | なし | GUARD_SET は `policy in drop`。Wi-Fi から入れるのは established だけ（user/apps/netctl/src/main.rs:45） | `pass in on wlan0 proto udp port 5540`、`... port 5353`、コントローラが使うポート（購読の報告が来るのは長い間隔の後で、追跡の表から消えていることがある）。`T5OS/NET.RUL` か、組み込みの規則に足す |
| mDNS のレスポンダとブラウザ | コアにある（`MdnsResponder`、`MdnsClient`、`CommissionableSet`、`parse_operational`、`parse_host_addrs`） | `net` に mDNS はない（DNS のリレーとリゾルバだけ。doc/29） | **`matter` の中に置く**。ほかに mDNS を使うもの（`tab5.local` など）が出たら、`net` か独立した `mdns` サービスへ移すかを決める |
| ホスト名とアドレス | `Host::from_mac(mac, ipv6, ipv4)` と `sm_set_addrs` | `Net::if_info(k)`: MAC、IPv4、IPv6 を 4 つまで | 一時アドレスかどうかの印が if_info にない。IID で見分ける（上）。アドレスが変わったら `notify_change`（定期的に `if_info` を読む） |
| 時刻とタイマ | `now_ms: u64` と `next_deadline` | `time_ns()`、`Timer::set_oneshot_ns`、`wait` の timeout | なし |
| 乱数 | `crypto::Rng::fill_bytes` | `t5_rt::random_fill`（`--permit random`） | 10 行ほどのアダプタ |
| 永続化（KVS） | `kvs::Kvs`（get、set、remove。キーは 4 バイト: `fabm`、`fab0`…、`rsmp`） | `Files`（8.3、rename なし） | `T5OS/MATTER/` の下に、1 キー 1 ファイル（キーを 16 進にした 8.3 の名前）。**書き換えの途中の電断に弱い**ので、A/B の 2 ファイルと世代番号（または CRC）で、壊れていない新しい方を読む。値は数 KiB まで |
| 暗号 | `crypto::Crypto`。RustCrypto（sha2、hmac、hkdf、aes+ccm、p256 の ecdsa/ecdh） | なし（ソフトで計算する） | なし。P4 の SHA、AES、ECC の加速器は、カーネルのオブジェクトが要るので後で（§4.5） |
| attestation | デバイス: `dev_creds.rs` の試験用 DAC、PAI、CD（FFF1/8001）。コントローラ: `AttestationPolicy::{Skip, Verify, VerifyNoPaa}` | なし | MT1 は試験用の証明書をそのまま組み込む。後で factory-data を SD に置く（§4.6） |
| BLE（BTP） | `Btp` は sans-IO。GATT の 0xFFF6、C1 への write、C2 の indication、広告の service data（`AdvData::encode_adv`） | `bt:hci` だけ。`ble` サービスはこれから（M2〜M5） | `svc:ble` に、汎用の GATT の決まりごと（ペリフェラル: 広告、サービスの登録、C1 の書き込みと C2 の購読を知らせる、indication。セントラル: scan、connect、write、subscribe、indication を知らせる）。BTP は `matter` の中で動かす（§5） |
| Wi-Fi の資格情報（NetworkCommissioning） | デバイス: `sm_wifi_seed_network`、`sm_take_wifi_request`。コントローラ: `Commissioner::set_wifi_credentials` | `wifi` のビルド時の `credentials.rs`（`include!`）。実行中に変える口はない | デバイス: いま繋がっているネットワークを 1 つ見せ、追加と接続は断る（MT1）。コントローラ（ble-wifi）: SSID とパスワードが要る。`wifi` と同じ build.rs から渡すか、SD の設定から読む（§4.8） |
| Thread | `thread.rs` と Commissioner の `set_thread_dataset` | なし | 範囲外（§4.9） |

---

## 3. 構成

### 3.1 案の比較

| 案 | 内容 | 判断 |
|---|---|---|
| A. `matter` サービス 1 つ | デバイスとコントローラを 1 プロセスに。アプリとシェルは `svc:matter` を使う | **推奨**。5353 を 1 人で持てて、永続化も 1 か所で済む |
| B. 各アプリにリンク | アプリがそれぞれ Matter のノードになる | 5353 を取り合う。fabric と SD の書き込みも散らばる。アプリごとに 150〜200 KB のコードを持つ。却下 |
| C. A に加えて、アプリがエンドポイントを持ち込む | `matter` がノード（EP0）を持ち、アプリが EP1 から先を `svc:matter` で登録する（ブリッジに近い） | A の上で MT5 として作る |

### 3.2 `matter` サービスの中身（案）
- ハンドル（16 まで）: `listen:matter`、そのクライアント（シェル、アプリ）、`svc:net`、`svc:fs`、`random`、`timer`、そして後で `svc:ble`。
- ループ:
  1. `wait([listener, clients…, timer], min(next_deadline, 20 ms))`
  2. UDP 5540 を空になるまで読み、`MatterStack::handle_rx` に渡す。返った `SendDirective` を送る。
  3. 5353 を空になるまで読む。`handle_query` の応答を送り、`CommissionableSet::ingest` と `parse_operational` でコントローラに渡す。
  4. コントローラのソケット（エフェメラルのポート）を読み、`ControllerStack::handle_rx` に渡す。
  5. `poll` を、`None` になるまで呼ぶ（デバイス、コントローラ、`poll_announce` の 3 つ）。
  6. クライアントの要求を処理し、終わった操作の結果を返す。
- コントローラの処理は長い（PASE から CASE まで数秒）。要求を受けたら「受けた」とだけ返し、終わったら結果を送る（状態機械を `drive` で進める。ESP-IDF の `ctrl_pump` の、操作を 1 件ずつ実行する形と同じ）。
- デバイスとコントローラのスタックは、別々の型として持つ（`MatterStack` と `ControllerStack`）。tab5_ctrl_app も、デバイスとコントローラのシムを別々に使っている。両方の型を 1 つのバイナリに入れたときの大きさは §7 で見積もる。

### 3.3 `svc:matter` の決まりごと（下書き）
`svc:net` と同じく、要求と返事は 12〜28 バイトのメッセージ、大きいもの（TLV、ノード一覧）は共有メモリの切れ端を通す。チャネルは 1 メッセージ 256 バイト、待ち行列は 4（abi `chan`）。

- コントローラ:
  - `PAIR_ONNET`（node id、passcode、アドレスか discriminator）
  - `PAIR_BLE`（discriminator、passcode。MT4）
  - `NODES`（ノード帳を切れ端に）
  - `READ`、`WRITE`、`INVOKE`（EP、クラスタ、属性かコマンド、TLV は切れ端）
  - `SUBSCRIBE`、`UNPAIR`、`OCW`
  - `RESULT`（終わった操作の結果をとる）
- デバイス（MT5）:
  - `EP_ADD`（compose.rs の合成 TLV。EP の番号、デバイスタイプ、クラスタの一覧、初期値）
  - `ATTR_SET`（アプリから値を入れる。中で `mark_dirty` して購読者に報告）
  - `EVENTS`（コントローラからの書き込みやコマンドを、アプリが取りに来る）
  - `WINDOW`（コミッショニングの窓を開ける。QR と手入力用のコードを返す。`qr_payload` と `manual_pairing_code`）
- 属性の値は `matter` の中に持つ（キャッシュ）。読み出しのたびにアプリへ IPC で同期的に聞くことはしない。アプリが止まると Matter 全体が止まるため。コマンドは `matter` がすぐに Success で答え、アプリへは事後に知らせる。ESP-IDF の `SmClusterChange` と `sm_take_event` と同じ考え方。
  - 失敗を返さないといけないコマンド（Door Lock など）は、あとで「保留付きの応答」として扱う（IM の応答を少し待たせる。コアの invoke の継続が使えるかは**未確認**）。

### 3.4 デバイスの役で何を見せるか（MT1）
- まず、EP1 に仮想の On/Off ライト（bloat-check と同じ構成。`crates/bloat-check/src/lib.rs`）を置く。状態はログ（`event!`）と、画面のアプリへの `ATTR` 通知で見せる。
- 実際の部品（バックライトなど）をつなぐのは後回し。エクスパンダ #2 は bit0（WLAN_PWR_EN）だけ触ってよい約束があるので、ハードを動かすものは別に判断が要る。
- Tab5 らしいもの（候補）: 照度や温度のセンサ、画面上のスイッチ（Generic Switch）、Identify で画面を点滅させる。

---

## 4. 要件ごとの詳細

### 4.1 UDP、IPv6、マルチキャスト
- Wi-Fi のリンクだけなら、MT1 に必要な UDP の機能は今の API にそろっている。
  - `udp_bind_at(0, WLAN0, 5540)` は、アドレスを指定しないので、IPv4 と IPv6 の両方を受ける。
  - `udp_send_ip` で fe80 にも送れる（`route6` が Wi-Fi に出す）。
- 足すもの:
  - (1) `UDP_JOIN`/`UDP_LEAVE`（グループ。ff02::fb と 224.0.0.251）。smoltcp 0.14 の `Interface::join_multicast_group`。
  - (2) UDP_RECV の返事に、受けたリンクと宛先アドレス（v1 の空きか、v6 の返事の後ろ）。
  - (3) 送り元アドレスの固定（`udp_bind_at` に安定したアドレスを渡せば、smoltcp はそれを送り元にする。ただしそのときは 1 つのファミリだけになる。IPv4 と IPv6 で 2 つのソケットにするか、`net` に「送り元を受けた宛先にする」選択を足す）。
- NDP proxy（doc/27）: ff02::fb はリンクの中だけなので、下流（試験用 LAN、ホストリンク）には転送されない。下流の機器を Matter で見つけるには mDNS の中継が要る。範囲外としてよい。
- ソケットの数: `matter` は 5540、5353、コントローラ用の 1〜2 個で、4 個ほど使う。`net` は全体で 8 個まで（POOL 16）で、DNS のリレーなども使う。足りないときは SOCKETS を増やす（静的なバッファ。UDP 1 個で 8 KiB × 2）。
- クライアントの数: `net` は 4 人まで。shell、netctl、nettest に `matter` が加わる。足りなくなりそうなら増やす。

### 4.2 mDNS
- **`matter` の中に置く**。コアのレスポンダとクライアントはパケットを扱うだけなので、5353 のソケットを 1 つ持てばよい。
  - デバイス: `_matterc._udp`（窓が開いている間）と `_matter._tcp`（fabric ごと）。
  - コントローラ: `_matterc._udp` のブラウズ、`_matter._tcp` の解決、AAAA と A。
- 運用での発見（operational discovery）: コントローラは、ノード帳の node id から `<CFID>-<NodeId>._matter._tcp.local` を解決する。ノード帳にアドレスを覚えておき、届かなくなったときだけ引き直す（tab5_ctrl_app の `sm_ctrl_resolve_start` と `node_addr` と同じ）。
- 将来、ほかのプロセスも mDNS を使うなら、`net` に「mDNS の受信をいくつかのクライアントに配る」機能を足すか、`mdns` サービスを作る。今は要らない。

### 4.3 タイマ
- `MatterStack::next_deadline` と、コントローラの `transport_deadline`、`next_deadline` の小さい方を、`wait` の timeout にする。受信の通知がない間は、上限を 20 ms にする（§2）。
- `now_ms = time_ns() / 1_000_000`。

### 4.4 乱数
- `t5_rt::random_fill`（ヘルステストに失敗している間は `Fail`）を `Rng` に。失敗はそのまま `Error` で返す。P-256 の鍵と SPAKE2+ の乱数もこれを通る。doc/47 で鍵に使ってよい品質だと確かめてある（「鍵には使わない」の注意は外した）。

### 4.5 暗号
- RustCrypto（sha2 0.10、hmac 0.12、hkdf 0.12、aes 0.8、ccm 0.5、p256 0.13）。どれも alloc を使わない。オフラインで tab5-os のツールチェーンでもビルドできた（§9）。
- 速さ: ESP-IDF の上の Tab5（同じコード、同じ CPU）で、コミッショニングは 5〜7 秒（p4-thread-controller.md:782,804）。tab5-os ではコードが PSRAM のスロットか XIP にあり、ESP-IDF（コードは flash の XIP で、キャッシュを通る）とは置き場所が違う。P-256 の時間は測っていない（**未確認**）。MT0 で、`ecdsa` の署名と検証、`ecdh` の時間を実機で測る。
- P4 の加速器（SHA、AES、ECC、ECDSA）: カーネルのオブジェクトにして U モードに開く必要がある。CASE の 1 回が数百 ms で済むなら不要。測ってから決める。

### 4.6 attestation と、デバイスの身元
- デバイス: MT1 は、コアの `DEV_DAC_CERT_FFF1_8001`、`DEV_PAI_CERT_FFF1`、`DEV_CD_FOR_ALL_EXAMPLES` と、`dev_pase`（passcode 20202021、discriminator 3840 の verifier）を組み込んだまま使う。
  - chip-tool では、試験用の PAA のディレクトリを渡せば検証も通る（README.md）。
  - 後で `factory-data` feature（ESP-IDF の NVS の形式）か、SD の `T5OS/MATTER/FACTORY.BIN` に、Tab5 ごとの discriminator、verifier、シリアルを置く。DAC の秘密鍵を SD に平文で置くのは試験のときだけにする。
- コントローラ: smctl と同じく `VerifyNoPaa` を既定にする。自分の機器（NanoC6、AirQ）は試験用の DAC なので、それで通る。

### 4.7 永続化
- fabric（`fabm`、`fab0..`）、resumption（`rsmp`）、コントローラの CA（Ca::restore の形）、ノード帳（`controller::nodes`）。
- 書く頻度は低い（fabric が変わったとき、CASE の resumption のとき）。ただし resumption は CASE のたびに書くので、SD への書き込みが少し増える。
- SD を抜いているときは、RAM の中だけで動き、ログに `matter.kvs_volatile` を出す（`init` の、SD がなくても動く方針に合わせる）。
- `fs` の制約: 8.3 の名前、rename なし、FAT。§2 の A/B の 2 ファイルの方式にする。

### 4.8 Wi-Fi と NetworkCommissioning
- Tab5 がデバイスのとき、Tab5 はもう `wifi` サービスで Wi-Fi に繋がっている。on-network でのコミッショニングでは、NetworkCommissioning は「今のネットワークを 1 つ、接続済み」と見せるだけでよい（`sm_wifi_seed_network` と同じ考え方）。
  - `AddOrUpdateWiFiNetwork` と `ConnectNetwork` は、BLE でのコミッショニング（Tab5 がペリフェラル）のときだけ要る。そのときは `wifi` に実行中に資格情報を渡す口（`svc:wifi`）が要る。価値が低いので最後にする。
- Tab5 がコントローラで、相手が ble-wifi のとき: 相手に渡す SSID とパスワードが要る。
  - 今は `wifi` のビルド時の `credentials.rs` にしかない。
  - 候補は 2 つ: (a) `matter` の build.rs が同じファイルを読む。(b) `wifi` に、資格情報を渡す操作を足す（権限 `svc:wifi`）。
  - (a) は簡単だが、秘密が 2 つのイメージに入る。(b) の方がよい。

### 4.9 Thread
- C6 の上は、M5Stack が入れた esp_hosted 2.12.7 で、書き換えない約束（doc/48 §1、メモリの安全範囲）。esp_hosted は 802.15.4 を中継しない（**未確認**。2.12.7 の proto に 802.15.4 の RPC があるかは見ていない）。よって C6 で Thread は、範囲外とする。
- simple_matter の tab5_ctrl_app は、Port A の Unit Gateway H2（ot_rcp、spinel over UART）で Thread を使っている。tab5-os でそれをするには、UART のドライバ、OpenThread（C）か spinel のホスト、Port A の 5V が要る。5V はエクスパンダの別のビットかもしれない。これは**メモリの「エクスパンダ #2 は bit0 のみ許可」に触れる**ので、利用者の判断が要る。範囲外。

---

## 5. doc/48 の BLE の計画との関係
- doc/48 の方針（async は `ble` サービスの中だけ）と、simple-matter の作り（BTP 本体は sans-IO で同期型。GATT だけ async の trait）はよく合う。
  - `matter` は async を持たない。`svc:ble` のメッセージで GATT のイベントを受け、`Btp::process_incoming` に渡し、`process_outgoing` の断片を `svc:ble` に書く。
  - `PeerAddr::Ble(BtpConnId)` の `SendDirective` は BTP に回す。
- BTP の断片は ATT_MTU−3（最大 244 バイト）。チャネルの 1 メッセージ（256 バイト）に、ヘッダと一緒に入る。共有メモリは要らない。
- `svc:ble` に要るもの:
  - ペリフェラル（M4）: 広告（`AdvData::encode_adv` の 15 バイトの service data 0xFFF6）、0xFFF6 のサービスと C1（write）、C2（indicate）、C3（任意）の登録。`Connected`、`C1Write`、`C2Subscribed`、`Disconnected` の知らせ。indication と、その確認の知らせ。
  - セントラル（M3）: scan（service data 0xFFF6 の絞り込み）、connect（MTU の交渉の結果を返す）、C1 への write（応答あり。simple_matter 3a91d04）、C2 の subscribe、indication の知らせ、disconnect。
  - 順序の注意（gatt.rs のコメント）: handshake の C1 の write → C2 の subscribe。逆にすると chip のペリフェラルは subscribe を捨てる。
- trouble-host の版: tab5-os は =0.8.0、simple_matter の ESP32 のポートは 0.6（esp-radio の bt-hci 0.8 の制約）。コアの `ble` feature は TrouBLE に依存しない（依存なし）ので、版の違いは関係ない。tab5-os の `ble` の中で、GATT のサービスを静的に定義する（`gatt` と、M4 で `peripheral` の feature を足す）。
- Wi-Fi との共存（doc/48 §5）: スキャンの窓を間隔より短くする。BLE でのコミッショニングは、Wi-Fi が遅くなっても、数十秒で終わる。
- M5（ペアリング）は Matter には要らない（BTP は暗号化しないリンクの上で PASE を使う）。

---

## 6. コントローラの操作画面
- `run` は整数 2 つしか渡せないので、smctl のような別のアプリに文字列の引数を渡せない。**シェルに `mt` という組み込みのコマンドを足し、`svc:matter` に頼む**。`ping` が `svc:net` を使うのと同じ形。
  - `mt status`
  - `mt pair <node> <passcode> <addr>`（on-network、アドレス指定）
  - `mt pair <node> <passcode> d<discriminator>`（mDNS で探す）
  - `mt nodes`
  - `mt on|off|toggle <node> [ep]`
  - `mt read <node> <ep> <cluster> <attr>`
  - `mt sub <node> ...`
  - `mt unpair <node>`
  - `mt window`（Tab5 がデバイスのとき、窓を開けて手入力用のコードを表示）
  - `mt ble-pair ...`（MT4）
  - シェルの許可に `--permit svc:matter` を足す。
- GUI は後: `paint` や `term` と同じく、`display` と `touch` を持つ `mtui` アプリが `svc:matter` を使う。tab5_ctrl_app の画面（Devices、Dashboard、Pair のダイアログ）が手本になる。
  - QR は、デバイスの役の窓を開けたときに画面に出す（`qr_payload`）。
  - コントローラの役でカメラから QR を読む（docs/design/tab5-camera-qr.md）のは、カメラのドライバがないので範囲外。

---

## 7. 大きさの見積もり

bloat-check（デバイス: On/Off ライト一式。コントローラ: ControllerStack、Commissioner、MdnsClient、ca-state）を `riscv32imafc-unknown-none-elf` で測った（2026-11-04、この調査で実行）。

| 構成 | `.text` | `.rodata` | 計 |
|---|---:|---:|---:|
| デバイス、opt-level z と LTO（リポジトリの既定） | 129,442 | 7,850 | 約 134 KiB |
| デバイス、opt-level s と LTO | 139,374 | 7,800 | 約 144 KiB |
| デバイス、opt-level s、LTO なし（tab5-os の release に近い） | 167,102 | 20,055 | 約 183 KiB |
| コントローラ、z と LTO | 115,100 | 3,232 | 約 116 KiB |
| コントローラ、s と LTO | 127,782 | 3,168 | 約 128 KiB |
| コントローラ、s、LTO なし | 150,686 | 14,603 | 約 161 KiB |

参考: thumbv7em では、デバイスは約 102 KB（README）。RISC-V は 3 割ほど大きい。

- `matter` サービス（両方の役、tab5-os の profile = s、LTO なし、`debug = true` は読み込む大きさに入らない）: 暗号、TLV、transport、mDNS は両方で共有するので、**約 250〜350 KiB のコード**と見込む（**未確認**。足し算 340 KiB から共有分を引いた）。
- RAM（ホストで `size_of`）:
  - DefaultStack は 22,048 B、外側を含めて約 32 KiB。thumbv7em では 23,982 B（README）。
  - コントローラの状態は bloat-check にない（**未確認**）。数十 KiB と見込む。
- スタック: ESP-IDF の Tab5 のコントローラは 128 KiB の静的スタックが必要だった（ctrl_pump.cpp:2877。sm_ctrl_init のスタックの一時コピーと、P-256 の署名チェーン）。`--stack 131072` から始め、`t5log` のスタックの使用量（doc/40）で詰める。
- 合計で 1 MiB を下回る見込み。3 MiB のスロットに十分収まる。XIP にもできる。

---

## 8. 危険と未確認の点

1. **プロセスの数（上限 8、`sched::MAX_PROCS`）**: 常駐は `wifi`、`net`、`fs`、`font`、`term`、`shell` の 6 つ（`init` と `netctl` は終わる）。`ble` と `matter` を足すと 8 で上限に達し、`hcitest` などを動かす余地がなくなる。MAX_PROCS を 12 にする（スロットは 2 MiB になる）か、`font` を `term` に入れるなどの判断が要る。
2. マルチキャスト: smoltcp の join と、C6 の受信（ff02::fb、224.0.0.251）。届かないと mDNS が使えない。その場合でも、アドレスを指定した on-network（`pairing already-discovered` と `mt pair ... <addr>`）は動く。
3. 送り元アドレス（一時アドレス）。§4.1。
4. 受信の通知がない。20 ms の問い合わせで CPU をどれだけ使うか（doc/31 の CPU 使用率で測る）。
5. SD への書き込みの安全さ（rename なし）と、SD がないときの動き。
6. P-256 の速さ（PSRAM のコード）と、t5emu（40 MHz）での時間。PASE と CASE の時間切れは 60 秒（`PASE_SESSION_EST_TIMEOUT_MS`）なので、t5emu でもたぶん間に合う（**未確認**）。
7. `ble` サービスの M3 と M4 の完成。TrouBLE の GATT サーバを tab5-os の executor で動かせるか（M2 の判定次第で、中身を自作に替える可能性がある）。
8. simple-matter 側のバグの修正は、tab5_ctrl_app と同じ経路で入る（例: exchange のリーク f474663）。tab5-os からは path 依存（`../simple_matter`）か git の rev に固定して取り込む。どちらにするかは利用者の判断。user/Cargo.lock に p256 などが増える。
9. `net` のソケットとクライアントの上限（8 と 4）。
10. 証明書: 試験用の DAC は製品には使えない（試験用）。

---

## 9. コンパイルの確認（この調査で実行）

両リポジトリのソースは変えていない。出力はスクラッチパッドの `sm-target*` の下に置いた。どれも `--offline --locked` で、ネットワークは使っていない。

1. `cargo build -p bloat-check --bin flash-probe --release --target riscv32imafc-unknown-none-elf`（stable、opt z と LTO）: 成功。
2. `controller-probe`（`--features controller`）: 成功。
3. `CARGO_PROFILE_RELEASE_OPT_LEVEL=s`、LTO のあり／なし: 成功（§7 の表）。
4. tab5-os と同じ条件: `RUSTFLAGS="-C relocation-model=pie" cargo +nightly-2026-09-10 build -Zbuild-std=core -p simple-matter --no-default-features --features rustcrypto,controller,ble --release --target riscv32imafc-unknown-none-elf`: **成功（12.6 秒、警告なし）**。

まだしていないこと: tab5-os の user workspace にメンバーとして入れ、`t5_rt` とリンクし、`elf2t5a` でイメージにすること（MT0 の作業）。

---

## 10. 段取り

| 段 | 内容 | 終わりの条件 | t5emu | 実機 |
|---|---|---|---|---|
| **MT0** | user workspace に `matter`（中身は空）。simple-matter を path 依存に。`Rng`（random）と `Kvs`（fs、A/B）のアダプタ。暗号の速さを測る試験（`mt0` の引数で、ecdsa の署名と検証、ecdh、PBKDF2 1000 回の時間を `event!`） | ビルドでき、イメージになる。KVS の往復の自己試験が通る。暗号の時間が出る | ○（KVS は `--sd`、時間は参考値） | ○（暗号の時間の本番） |
| **MT1a** | `matter` のデバイスの役（On/Off、試験用の証明書）、UDP 5540。プロセスの中の試験用コントローラ `mttest`（ループバックの 127.0.0.1 で PASE、AddNOC、CASE、Toggle、そして fabric の永続化と再起動後の CASE） | `test-p4emu` に入れる | ○（ループバックだけ。mDNS なし） | ○ |
| **MT1b** | `net` に `UDP_JOIN`、受けたリンクと宛先、送り元の固定。フィルタの規則。`matter` に mDNS のレスポンダ | PC の chip-tool/smctl から `pairing onnetwork 1 20202021`（mDNS で見つける）、`onoff toggle`、subscribe、Tab5 の再起動後の CASE resumption | △（ホストリンクで `already-discovered 10.55.0.2`。マルチキャストは**未確認**） | ○（本番は Wi-Fi の LAN） |
| **MT2** | コントローラの役（CA とノード帳を SD に、Commissioner は `VerifyNoPaa`）。`svc:matter` のコントローラの操作と、シェルの `mt` のコマンド | シェルから、NanoC6（`e5-light`、Wi-Fi で運用中）を on-network でコミッショニング（`mt pair` に手入力用のコードかアドレス）し、toggle と subscribe。再起動後もノード帳から再接続 | △（`mttest` が相手のループバックだけ） | ○ |
| **MT3** | Tab5 がデバイスで、窓を開ける（OCW、`mt window`、QR とコード）。マルチ fabric（chip-tool と Tab5 自身のコントローラの両方から） | Tab5 のコントローラが、Tab5 のデバイスを含む 2 台を扱える | ○（ループバック） | ○ |
| **MT4** | BLE（doc/48 の M3 と M4 の後）。`svc:ble` の GATT の決まりごとと、`matter` の BTP。コントローラの ble-wifi（`wifi` から資格情報を受け取る）。デバイス側のペリフェラルは後 | `mt ble-pair` で、出荷状態の NanoC6 を BLE → Wi-Fi → CASE まで | ×（t5emu に HCI はない。**未確認**） | ○ |
| **MT5** | アプリがエンドポイントを持ち込む（`EP_ADD`、`ATTR_SET`、`EVENTS`。compose.rs の合成 TLV を移植）と、GUI の `mtui`（ノード一覧、Toggle、センサの表示、ペアリングのダイアログ、デバイスの役の QR） | `paint` などのアプリが On/Off の EP を出し、chip-tool から操作できる。画面から操作できる | △（画面は t5emu の表示で） | ○ |

- 範囲外として記録だけするもの: Thread（H2 と UART、エクスパンダの判断）、P4 の暗号の加速器、mDNS の中継（NDP proxy の下流）、OTA（BDX）、ICD。
- 実機での作業（書き込み、`install-apps`、`t5log rtt`）は、メモリの約束どおり、その都度確認してから行う。実機のストアは古いアプリのままなので、試す前に入れ直す。
