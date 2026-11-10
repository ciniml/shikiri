# 71. Tailscale のクライアント（検討と実装）

利用者の依頼（2026-11-10）: tab5-os に Tailscale のクライアントを入れられるかを調べ、設計と段取りを考える。この文書は調査と設計だけ。コード、実機、ホストの網の設定には触れていない。

- 書き方: **（確認）** は、この文書を書くときに一次資料（ソース、実際の応答）で確かめたこと。**（推測）** は確かめていないこと。どちらも付けていないものは、このリポジトリの既存の文書に書いてあること。
- 調べた版: Tailscale（Go）は main（2026-11-10 取得、最新のリリースは v1.104.5、`CurrentCapabilityVersion` = 150）。tailscale-rs は a2a56c4（2026-11-06、0.7.1）。

## 1. 結論（先に）
- **作れる**。最小の形は「DERP だけで通る、auth key で登録する、`ts` というアプリ（サービス）の中に小さな IP スタックを持つ」もの。Tab5 の CPU とメモリには十分な余裕がある（§4.3）。
- **一番重いのは暗号ではなく、周りのプロトコル**: 制御は Noise の上の **HTTP/2**（HPACK を含む）と大きな JSON、DERP は **TLS 1.3**。どちらも今の tab5-os にない。
- **制御の平面には TLS は要らない**。公式のクライアントも、まず 80 番の平文の HTTP で `/ts2021` に Upgrade し、その上は Noise で守る（確認、§3.4）。ただしサーバの公開鍵を取る `/key` は HTTPS にリダイレクトされる（確認）ので、鍵は設定に固定する。
- **公開の DERP には TLS が要る**（確認、§3.4）。TLS 1.3 のクライアント（`embedded-tls`、no_std、alloc なし）を入れるか、最初は証明書を確かめない形で始めるか、自前の DERP を立てるかを決める必要がある（§8 の問い）。
- **そのまま使える no_std の Rust の実装はない**。tailscale-rs（Tailscale 社の Rust の実装）は tokio と std が前提で、no_std なのは鍵、disco、制御の JSON の型などの一部の crate だけ（確認、§3.1）。simple-matter のときと同じく、同期型（sans-IO）の核を自分で書き、RustCrypto の暗号と tailscale-rs の no_std の部品を借りるのがよい（判断の案）。
- **アプリで初めてヒープが要る**。map の応答は大きさの決まらない JSON（小さな tailnet で数 KB〜数十 KB、大きなものは数 MB。tailscale-rs の issue #16、確認）。今のアプリは alloc を使っていない（確認: `user/` に `global_allocator` がない）ので、`ts` の中に静的な領域のヒープを置く。

## 2. 最小のクライアントに要るもの

### 2.1 全体の流れ
```
         制御サーバ（tailscale.com か headscale）
            ▲  TCP 80: POST /ts2021（Upgrade）→ Noise IK → HTTP/2
            │    /machine/register、/machine/map（長く開けたまま）
   ┌────────┴────────┐
   │  ts（Tab5）     │── TCP 443: TLS → GET /derp（Upgrade: DERP）── DERP の中継 ──▶ 相手
   │  鍵、netmap、WG │── UDP 41641: WireGuard と disco（直の経路、後の段）────────▶ 相手
   └─────────────────┘
```

### 2.2 部品ごとの要否

| 部品 | 中身 | 最初の版 | 根拠・メモ |
|---|---|---|---|
| 鍵 | machine key（機器、長く使う）、node key（= WireGuard の鍵）、disco key（起動ごとでよい） | **要る** | すべて X25519。machine key と node key は保存する（§4.5） |
| サーバの鍵 | `GET /key?v=150` の `publicKey`（`mkey:…`） | **固定値でよい** | 80 番では HTTPS へ 302（確認）。設定に書く |
| ts2021 | TCP 80 で `POST /ts2021`、ヘッダ `Upgrade: tailscale-control-protocol`、`Connection: upgrade`、`X-Tailscale-Handshake: base64(Noise の最初の通信)`。101 の後は Noise の枠 | **要る** | 確認（`controlhttp/client.go`、`controlhttpcommon.go`）。§3.4 |
| Noise | `Noise_IK_25519_ChaChaPoly_BLAKE2s`、prologue は `"Tailscale Control Protocol v" + 版`。枠は最大 4096 バイト | **要る** | 確認（`controlbase/handshake.go`、`conn.go`） |
| EarlyNoise | Noise の直後に来ることがある `\xff\xff\xffTS` + 長さ + JSON（NodeKeyChallenge など） | **要る**（読み飛ばしだけでも） | 確認（`ts2021/conn.go`）。来ないサーバもある |
| HTTP/2 | Noise の上で HTTP/2 だけを使う（`ts2021/client.go:170` のコメント「We force only HTTP/2」） | **要る** | 確認。SETTINGS、HEADERS（HPACK）、DATA、WINDOW_UPDATE、PING、GOAWAY、RST_STREAM。ストリームは 2〜3 本で足りる |
| 登録 | `POST /machine/register`（`Version`、`NodeKey`、`Hostinfo`、`Auth.AuthKey`）→ `MachineAuthorized` か `AuthURL` | **要る** | 確認（`control/tsp/register.go`） |
| map | `POST /machine/map`（`NodeKey`、`DiscoKey`、`Hostinfo`、`Stream`、`KeepAlive`、`Compress`）。答えは「4 バイトの長さ（LE）+ 本体」の並び | **要る** | 確認（`tsp/map.go`）。`Compress: ""` で zstd なし（`tailcfg.go:1446` のコメント「"zstd" or "" (no compression)」、確認） |
| netmap の解釈 | 自分の `Node`（アドレス）、`Peers`（Key、DiscoKey、Addresses、AllowedIPs、HomeDERP、Endpoints）、`DERPMap`、`PacketFilter`、`ControlTime` | **要る**（必要な項目だけ） | 確認（`tailcfg.go`）。差分の更新（`PeersChanged` など）も読む |
| DERP | TLS の上で `GET /derp`、`Upgrade: DERP`。枠: ServerKey、ClientInfo（NaCl box の JSON）、ServerInfo、SendPacket、RecvPacket、KeepAlive、Ping/Pong、NotePreferred | **要る** | 確認（`derp/derp.go`、`derphttp_client.go`）。1 つの DERP（自分の home）だけでよい |
| WireGuard | `Noise_IKpsk2`、TAI64N、MAC1、カウンタと再送防止の窓、鍵の入れ替え（2 分、REKEY） | **要る** | 相手の静的鍵 = node key。最初の通信は 148 バイト、返事は 92 バイト（wireguard.com/protocol の構造から計算、確認） |
| パケットフィルタ | netmap の `PacketFilter` で、入ってくるパケットを照らす | **要る**（既定は拒否で） | 受けないと tailnet の ACL が効かない。tailscale-esp32 も「フィルタを受けるまでは通さない」としている（確認、`docs/protocol-support.md`） |
| disco | `TS💬` + disco 公開鍵 + nonce + NaCl box（ping、pong、CallMeMaybe） | **最小の pong だけ**（推測で要る） | DERP 越しの ping に pong を返すだけなら小さい。答えなくても DERP で通るかは未確認（§2.3） |
| STUN と直の経路 | DERP の STUN（3478）で外のアドレスを知り、disco で穴をあける | **後回し** | DERP だけで通る。速さと遅れは悪くなる |
| MagicDNS | 100.100.100.100 の DNS | **後回し** | netmap の名前（`host.tailnet.ts.net.`）から自分で引けば足りる |
| 対話のログイン | `AuthURL` を利用者がブラウザで開く | **後回し**（画面に QR を出す形で後で） | auth key なら URL は要らない |
| そのほか | exit node、subnet router、Taildrop、SSH、serve/funnel、key の自動の入れ替え、Tailnet Lock、ログの送信 | **出さない** | tailscale-rs も多くは未対応（確認、README） |

### 2.3 DERP だけの版で通るか
- 自分の側: map の要求の `Hostinfo.NetInfo.PreferredDERP` に home の DERP を書き、`Endpoints` は空にする。相手は自分の node key あてに、自分の home の DERP へ WireGuard を送る。
- 相手の側（Go の magicsock）は、直の経路がなければ DERP で送り、並行して disco で直の経路を探す（推測。magicsock のソースは読んでいない）。
- tailscale-esp32 は「home の DERP 越しの WireGuard を受ける」形で動くとしている（確認、README。動作は確かめていない）。
- 相手の disco の ping に答えないと何が起きるかは未確認。DERP 越しの pong だけは最初の版に入れておく（判断の案）。

## 3. 既存の実装と部品

### 3.1 Tailscale の実装

| 名前 | 版・日付 | ライセンス | no_std | メモ |
|---|---|---|---|---|
| tailscale（Go） | v1.104.5 | BSD-3-Clause | – | 仕様の代わり。`control/tsp`（Go）は最小のクライアントの書き方の手本（`/key`、register、map、zstd の外し方）（確認） |
| **tailscale-rs**（tailscale/tailscale-rs、crate `tailscale`） | 0.7.1、2026-11-06。2026-04-05 公開、「preview, experimental」 | BSD-3-Clause | 一部だけ | tokio 前提。DERP と直の経路、TCP/UDP。MagicDNS、exit node などは未対応（確認、README） |
| tailscale-esp32（0xdilo） | 0.1.0、2026-07 | MIT | なし（std、ESP-IDF には依らない） | ts2021、最小の HTTP/2、register、map、WireGuard、disco、STUN、DERP。`src/` は約 213 KB（確認）。依存: snow 0.9、x25519-dalek 2、chacha20poly1305、blake2、crypto_box、serde_json、httlib-hpack。星 7、個人の作 |
| MicroLink（CamM2325/microlink） | 2026-03 | 不明（GitHub は NOASSERTION） | C、ESP-IDF | DERP、disco、STUN、MagicDNS。README によれば SRAM 静的 85〜116 KB、map のために PSRAM に H2 512 KB と JSON 512 KB（確認は README だけ）。TLS は mbedTLS |

tailscale-rs の crate（a2a56c4 で確認）:

| crate | `#![no_std]` | 中身 | 依存で気になるもの |
|---|---|---|---|
| `ts_keys` | ある | 鍵の型 | x25519-dalek、crypto_box |
| `ts_disco_protocol` | ある | disco の符号化 | crypto_box |
| `ts_control_serde` | ある（alloc） | 制御の JSON の型（2,720 行） | serde_json、chrono、url、ipnet。std が混ざる件は issue #455 で直った |
| `ts_packet` | ある | パケットのバッファ | bytes、yoke |
| `ts_noise` | ない | Noise の IK と IKpsk2（818 行） | `std` は数か所だけ（推測で直しやすい） |
| `ts_tunnel` | ない | WireGuard（3,393 行） | `std::collections::HashMap`、`Instant`、`SystemTime` |
| `ts_control_noise`、`ts_control`、`ts_derp` | ない | ts2021、制御、DERP | tokio、futures、tokio-util |
| `ts_netstack_smoltcp` | ある（印だけ） | smoltcp の上のソケット | flume が std を引く（issue #437、未解決） |

- 開発者は issue #437 で「no_std はちゃんと追っていない、主な問題は HashMap だろう」と答えている（確認）。
- 結論: tailscale-rs を丸ごと載せるのは無理。`ts_keys`、`ts_disco_protocol` はそのまま、`ts_noise`、`ts_tunnel` は third_party に置いて std を外す（smoltcp と同じやり方、doc/20）か、読んで自分で書く。

### 3.2 WireGuard

| 名前 | 版 | ライセンス | no_std | メモ |
|---|---|---|---|---|
| boringtun（Cloudflare） | 0.7.1（2026-05） | BSD-3-Clause | なし | `noise` モジュールは sans-IO に近いが、`ring`、`libc`、`parking_lot`、`getrandom` に依る（確認、Cargo.toml）。ring を riscv32 の U モードで組めるかは未確認 |
| gotatun（Mullvad、boringtun の枝） | 0.10.3（2026-11-09） | MPL-2.0 | なし | aws-lc-rs か ring。重い（推測） |
| tailscale-rs `ts_tunnel` | 0.7.1 | BSD-3-Clause | なし | §3.1 |
| tailscale-esp32 `wireguard.rs` | 0.1.0 | MIT | なし | 約 21 KB のソース。読みやすい手本 |
| wireguard-lwip（smartalock）、esp_wireguard（trombik）、WireGuard-ESP32-Arduino（ciniml） | – | GitHub は NOASSERTION | C | lwIP の上の WireGuard。ESP32 での実績がある。ciniml のものは利用者の作 |

- WireGuard の本体（ハンドシェイク、データ、タイマ）は 1,000〜1,500 行ほどで書ける（推測。ts_tunnel は試験を含めて 3,393 行）。

### 3.3 Noise と暗号

| crate | 版 | ライセンス | no_std | メモ |
|---|---|---|---|---|
| snow | 0.10.0 | Apache-2.0 OR MIT | alloc が要る（推測） | 汎用。ts2021 には IK だけでよい |
| clatter | 2.3.0 | MIT | no_std、alloc なし | 「no_std compatible implementation of Noise」（確認、crates.io）。IK と 25519、ChaChaPoly、BLAKE2s を選べる |
| x25519-dalek | 3.0.0（2026-07） | BSD-3-Clause | no_std | |
| chacha20poly1305 | 0.11.0 | Apache-2.0 OR MIT | no_std | 今の `Cargo.lock` にはない（aead 0.5.2 はある） |
| blake2 | 0.11.0 | MIT OR Apache-2.0 | no_std | BLAKE2s と、Noise の HKDF（HMAC-BLAKE2s） |
| crypto_box | 0.9.1 | Apache-2.0 OR MIT | no_std（alloc は任意） | DERP の ClientInfo と disco の NaCl box（XSalsa20Poly1305） |

- ts2021 の Noise と WireGuard の Noise は形がほぼ同じ（IK と IKpsk2）。HKDF と MixHash を共用すれば、自分で書いても 300〜500 行（推測）。clatter を使うか自分で書くかは T0 で決める。
- 版の注意: RustCrypto は 0.10 系（今の lock にある aead 0.5、sha2 0.10）から 0.11 系へ移っている途中。simple-matter が使う版とそろえないと、同じ機能の crate が 2 つ入る（推測）。

### 3.4 HTTP と TLS
- **ts2021 は 80 番の平文で足りる**（確認）:
  - `controlhttp/client.go`: まず 80 番を試し、うまくいかなければ少し待って 443 番（TLS）も試す。80 番を使う理由は「二重の暗号を避けるため」とコメントにある。
  - 443 番で TLS を使うときも、証明書の検査の失敗は記録するだけで通す（`VerifyConnection` が `return nil // regardless`）。中身は Noise で守られているから。
  - 実際に `http://controlplane.tailscale.com/ts2021` へハンドシェイクなしの POST を送ると 400（口がある）。`/key` は 302 で HTTPS へ（2026-11-10、確認）。
- **HTTP/1.1 の Upgrade**: 要るのは「POST を 1 つ送り、`101 Switching Protocols` と `Upgrade` の値を確かめ、残りのバイトを Noise に渡す」だけ。`ota` の HTTP/1.0 の GET（doc/67 §5）と同じ程度の大きさで書ける。DERP の `GET /derp` も同じ形。
- **HTTP/2**: `h2`（0.4.20、MIT）は tokio と std が前提で使えない。最小の HTTP/2 のクライアントを自分で書く。
  - HPACK の符号化はリテラル（索引なし、Huffman なし）だけでよい。復号は静的表、動的表、Huffman が要る（サーバが使う）。
  - フロー制御: map の応答は長いので、WINDOW_UPDATE を早めに返す（MicroLink も README で同じことを書いている）。
  - 大きさの目安: tailscale-esp32 の `h2.rs` は約 19 KB のソース（HPACK は別の crate）。
- **DERP の TLS**:
  - 公式のクライアントは、DERP にいつも HTTPS（443、`DERPPort` があればそれ）でつなぐ。平文は環境変数のデバッグの設定のときだけ（確認、`derphttp_client.go` の `useHTTPS`）。
  - 公開の DERP の証明書（2026-11-10、`derp7e.tailscale.com` で確認）: TLS 1.3、`TLS_AES_128_GCM_SHA256`、鍵交換 X25519。リーフは ECDSA P-256（約 920 バイト）、中間は Let's Encrypt YE1（P-384）、その上は Root YE、ISRG Root X2（どちらも P-384）。`controlplane.tailscale.com` も同じ形。
  - つまり、確かめるには ECDSA P-384 の検証が要る。今の lock にある p256 だけでは足りない。
  - `embedded-tls` 0.19.0（Apache-2.0、TLS 1.3 のクライアント、no_std、alloc なし）。README によれば、証明書の検査（`webpki` か `rustpki`、P-256/P-384）、日付を見ない検査（`InsecureIgnoreDates`）、検査なし（`InsecureNoVerify`）を選べる。同期型の `blocking` もある（確認は README だけ。組んではいない）。
  - 検査しない場合: DERP の中身は WireGuard で端から端まで暗号化されているので、中継の人に中身は見えない。ただ、なりすました DERP は、誰と誰が話しているか（node key）を見られ、止めることもできる（推測）。

### 3.5 headscale
- juanfont/headscale v0.29.7（2026-10-30）、BSD-3-Clause。ts2021 と DERP（埋め込みの DERP も）を持つ自前の制御サーバ。
- **合わせる意味はある**:
  - 試験の相手として。LAN の中に置け、`server_url` を `http://` にでき、auth key を自分で発行できる（推測。headscale の設定は今回読んでいない）。
  - プロトコルは同じなので、両方で動くように作るのは難しくない。違いは `/key` の取り方（headscale は自分の `server_url` で配る）と、DERP の地図。
  - 利用者が tailscale.com を使うなら、headscale は試験だけでよい。

## 4. tab5-os に合わせる

### 4.1 どこに置くか
- **`ts` サービス（アプリ）を 1 つ作る**（判断の案）。`matter` と同じく、同期型の核を `wait` のループに載せる。async は要らない（async は `ble` の中だけ、doc/48 §1）。
- `ts` は `svc:net` の普通のクライアントとして、下の網（underlay）を使う:
  - TCP: 制御サーバの 80 番、DERP の 443 番（`svc:net/tcp-connect`）。名前は `svc:net/resolve`。
  - UDP: 41641 番（直の経路、後の段。`svc:net/udp:41641`）。受信の通知は `UDP_WAIT`（doc/49 §18）。
  - 今の `svc:net` で足りる。`net` を変えずに始められる。
- **上の網（tailnet の IP）を誰が持つか**: 2 つの形がある。

| 形 | 中身 | 良い点 | 悪い点 |
|---|---|---|---|
| A. `ts` の中に IP スタック | `ts` が自分の smoltcp（`Medium::Ip`、100.x/32 と fd7a:115c:a1e0::/48 の自分のアドレス）を持つ。tsnet や tailscale-rs と同じ形 | `net` を変えない。tailnet の通信は `ts` を通る分だけ、と決まる | ほかのアプリが tailnet を使うには、`svc:ts` に `svc:net` に似た口を作る |
| B. `net` に tun のようなリンク | `ts` が復号した IP パケットを `net` のリンク `ts0` に渡し、`net` は 100.64.0.0/10 を `ts0` へ向ける | 今のアプリが `svc:net` のまま tailnet を使える。フィルタ（doc/24）、NAT（doc/25）、転送（doc/23）がそのまま使え、Tab5 を subnet router にもできる | リンクは今はカーネルのオブジェクト（`netdev:host` など）だけ。アプリが出すリンクの仕組み（共有メモリの輪と通知）を `net` に足す必要がある。パケットが `ts`→`net`→アプリと 1 回多く写る |

- **まず A、要るなら後で B**（判断の案）。最初の使い道（ping に答える、状態のページを出す、tailnet の HTTP から取る）なら A で足り、`net` に手を入れずに済む。
- `svc:ts` の口（A の場合）: `svc:net` と同じ 12 バイトの要求と 64 KiB の切れ端の形（`TCP_CONNECT`、`TCP_LISTEN`、`SEND`、`RECV`、`UDP_*`、`RESOLVE` を tailnet の名前で）にすれば、`t5_rt::Net` をほぼ使い回せる（推測）。grant は `svc:ts/tcp-connect`、`svc:ts/tcp-listen:PORT`、`svc:ts/admin`（ログイン、ログアウト）など（doc/52 の形）。

### 4.2 メモリ
| 項目 | 見込み | 根拠 |
|---|---|---|
| コード | 400〜700 KB | `matter` の像は 474 KB（確認、`build/apps/matter.t5a`）。ts は暗号が小さい（P-256 と SPAKE2+ の代わりに X25519）一方、serde_json、HTTP/2、TLS（P-384 の検査を入れると大きい）、smoltcp が入る（推測） |
| ヒープ | 256 KiB〜1 MiB | map の 1 通を丸ごと読んでから解釈する場合。小さな tailnet なら数十 KB。DERP の地図だけで約 16 KB（`https://login.tailscale.com/derpmap/default`、28 地域、確認）。上限を設けて、超えたら断る（tsp は既定で 4 MiB、確認） |
| バッファ | 約 100 KiB | TLS の受信の枠 16 KiB + 送信、Noise の枠 4 KiB × 2、HTTP/2 の受信、WireGuard の 1 パケット、smoltcp のソケット（数個） |
| netmap | 相手 1 つあたり数百バイト | 相手の数で増える。名前と鍵とアドレスだけを持つ |
| スタック | 32〜64 KiB | `matter` は 128 KiB。暗号の関数の中でスタックを食う |
| 計 | **約 1〜2 MiB** | アプリの 1 スロットは 2 MiB（doc/30 §4）。超えるなら続いた 2 スロット（`free_span`） |

- ヒープ: 今のアプリは alloc を使っていない。`ts` の中だけに `GlobalAlloc`（静的な配列の上の小さなアロケータ。例えば `embedded-alloc` や `talc`。どれにするかは未定）を置く（判断の案）。serde_json と `ts_control_serde` は alloc が前提。
- JSON を流しながら読む方法（`picojson` 0.3.0 などの pull 型）なら、ヒープを数十 KB にできる。ただ、netmap の型は大きいので書く量が増える。まずは丸ごと読む（判断の案）。
- プロセスの数: 上限は 12（`sched::MAX_PROCS`、確認）。常駐（fs、wifi、net、netctl、cfg、font、term、shell、ble、matter、ota など）に `ts` が 1 つ増える。

### 4.3 CPU
| 処理 | 見込み | 根拠 |
|---|---|---|
| X25519 1 回 | 5〜20 ms | 推測。P-256 の ECDH が 22 ms（doc/49 §2、Tab5、360 MHz）。X25519 はふつう P-256 より軽い |
| ts2021 のハンドシェイク | 数十 ms | DH が 4 回ほど |
| WireGuard のハンドシェイク | 数十 ms | DH が 4 回ほど。2 分ごと（REKEY） |
| ChaCha20-Poly1305 | 数 MB/s 以上 | 推測。AES-CCM（ソフト）が約 0.5 MB/s（doc/49 §2）。ChaCha20 は 32 ビットの加算、XOR、回転だけなので、ソフトの AES よりかなり速いはず。T0 で測る |
| 下の網の速さ | 約 13 Mbps（TCP） | doc/35、doc/37。DERP 越しでは DERP までの往復と中継が加わる |

- tailnet の通信は、Wi-Fi の速さと DERP の遅れで決まり、CPU は足りる見込み（推測）。
- エミュレータ（40 MHz、doc/11）では暗号が約 9 倍遅い。Matter の PASE は模擬時間で約 6 秒だった（doc/49 §16）。X25519 が数回の ts なら、数秒で通る見込み（推測）。

### 4.4 暗号の部品
| 部品 | 使う所 | crate の案 |
|---|---|---|
| X25519 | machine/node/disco の鍵、Noise、WireGuard、NaCl box | x25519-dalek（curve25519-dalek の 32 ビットの実装） |
| ChaCha20-Poly1305 | Noise（ts2021）、WireGuard のデータ | chacha20poly1305 |
| BLAKE2s | Noise の hash と HKDF、WireGuard の MAC1/MAC2 | blake2 |
| XSalsa20-Poly1305（NaCl box） | DERP の ClientInfo/ServerInfo、disco | crypto_box |
| TLS 1.3（AES-128-GCM、X25519、ECDSA P-256/P-384 の検証） | DERP | embedded-tls と RustCrypto |
| 乱数 | 鍵の生成、ephemeral、nonce、WireGuard の送信者の番号 | カーネルの `random`（§4.6） |

- P4 には AES や SHA の加速器があるが、Matter と同じく当面はソフトで足りる（doc/49 §2 の判断に合わせる）。

### 4.5 状態の保存
| 値 | 大きさ | 置き場の案 |
|---|---|---|
| machine key（秘密） | 32 B | `cfg` の秘密の値（`ts.machine_key`）。値は 128 B まで（doc/50 §2）なので入る |
| node key（秘密） | 32 B | 同じ（`ts.node_key`） |
| auth key（秘密） | 60 B 前後（推測） | 同じ（`ts.auth_key`）。登録が通ったら消してよい |
| 制御サーバの URL、サーバの公開鍵 | 数十 B | `cfg` の普通の値（`ts.server`、`ts.server_key`） |
| WireGuard の最後の時刻（§4.7） | 12 B | `cfg` か `svc:fs`（`T5OS/TS/`） |

- `cfg` の秘密は SD のファイルの中では平文（doc/50 §1）。カードを抜けば node key を読まれ、tailnet にその機器として入られる。今の Wi-Fi の passphrase と同じ扱い。気になるなら key の期限を短くするか、ephemeral の node にする（§8 の問い）。

### 4.6 乱数
- カーネルの `random`（doc/47）を使う。SAR ADC のエントロピー源とヘルステストがあり、統計の検定は通っている。
- doc/47 §8 の残り「鍵を作るアプリのための、ハッシュや DRBG を使った条件付け」がまだ。`ts` の中で、`random` の 64 バイトを BLAKE2s に通して ChaCha20 の乱数器の種にする（判断の案。mtport の `KernelRng` と同じ場所に置けば Matter も使える）。
- `random` が `Fail` を返したとき（ヘルステストの失敗）は、鍵を作らずに止まる。`net` のように時刻に戻ってはいけない。

### 4.7 時刻
- カーネルの `time` は単調な時刻だけ（doc/07）。壁時計はない。RTC（RX8130）はまだ読んでいない（doc/18 §「時計がない」）。
- 時刻が要る所:

| 所 | 要る理由 | 案 |
|---|---|---|
| WireGuard の TAI64N | 受け手は相手ごとに最大の時刻を覚え、それ以下の最初の通信を捨てる（wireguard.com/protocol、確認）。起動のたびに 1970 年から始めると、相手が前の値を覚えている間はハンドシェイクが通らない | map の応答の `ControlTime`（`tailcfg.go`、確認）で合わせる。map はデータより先に来るので間に合う。念のため、最後に使った時刻を保存し、それより後を使う |
| TLS の証明書の日付 | 期限の検査 | `ControlTime` か HTTP の `Date` を使う。最初は日付を見ない（`InsecureIgnoreDates`） |
| node key の期限 | 期限はサーバが決める | 画面に出す程度。`ControlTime` で計算 |
| タイマ（REKEY 2 分、keepalive、DERP の keepalive） | 単調な時刻で足りる | カーネルの `time` |

### 4.8 権限（doc/52 の形）
- 案: `svc:net/tcp-connect`（80 と 443。今の grant は `tcp-connect[:PORT]` なのでポートを絞れる）、`svc:net/resolve`、`svc:net/udp:41641`（後の段）、`svc:net/info`、`random`、`svc:cfg/r:ts.`、`svc:cfg/w:ts.`、`svc:cfg/secret:ts.`、`listen:ts`、スタック 64 KiB。
- B の形にするなら、`net` 側に新しい許可（アプリがリンクを出す）が要る。

## 5. 段取り（案）

| 段 | 内容 | 終わりの条件 | 試験 |
|---|---|---|---|
| **T0** 部品と速さ | ワークスペースに暗号の crate を入れる（版を simple-matter とそろえる）。`tsbench` アプリ: X25519、ChaCha20-Poly1305、BLAKE2s、NaCl box、HKDF の速さと、RFC 7748、RFC 8439、BLAKE2 の試験ベクタ。`ts` のヒープの仕組みを決める | 速さの表。ヒープを使う小さな試験が動く | ホストの `cargo test`（同じコードを x86 で）。t5emu に入れる（ベクタの照合）。実機は利用者の確認の後（`mtbench` と同じ形） |
| **T1** 制御の平面 | Noise IK（ts2021）、HTTP/1.1 の Upgrade、最小の HTTP/2、EarlyNoise、register（auth key）、`Stream=false` の map を 1 回。netmap から自分のアドレス、相手、DERP の地図を出す（`ts.registered`、`ts.netmap` のイベント） | 管理画面（tailscale.com か headscale）に Tab5 が 100.x のアドレスで出る | ホスト: Noise の部分を Go の `controlbase` の試験と同じ入力で照合（T0 と同じく x86 で）。t5emu: ホストの相手（`emu/src/hostpeer.rs`）に、Rust（std）で書いた偽の制御サーバ（snow と h2 の crate でよい）を足し、決まった netmap を返す。実機: 利用者の決めたサーバへ |
| **T2** DERP と流れる map | TLS 1.3（embedded-tls）、DERP の接続（ServerKey、ClientInfo、ServerInfo、NotePreferred、KeepAlive）。`Stream=true`、`KeepAlive=true` の map を開けたままにし、差分を当てる。切れたらつなぎ直す | home の DERP につながったまま、相手の増減が netmap に出る | t5emu: 偽の DERP（ホストの相手の中、試験では平文でも可）。偽の制御サーバが相手を足す・消す。実機: 公開の DERP か自前の DERP |
| **T3** DERP 越しの WireGuard | WireGuard（initiator と responder、REKEY、keepalive）、`ts` の中の smoltcp（形 A）、ICMP の echo と UDP の echo、パケットフィルタ、DERP 越しの disco の pong、`ControlTime` で時刻を合わせる | tailnet の PC から `ping 100.x.y.z` と `tailscale ping` が通る。Tab5 から相手へ UDP が届く | t5emu: ホストの相手に、boringtun（std）か tailscale-rs の WireGuard の部品で相手役を作り、偽の DERP 越しに ping を往復。実機: tailnet の PC から |
| **T4** アプリから使う | `svc:ts`（TCP、UDP、名前の解決は netmap から）。例: tailnet の HTTP から `ota`（doc/67）、状態のページ。shell の `ts` コマンド（status、peers、ping） | ほかのアプリが tailnet の相手と TCP で話せる | t5emu: `ota app http://<偽の相手の 100.x>/...` が入る（p4emu-run-37 の形）。実機 |
| **T5** 直の経路 | STUN、disco（ping、pong、CallMeMaybe）、UDP 41641、経路の選択と DERP への戻り | 同じ LAN の相手と直の経路（`tailscale ping` が `via <IP>:41641`） | t5emu: 偽の相手が UDP でも答える。実機: 同じ LAN と別の網 |
| **T6** 広げる（要るものだけ） | 対話のログイン（`AuthURL` を QR で画面に）、MagicDNS（100.100.100.100）、node key の入れ替え、形 B（`net` のリンク `ts0`、subnet router）、ephemeral の node | 利用者が選んだもの | それぞれ |

- t5emu の試験は、外の網を使わずに通る形にする（`make test-p4emu` は決まった結果を出す、doc/11）。偽の制御サーバと偽の DERP はホストの相手の中に置く。本物との相互接続は、ホストで headscale と Tailscale のクライアントを動かす形の別の試験にする（ホストに入れるのは利用者の確認の後）。
- ホストで先に核を試す: 核を sans-IO の crate（`user/tsport` のような形）にすれば、x86 で tailscale-rs（std）を相手に動かして確かめられる（推測。tailscale-rs の API は読んでいない）。

## 6. 危ないところ
- **HTTP/2 と JSON の大きさ**: 一番手間がかかる。tailnet が大きいと map が MB 単位になる。上限を決め、超えたら止まって知らせる。
- **プロトコルの変化**: 制御のプロトコルは capability version（今 150）で変わり続ける。古い版を名乗れば当面は通る（推測）が、Tailscale がいつまで古い版を受けるかは分からない。
- **DERP の TLS**: P-384 の検証、16 KiB の受信の枠、証明書のつなぎの置き場。embedded-tls を組んで確かめるまで、大きさと速さは推測。
- **秘密の置き場**: SD の平文（§4.5）。
- **tailnet の中の守り**: tailnet の ACL は信じられるが、Tab5 の側でも、`ts` で開ける口を grant で絞る（doc/52）。shell を tailnet に出すのは別に考える。
- **C6 と BLE**: Wi-Fi は C6 で BLE と無線を分け合う（doc/49 §10）。DERP の keepalive は軽いので影響は小さい見込み（推測）。

## 7. 調べたもの
- Tailscale（Go、main、2026-11-10）: `control/controlbase/handshake.go`、`conn.go`、`control/controlhttp/client.go`、`constants.go`、`controlhttpcommon/controlhttpcommon.go`、`control/ts2021/client.go`、`conn.go`、`control/tsp/tsp.go`、`register.go`、`map.go`、`tailcfg/tailcfg.go`、`tailcfg/derpmap.go`、`derp/derp.go`、`derp/derphttp/derphttp_client.go`（https://github.com/tailscale/tailscale）
- 実際の応答（2026-11-10）: `http://controlplane.tailscale.com/key?v=150`（302）、`https://controlplane.tailscale.com/key?v=150`（`publicKey` と `legacyPublicKey`）、`http://controlplane.tailscale.com/ts2021`（400）、`https://login.tailscale.com/derpmap/default`（28 地域、15,974 B）、`derp7e.tailscale.com:443` と `controlplane.tailscale.com:443` の TLS（openssl s_client）
- tailscale-rs a2a56c4: README、Cargo.toml、各 crate の `lib.rs` と依存、issue #16、#321、#437、#455（https://github.com/tailscale/tailscale-rs）
- tailscale-esp32: README、`docs/protocol-support.md`、Cargo.toml（https://github.com/0xdilo/tailscale-esp32）
- MicroLink: README（https://github.com/CamM2325/microlink）
- boringtun: `boringtun/Cargo.toml`、`src/lib.rs`（https://github.com/cloudflare/boringtun）
- embedded-tls: README（https://github.com/embassy-rs/embedded-tls）
- WireGuard の仕様: https://www.wireguard.com/protocol/
- crates.io の版とライセンス（2026-11-10）: boringtun、gotatun、snow、clatter、x25519-dalek、chacha20poly1305、blake2、crypto_box、h2、embedded-tls、rustls、serde-json-core、picojson、tailscale、ts_*
- headscale: GitHub のリリース（v0.29.7）とライセンス。設定と文書は読んでいない。
- 読んでいないもの: Go の `wgengine/magicsock`（DERP だけのときの相手の動き）、headscale の設定、tailscale-rs の公開 API。

## 8. 利用者への問い
1. **制御サーバ**: tailscale.com か、headscale（自前）か。試験は headscale、本番は tailscale.com、という分け方もある。
2. **使い道**: どちらが先か。
   - tailnet から Tab5 に届く（ping、状態のページ、ログ、OTA を入れる、Matter の操作を遠くから）。
   - Tab5 から tailnet の機器に届く（家の Home Assistant、tailnet の上の更新サーバ）。
   - Tab5 を subnet router にする（試験用 LAN を tailnet に出す。形 B が要る）。
3. **登録のしかた**: auth key（設定に入れる、最初の版はこちらの案）か、対話のログイン（画面に URL か QR）か。auth key なら、使い捨てか再利用か、ephemeral（切れたら消える node）にするか、タグを付けるか。
4. **DERP の TLS**: 最初の版で、(a) 証明書を確かめる（P-384 の検証を入れる）、(b) 確かめない（中身は WireGuard で守られる。§3.4 の危なさを受け入れる）、(c) 自前の DERP を LAN に置いて試す、のどれにするか。
5. **形 A と B**: 最初は `ts` の中に IP スタック（形 A）でよいか。
6. **アプリのヒープ**: `ts` に初めてアロケータを入れてよいか。入れるなら、ほかのアプリにも広げる前提か、`ts` の中だけか。
7. **依存の取り方**: tailscale-rs の一部（`ts_keys`、`ts_disco_protocol`、`ts_noise`、`ts_tunnel`）を third_party に置いて直すか、読んで自分で書くか。ライセンスはどれも BSD-3-Clause で、プロジェクト（MIT OR Apache-2.0）と組み合わせられる。
8. **秘密の置き場**: node key を SD の平文に置いてよいか（Wi-Fi の passphrase と同じ扱い）。
9. **試験の相手**: ホストに headscale と Tailscale のクライアントを入れて相互接続を試してよいか（今回は入れていない）。実機の試験は今までどおり確認の後に行う。

## 9. 実装: 段 T1（2026-11-11）
利用者の指示（2026-11-11）: 「tab5_ssh_client に ESP32 向けの実装があるので、これを参考に実装してください」。その名のリポジトリはこの PC になく、Tab5 向けの Tailscale のクライアント（C、約 6,000 行）を持つ `~/repos/tab5_claude_client`（remote は portable_terminal）の `components/tailscale` を参考にした。§8 の問いは参考の実装の選び方に合わせた。
- 制御サーバは既定で tailscale.com（`ts.server` で headscale にも変えられる）。
- 登録は auth key（`ts.authkey`）。なければ login の URL を出す。
- 制御サーバにも DERP にも TLS を使い、証明書は確かめる（参考は ESP-IDF の証明書の束）。
- 直の経路（disco）は後の段で。参考も持つ。

### 9.1 作ったもの
- `user/tsclient`（no_std と alloc、I/O を持たない）:
  - `keys`: machine、node、disco の鍵。`nodekey:` と 16 進。
  - `noise`: ts2021 の IK とレコード（参考の `tailscale_noise.c` と同じ手順）。
  - `h2`: HTTP/2 の最小のクライアント。
  - `control`: `/key`、`/ts2021` の Upgrade、`/machine/register`、`/machine/map` の流れ。
  - `netmap`: serde で必要な欄だけ。`Peers`、`PeersChanged`、`PeersRemoved`、`DERPMap`。
  - `tls`: embedded-tls に ISRG Root X2 を渡す。
- 参考と変えたところ:
  - Noise を「レコードの列」ではなくバイトの流れとして扱う。1 つのレコードの平文は最大 4077 バイト（Go の controlbase と同じ）。参考は HTTP/2 のフレーム 1 つを 1 レコードに入れ、4096 バイトの上限を超えうる。
  - HTTP/2 の受けの窓を 16 MiB に広げ、読んだ分を WINDOW_UPDATE で返す。参考は窓を 64 KiB のまま返さないので、流れる map が 64 KiB を超えると止まる（推測。参考の利用者の tailnet が小さかったので出なかったのだろう）。
  - 受けるフレームは 16 KiB まで（HTTP/2 の既定）。参考は 4 KiB を超えると切る。
- `third_party/embedded-tls-0.19.0`（`src/pki.rs`）:
  - 証明書の連なりを、信頼する根が署名した最後の証明書から確かめる。Let's Encrypt の ECDSA の連なりは X2 の後ろに「X1 が署名した X2」（RSA）を付け、Tailscale の DERP はさらに自己署名の ed25519 の証明書を付ける。元のままでは末尾から確かめて落ちる（2026-11-11 に `openssl s_client` で確認）。
  - 信頼する根を複数持てるようにした（`with_more_roots`）。同じ日のうちに、Tailscale のサーバ（制御も DERP も）が ECDSA の連なり（ISRG Root X2 へ）から RSA の連なり（ISRG Root X1 へ）に変わった。RSA の連なりのとき、ECDSA だけを申し出るとサーバは握手を断る（HandshakeFailure）。それで embedded-tls の `rsa` 機能を入れ、X1 と X2 の両方を信頼する。
  - 時計がないので、有効期限は見ない。
- `user/apps/ts`（system の鍵、`svc:net/tcp-connect`、`svc:net/resolve`、`random`、`svc:cfg/r|w|secret:ts.`）:
  - 1 MiB のヒープ（linked_list_allocator）とスタック 256 KiB。プロセスは全部で 2 MiB まで（4 MiB のヒープはローダに断られた）。スタックが 64 KiB のときは、TLS の握手（RSA の確かめを含む）がスタックを越えてヒープを壊し、アロケータの中で load access fault になった。
  - 乱数は kernel の `random` で種を付けた ChaCha20。
  - 鍵は初回に作り、秘密の設定 `ts.keys` に置く。
  - 失敗したら 30 秒後にやり直す。
  - イベント: `ts.start`、`ts.server_key`、`ts.noise`、`ts.registered`、`ts.login`（URL）、`ts.map`（自分のアドレス、相手の数、DERP の領域の数、home）、`ts.failed`。
  - .text は 470 KB ほど（RSA を入れて約 60 KB 増えた）。像は 515 KB。
  - アプリが alloc を使うので、`user/.cargo/config.toml` の build-std に `alloc` を足した。
  - `ts` は別の cargo の呼び出しで作る（`HEAP_APPS`、`target/heap`）。同じ呼び出しで作ると、`rsa` が入れる `zeroize/alloc` などの機能がほかのアプリにも及び、アロケータのない `cfg` がリンクできなかった。
  - エミュレータのストアの像には入れない。0.5 MB を入れると、p4emu-run-35 の散らしたストアに空きが足りなくなった。
- `tools/tsprobe`: 同じ `tsclient` を、ホストの TCP と embedded-tls で制御サーバに向ける試験の道具。
- 試験: `cargo test -p tsclient`（ホスト、8 つ）。
  - Noise の IK を、試験の中に書いた応答側と突き合わせる。サーバ鍵が違えば落ちる。
  - RFC 7748 のベクタ、base64、HPACK の整数と `:status`、netmap の差分の当て方、HTTP/1.1 の本文（長さ、chunked、閉じるまで）。

### 9.2 本物の制御サーバで（2026-11-11）
- 利用者の tailnet は Tailnet Lock が有効。新しい node は、登録の URL でログインした後、署名できる端末で `tailscale lock sign nodekey:…` が要る。`ts.start` は node key を出す。
- ホスト（`tsprobe`）: TLS、`/key`、Noise、HTTP/2、register（login の URL）。承認と署名の後は `authorized=true`。最初の map は 32,643 バイトで、自分の 100.x のアドレス、相手 11、DERP の領域 28（home は 1、derp1j）。その後は KeepAlive（18 バイト）。
- Tab5（`run ts`）:
  - TLS の握手（RSA の連なり）と `/key` は約 1.4 秒、Noise と HTTP/2 はさらに約 1.6 秒。
  - 承認と署名の後、最初の map は 33,900 バイトで、相手 12、領域 28。差分（1,302 バイト）も当たった。
  - 待つ間の CPU は 4‰（`Net::recv` が 1 ms ごとに見に行く）。
- まだ:
  - 相手の node key の Tailnet Lock の署名を確かめていない。今は制御サーバが渡す相手をそのまま信じる。
  - エミュレータの試験（偽の制御サーバ）は T2 と合わせて作る。
  - `Net::recv` は 1 ms ごとに見に行く形で、map を待つ間も CPU を少し使う。

