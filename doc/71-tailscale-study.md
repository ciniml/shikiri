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

## 10. 段 T2・T3: DERP と、DERP 越しの WireGuard（2026-11-11）
利用者の指示（2026-11-11）: 「すすめて」（T2 と T3）。

### 10.1 作ったもの
- 1 つのプロセスで、制御の接続と DERP の接続の両方に付き合う:
  - `tsclient::stream::Buffered`: TLS の下に置く読みのバッファ。いつもレコード 1 つ分（16 KiB）の場所で読むので、TLS は復号した残りを持たない。embedded-tls はレコードの頭と本体をちょうどの長さで読み、先読みしない（`src/record_reader.rs` で確認）。
  - そのため、待っている読みは「こちらのバッファ（Buffered、Noise、map の流れ）」か「TCP の受けの列」のどちらかにある。
  - `ts` の輪: どちらかの接続に読むものがあるとき（`svc:net` の `TCP_STATE` の待ちバイト数、または接続が切れた）だけ、その接続のフレームを 1 つ読む。HTTP/2 は `H2::step`（1 フレーム）、map の流れは `MapStream::poll`（1 フレーム）、DERP は `Derp::read`（1 フレーム）。何もなければ眠る。眠りは 1 ms から倍にして 20 ms まで。
  - 残る危うさ: TLS の NewSessionTicket だけのレコードが来ると、embedded-tls はアプリのデータが来るまで待つ。今のところ起きていない。
- DERP（`tsclient::derp`、参考の `tailscale_derp.c` と同じ手順）:
  - `GET /derp` を Upgrade し、ServerKey を受ける。
  - ClientInfo（node key と、NaCl box に入れた `{"version":2}`。box は crypto_box）を送り、ServerInfo を受け、NotePreferred を送る。
  - サーバの Ping には Pong を返す。
- home の DERP: 最初の map の DERP の地図から選ぶ（参考と同じく、番号の一番小さい領域。近さでは選ばない）。そのあと `Stream: false`、`OmitPeers: true` の map（「lite」の更新）で `Hostinfo.NetInfo.PreferredDERP` を制御サーバに伝える。応答は読まない（map の流れは、ほかの stream のフレームを飛ばす）。
- WireGuard（`tsclient::wg`、白書の §5）:
  - 応答側だけ。始める側は TAI64N の時刻が要る（§4.7）ので後で。
  - 初めのメッセージの mac1、IKpsk2（事前の共有鍵は 0）、TAI64N の巻き戻しの拒否、応答。
  - transport: 受けは 64 の窓で重複を拒み、送りは 16 バイトに詰める。セッションは 180 秒まで。
  - DERP の送り主の node key と、握手の中の static 鍵が同じでなければ捨てる。中の IPv4 の送り元が相手のアドレスでなければ捨てる。
- `tsclient::ip`: 自分の 100.x への ICMP の echo に答える。`tsclient::node::Node`: netmap と WireGuard をまとめ、DERP から来たものに返すもの（握手の応答、echo の応答）を作る。disco（`TS💬`）は数えるだけ（T5）。
- `tsclient::tls`: 証明書の連なりを持つ場所を 8 KiB にした。DERP は 5 枚（最後に自己署名の ed25519）で 4 KiB を超え、embedded-tls が OutOfMemory を返した。
- 試験（ホスト、12）: WireGuard の握手と transport を、白書のとおりに書いた始める側と突き合わせる。重複の窓、echo の応答、map の相手が WireGuard に届くこと。

### 10.2 Tab5 での確認
- 1 回目: DERP の TLS が落ちた（上の 8 KiB）。ホストの `tsprobe --tls HOST` で同じ失敗を出して直した。
- 2 回目: 握手の初めが 5 秒ごとに届くが、すべて「unknown peer」。`NetMap::apply_json` が応答から相手の一覧を取り出して返すので、`Node` が相手を WireGuard に渡していなかった。`NetMap::peers_version` で変わったかを見る形に直し、試験を足した。
- 3 回目: このホスト（tailnet の一員）から Tab5 の 100.x へ `ping` が 10 回中 10 回返った（往復 322〜695 ms、平均 361 ms。home は nyc の derp1j。ホストの home は tok）。Tab5 は握手 1、受けたパケット 11、echo の応答 10。ホストの `tailscale status` に LastHandshake が出た。
- CPU（眠りを 2 ms に固定していたとき）: `ts` 49‰、`net` 193‰（それまでの約 115‰ から増えた。`TCP_STATE` を 2 ms ごとに 2 回呼ぶため）。眠りを 20 ms まで延ばす形にして `ts` 27‰、`net` 132‰（ping を含む 68 秒）。ping は 10 回中 9 回（最初の 1 回は握手の間に落ちたと見ている）。
- まだ:
  - home の DERP を近さで選ぶこと。日本からは tok の方が近い。
  - disco に答えて直の経路を作ること（T5）。
  - こちらから始める WireGuard（時刻が要る）。
  - TCP と UDP（T4、smoltcp）。
  - `net` に TCP の読める知らせを足し、見に行かずに待つこと。
  - 切れたら全体を 30 秒後にやり直す（制御と DERP を別々につなぎ直さない）。
  - Tailnet Lock の署名の確かめ。


## 11. 近い DERP を選ぶ（2026-11-11）
参考の実装は、番号のいちばん小さい領域（nyc）を home にしていた。日本からは往復が 300 ms を超える。Tailscale の netcheck と同じく、STUN の往復時間で選ぶ形にした。

### 11.1 作ったもの
- `tsclient::stun`: STUN の binding request と、その応答。Tailscale の DERP は、SOFTWARE が `tailnode` で FINGERPRINT（CRC-32 と 0x5354554E の XOR）の付いた要求にしか答えない（tailscale.com/net/stun の `ParseBindingRequest`）。応答の XOR-MAPPED-ADDRESS（なければ MAPPED-ADDRESS）から、インターネットから見たこの端末のアドレスがわかる。
- `tsclient::netcheck`: 中継をする node がある領域ごとに、IPv4 と STUN のある node を 1 つ選ぶ（STUNPort が -1 なら STUN はない、0 なら 3478）。要求を出すたびに新しいトランザクション ID を付け、領域ごとにいちばん速い往復を残す。I/O は持たない。
- `NetMap::derp_region(id)`。`control::update` は `Hostinfo.NetInfo.DERPLatency`（`"7-v4": 秒`）も送る。
- アプリ `ts`: 最初の map の後、UDP 41641（Tailscale のいつものポート。後で直の WireGuard にも使う）から全領域へ 700 ms を 3 回送り、いちばん速い領域につなぐ。答えがなければ前と同じく番号のいちばん小さい領域。イベントは `ts.netcheck`（領域、答えた数、選んだ領域と ms、外から見たアドレス）。許可に `svc:net/udp:41641` を足した。
- `tsprobe --netcheck DERPMAP.json`: ホストで領域ごとの往復を並べる（DERP の地図は https://login.tailscale.com/derpmap/default から取れる）。

### 11.2 確かめたこと
- ホスト: 28 領域中 27 が答えた（STUN のない 1 つを除く）。tok 7 ms、hkg 69 ms、sin 74 ms、…、nyc 158 ms、sao 356 ms。
- Tab5: `ts.netcheck regions=28 answered=27 best=7 ms=17`。DERP は derp7g（tok）につながった。ホストから Tab5 への ping は 10 回中 10 回返った（往復 18〜98 ms、平均 32 ms。nyc 経由では平均 361 ms）。
- この試験の前に、別のセッションが Tab5 の ota_0 を ESP32-S3 のアプリで上書きしていた（04:41 JST ごろ）。`make flash-p4` で焼き直してから試した。ストアは無事（60 個）。
- `cargo test -p tsclient` は 14（stun と netcheck を足した）。

## 12. 段 T4a: ts の中の IP（smoltcp）と状態のページ（2026-11-11）
T4 を 2 つに分けた。T4a は `ts` の中に IP スタックを置き（§3 の形 A）、tailnet の相手と TCP で話せるようにする。T4b は、ほかのアプリが使う `svc:ts` と、こちらから始める WireGuard。

### 12.1 作ったもの
- `tsclient::tun`: smoltcp 0.14（`medium-ip`、`proto-ipv4`、`socket-tcp`、`socket-udp`、`auto-icmp-echo-reply`）を、受けと送りの 2 つの列の device に載せる。アドレスは自分の 100.x を /10（100.64.0.0/10）で持つので、tailnet の相手はみなリンクの上にあり、経路は要らない。MTU は Tailscale と同じ 1280。
- `Node`: WireGuard が開いたパケットは smoltcp の受けの列へ入れる。`Node::poll` は smoltcp を回し、smoltcp が出したパケットを宛先のアドレスで相手を選んで暗号化し、（相手の node key、メッセージ）の列を返す。呼ぶ側が DERP で送る。WireGuard の session がない相手へのパケットは捨てる（`no session to destination`。こちらから握手を始めるのは T4b）。ping は smoltcp が自分で答える（`ip::echo_reply` はやめた）。統計に `packets_out`。
- アプリ `ts`: TCP 80 番で状態のページ（text/plain: ホスト名、tailnet のアドレス、DERP、動いた時間、統計、相手の名前と home の DERP と online）。一度に 1 本、要求の頭（空行まで、2 KiB まで）を受けたら返して閉じ、閉じ終われば聞き直す。`ts.stats` に `packets_out` と `web`。
- `tsclient` は `smoltcp` を `pub use` する（アプリと版をそろえる）。

### 12.2 確かめたこと
- `cargo test -p tsclient`: smoltcp の試験（ping への答えと、80 番への SYN に SYN-ACK）を足した。`ip` の試験は消えたので数は 14 のまま。
- Tab5（ts のメモリは 1.6 MB、2 MiB の内）: ホストから ping 5 回中 5 回（平均 48 ms）。`curl http://<Tab5 の 100.x>/` が続けて 6 回とも 200（418 B、1 回 55〜100 ms）。`ts.stats` は dropped=0。
- まだ: ほかのアプリから使う口（`svc:ts`）、こちらから始める WireGuard（時刻は map の `ControlTime` から取る）、UDP、tailnet の名前の解決。

## 13. 段 T4b: `svc:ts` と、こちらから始める WireGuard（2026-11-11）

### 13.1 作ったもの
- `wg::Device::initiate`（握手の始め、148 バイト）と応答の受け取り（`Received::Established`）。こちらが始めた session は 120 秒（REKEY_AFTER_TIME）で次の握手を始め、答えのない始めは 5 秒（REKEY_TIMEOUT）で出し直す。相手が始めた session は 180 秒まで使う（次は相手が始める）。`can_send`、`wants_handshake`。試験は Device どうしの握手と往復（応答側は §10 で白書と照合済み）。
- 時刻: map の `ControlTime`（RFC 3339）を `netmap::unix_ms` で読み、`Node::tai64n` が WireGuard の TAI64N（2^62 + 10 + 秒、ナノ秒）にする。tailscale.com は最初の応答に `ControlTime` を付けていた（`ts.map clock=true`）。
- `Node`: 送るものは `poll` がまとめて返す（握手の応答、始め、transport）。session のない相手へのパケットは握手を始めて待たせ（全体で 16 まで）、`Established` で送る。`from_derp` は何も返さない形にした。デバッグ用に、出入りするパケットを見る `tap`（`ts` の `TRACE_TCP` で `ts.tcp` を出す）。
- `svc:ts`（`user/apps/ts/src/svc.rs`）: `svc:net` と同じ要求の形（`t5_abi::net`）。TCP_CONNECT、TCP_LISTEN（tailnet のアドレスで。アドレスとリンクは見ない）、TCP_STATE、SEND、RECV、CLOSE、RESOLVE と RESOLVE_RESULT（netmap の名前: 短い名前か全体の名前、IPv4）。ほかは BAD_REQUEST。grant は `svc:net` と同じ形（`svc:ts/tcp-connect`、`tcp-connect:PORT`、`tcp-listen:PORT`、`tcp-listen:any`、`resolve`）で、拒んだら `ts.denied`。客は 2 つ（64 KiB の slice ずつ）、ソケットは全体で 8（送受 4 KiB ずつ）。客が閉じたことは `wait` の PEER_CLOSED で知る（read は Busy のまま）。新しい session（失敗の後）では、前のソケット番号は NOT_CONNECTED。ts の眠りは `wait`（客の要求で起きる）。許可に `shm` と `listen:ts`。
- rt: `Net::connect_to(SERVICE)`（`Net::connect` は `net`）。
- 試験アプリ `tsget`: `run tsget IP PORT` で `svc:ts` から HTTP/1.0 の `GET /` を 1 回、`tsget.done`（状態、バイト、ms）。

### 13.2 確かめたこと
- `cargo test -p tsclient`: 15（Device どうしの握手、ControlTime から TAI64N）。
- Tab5: ホストの tailnet のアドレスに立てた `python3 -m http.server` へ `run tsget <ホストの 100.x> 28080`。こちらからの握手に 40 ms で応答が来て、200、4,388 バイト（168 ms）。103,390 バイトは 1,144 ms（約 90 KB/s、dropped=0）。閉じたポートでは RST で `establish` の失敗になる。
- はじめはポート 8000 で試し、つながった直後にデータなしで FIN が返った。`ts.tcp` のトレースで TCP の流れは正しく、ホストの 0.0.0.0:8000 で別のもの（docker らしい）が待っていて、tailnet からの 8000 番がそちらへ行っていた（ホストの curl も python のログに出ていなかった）。Tab5 の側の問題ではなかった。
- 試験の途中で、制御の接続が 1 回切れた（`ts.failed step=map stream error=io`）。30 秒後につなぎ直した。原因はまだ見ていない（それまでの版は 1 時間以上切れなかった）。
- まだ: `ota` を `svc:ts` で使う（100.64.0.0/10 か `.ts.net` の名前なら `ts` につなぐ）、UDP、shell の `ts` コマンド、相手の home が別の DERP 領域のとき（こちらは自分の home の DERP にしか送らない。相手がそこにいなければ届かない）。

## 14. 段 T5（最小）: 直の経路と、ifconfig の ts0（2026-11-11）

### 14.1 作ったもの
- `tsclient::disco`: disco のメッセージ（`TS💬`、送り手の disco key、nonce、NaCl box）。Ping（トランザクション ID と node key）、Pong（トランザクション ID と、Ping の来たアドレス）、CallMeMaybe（endpoint の列）を作って開く。
- `Node`:
  - 送り先を `Dest::Derp(node key)` か `Dest::Udp(IPv4, port)` で返す。
  - disco の Ping には、来た道で Pong を返す。UDP なら UDP で、そのアドレスを入れる。DERP なら DERP で、`127.3.3.40:領域` を入れる。
  - CallMeMaybe には、相手の各 endpoint へ UDP で Ping を送る（NAT に穴を開ける）。
  - WireGuard を UDP でも受ける（`wg::Device::receive` の送り手は `Option`。始めの中の鍵で相手を決める）。認証の通ったメッセージが UDP で来た相手には、15 秒の間そのアドレスへ UDP で送り、来なくなれば DERP に戻る。
  - 統計に `udp_in`、`udp_out`、`pongs`。
- アプリ `ts`:
  - UDP 41641 を netcheck の後も開けたままにし、毎回の巡回で 1 つ読む。
  - endpoint（各インタフェースの IPv4 と STUN で見えたアドレス、ポート 41641）を lite の map で伝える（`ts.endpoints`）。
  - 直の相手の数が変われば `ts.direct`。許可に `svc:net/info`。
- `svc:ts` の IF_INFO: インタフェースは 1 つで、`ts0`。tailnet のアドレス（/32）と、出入りしたパケットの数を返す。grant は `info`。
- shell の `ifconfig` は `svc:net` の後に `svc:ts` にも聞き、`ts0` を出す。ts が動いていなければ、つなぎがすぐ失敗するので出ない（カーネルは聞き手のない名前を `Off` で返す）。shell の許可に `svc:ts/info`。
- ts は今も形 A（IP スタックを ts の中に持つ）。`ts0` は `net` のリンクではないので、ほかのアプリの `svc:net` からは tailnet に出られない。

### 14.2 確かめたこと
- `cargo test -p tsclient`: 16。disco の往復を足し、Node の試験で UDP の Ping に UDP で Pong が返ることを見た。
- Tab5（Wi-Fi の 192.168.8.x。ホストとは同じ外のアドレスで、その内側にもう 1 段 NAT がある）:
  - ホストの `tailscale ping` は、はじめ DERP（tok）経由で Pong が返った。その後 `direct 192.168.2.129:41641` になった（Tab5 の手前のルータの外側のアドレス）。
  - ICMP の ping は 10 回中 10 回返り、往復は最短 5.4 ms・平均 20 ms（DERP 経由では最短 17 ms）。
  - 状態のページは 1 回 10〜30 ms。
  - 相手が止まると、15 秒で DERP に戻る（`ts.direct peers=0`）。
- 制御の接続が切れる件（§13.2）:
  - 前の版では 155 秒で切れた。直の経路の版は 850 秒切れなかった（観察を続ける）。
  - 切れたときの TCP の状態と、最後の map からの時間を `ts.ctl_lost` に出すようにした。
- まだ: IPv6 の endpoint、NAT の対応表を保つための Ping。

### 14.3 その後
- こちらからも直の経路を探す。DERP で送る相手には、5 秒に 1 回まで（`PROBE_MS`）次の 2 つを送る。
  - 相手の IPv4 の endpoint へ、UDP で disco の Ping（こちらの node key 付き）。
  - DERP で、こちらの endpoint を並べた CallMeMaybe。
- こちらの Ping への Pong が UDP で来れば、その送り元を相手の経路にする（disco の box で相手の鍵が確かめられている）。これで、相手の home が別の DERP 領域でも、直に届けば話せる。Node の試験で確かめた。
- UDP の読み取り: 毎回の巡回では聞かない。何かあった直後か、眠りが `net` の知らせ（UDP_WAIT、`udp_arm` の後の `wait`）で終わったときだけ読む。
- `ota`: URL のホストが 100.64.0.0/10 のアドレスか `.ts.net` の名前なら、`svc:ts` でつなぐ。カーネルの確かめ（doc/67 §5）も同じ。tailnet から入れたカーネルは、起動のときに `ts` が動いていなければ確かめが済まない（`START.CFG` に `ts` を入れる）。許可に `svc:ts/tcp-connect`、`svc:ts/resolve`。
- 実機（2026-11-11）:
  - 前の版（§14.1）を動かしていた途中、1,057 秒で Tab5 が固まった。ping も JTAG の attach も効かなくなり、利用者がリセットした。原因はまだ分からない（最後のログは wlan.stats）。同じハブの別の機器が、つながり損ね続けていた（error -71）。リセットの後は、ハブごと何度か付き直してから安定した。
  - 新しい版:
    - ホストの `tailscale ping` は DERP の後に `via 192.168.2.129:41641` になった。
    - 双方の直の経路が切れてから Tab5 の `tsget` でホストの HTTP を取ると、取得の間は直の経路になった（`ts.direct peers=1`）。103,390 バイトを 499 ms で受けた（DERP 経由では 1,144 ms）。
  - `ota app http://<ホストの 100.x>:28080/greeter.t5a`（利用者が Tab5 のターミナルで打った）で、`ota.installed` が出た（6,860 バイト）。サーバのログに Tab5 の 100.x からの GET が出た。ota は自動起動の印付きの版を入れた。
  - shell の `ifconfig` は動いた（`shell.listed`）。`ts0` の表示は利用者の画面でしか見られない。
- 長い観察（新しい版）: 約 3,900 秒、`ts.ctl_lost` は 0 回で、固まりもなかった。その後も、入れ直しの時点で約 45,900 秒（12.7 時間）動いていた。直の経路は、通信のない間は 15 秒で切れ、次の通信でまた探す（`ts.direct` の 1 と 0 の行き来）。

### 14.4 ts の自動起動（2026-11-12）
- init の組み込みの一覧（`run init 1`）と画面の一覧（`run init 3`）の最後に `ts` を足した。
- `run ts MODE`:
  - 0（`START.CFG` から）: `ts.keys` か `ts.authkey` があるときだけ動く。どちらもなければ `ts.off` を出して 0 で終わる（init は 0 の終わりを起こし直さない）。tailnet に入ったことのない機器が、起動のたびに制御サーバへ登録しに行かないようにするため。
  - 1: いつでも動く。初めてなら鍵を作る（初めて tailnet に入るときはこちら）。
- 失敗の後の待ちは 2 秒から倍にしていき、30 秒まで。1 回の接続が 60 秒より長く続いたら 2 秒に戻す。起動の直後は `net` にまだアドレスがなく、名前引きが失敗するため（`ts.failed step=resolve`）。前は 30 秒待っていた。
- 実機: `run init 1` で `START.CFG` を書き直し、リセットした。
  - ts は init から起動し、名前引きに 2 回失敗（2 秒、4 秒待ち）した後、起動から 21 秒で map を受けた（前の版では 41 秒）。
  - ホストの `tailscale ping` は DERP の後に `via 192.168.2.129:41641` になった。

### 14.5 直の経路を保つ Ping（2026-11-12）
- Tailscale の heartbeat にならった。パケットを送るか受けるかした相手を、45 秒の間「通信中」とする（`ACTIVE_MS`、Tailscale の sessionActiveTimeout）。通信中の相手の直の経路へは、3 秒ごとに disco の Ping を UDP で送る（`HEARTBEAT_MS`、heartbeatInterval）。その Pong で経路の期限（15 秒）が延び、途中の NAT も対応表を保つ。
- 通信のない相手には送らない。経路は 15 秒で切れ、次の通信でまた探す（§14.3）。
- 経路探し（§14.3）の Ping と同じ関数（`Node::ping`）で送る。数は `Stats::heartbeats`（`ts.direct` の `heartbeats`）。
- Node の試験: 通信の後は 3 秒ごとに Ping、その Pong で 15 秒を過ぎても直のまま、通信が途絶えて 45 秒たつと Ping は止まり、経路が切れる。`cargo test -p tsclient` の 16 が通った。
- 実機: ホストから 20 秒おきに ICMP の ping を 6 回送った。Tab5 はその間ずっと直の経路のままだった（`ts.direct peers=1` の後、0 に戻らない）。往復は 1 回目を除き 3〜6 ms。前の版なら、20 秒の間に 15 秒の期限が切れて DERP に戻っていた。

### 14.6 `svc:ts` の UDP（2026-11-12）
- `svc:net` と同じ要求の形で UDP_BIND・UDP_SEND・UDP_RECV・UDP_WAIT・CLOSE を足した。ソケットは smoltcp の UDP（片道 4 個・4 KiB）で、TCP と合わせて 8 個まで。
  - UDP_BIND: アドレスとリンクは見ない。port 0 なら空いている番号（49152 から）を選び、v1 で返す。ほかの客が同じ番号を使っていれば ADDR_IN_USE。
  - UDP_SEND: 送り先は tailnet の相手。列が一杯なら 0 バイト。
  - UDP_RECV: `UDP_HEADER` の後にデータ、送り元の port は slice の先頭、IPv4 は v1（`svc:net` と同じ）。
  - UDP_WAIT: データグラムが待っていればすぐ 1。なければ答えを残し、届いたときに 1 を送る。先に客の次の要求が来たら、その答えの前に 0 を送る（答えの順を保つ）。
- grant は `udp:PORT` か `udp:any`。
- 試験: `run tsget IP PORT+0x10000` は UDP の版。5 個のデータグラムをエコーサーバへ送り、それぞれ UDP_WAIT で待つ（`tsget.udp`）。tsget の許可に `svc:ts/udp:any`。
- 実機（ホストの 100.x の UDP エコー）:
  - 起動直後の 1 回目は 5 個中 3 個が返った（最長 226 ms）。サーバには 5 個とも届いていた。最初の握手と経路探しの最中で、どこで落ちたかは追っていない。落ちたのではなく遅れだった（§14.9）。
  - その後は 3 回とも 5 個中 5 個が返った。往復は 6〜12 ms。直の経路が切れた後（70 秒の間を空けた後）の 1 個目も最長 187 ms で返った。

### 14.7 黙って死んだ接続に気づく（2026-11-12）
- 起きたこと: 画面の件（doc/17 §13）で `--reset` を 30 回ほど繰り返した後、最後の起動の ts が tailnet から見えなくなった。ホストの `tailscale ping` は DERP 経由でも届かなかった。ts は待っているだけで、ログには何も出なかった（`ts.ctl_lost` も `ts.failed` もない）。ts を止めて起こし直すと、すぐ届いた（直の経路で 29 ms）。
  - 考えられる原因: 途中の NAT が TCP の対応を忘れた。DERP サーバに、前の起動の切れていない接続が残っていた。ほかの原因もありうる。どれかは確かめていない。
  - どの原因でも、ts からは何も送らないので、相手から来なくなったことに気づけなかった。
- 直し: 制御サーバ（map の `KeepAlive`）も DERP サーバ（keepalive のフレーム）も、ほぼ 1 分ごとに何かを送ってくる。どちらかから 180 秒何も来なければ（`SILENT_MS`）、`ts.silent`（それぞれ最後に来てからの時間）を出してつなぎ直す。
- 実機: 通信のない状態で約 400 秒、`ts.silent` は出なかった（誤って切らない）。死んだ接続はもう一度は起こせていないので、この直しで戻るところはまだ見ていない（→ §14.10 で、規則で黙らせて確かめた）。

### 14.8 shell の `ts`（2026-11-13）
- ts の状態（相手、経路）はログでしか見られなかった。shell から見られるようにした。
- `svc:ts` に要求 `PEER_INFO`（`t5_abi::net::op`、18。`svc:net` は `BAD_REQUEST`）。`a` = 0 がこのノード、1 から相手。答えの v1 は記録の長さ、最後より先は `NO_ADDRESS`。許可は `IF_INFO` と同じ `info`。
- 記録（`net::peer`、72 バイト）: 名前（最初の `.` まで、48）、IPv4、フラグ（`SELF`・`ONLINE`・`OFFLINE`・`DIRECT`）、直の経路のポートとアドレス、ホームの DERP の地域のコード（8）、パケットを最後に送ったか受けてからの秒数（なければ `u32::MAX`）。
- Node に `direct_path`（今使っている直の経路）と `last_active`（§14.5 の通信中の印）を足した。
- `t5_rt::Net::peer_info(k)`。shell の `ts` は 1 行に 1 ノード: 名前、アドレス、`self` / `direct` / `derp` / `offline`、直なら ADDR:PORT、そうでなければ DERP の地域、最後の通信からの秒数。ts が動いていなければ `ts: not running`。
- 実機: 利用者が Tab5 で `ts` を打った。ホストからの ping が止まっている間はホストの行が `derp` か `offline`（直の経路は 15 秒で切れる、§14.5）。2 秒おきの ping の間は `direct 192.168.2.x:41641` になった。`cargo test -p tsclient` の 16 が通った（`direct_path` と `last_active` を足した）。

### 14.9 起動直後の UDP の「取りこぼし」は遅れ（2026-11-13）
- §14.6 の「起動直後の 1 回目は 5 個中 3 個」を調べた。再起動して 40 秒後に `run tsget` を UDP で 1 回、を 16 回繰り返した。
- 落ちた 2 回（§14.6 の 1 回と、この日の 1 回目）でも、ts が受けた転送パケットの数は成功した回と同じだった（`packets=6`）。WireGuard で落としたのは、重複したハンドシェイクの応答 1 個だけ（`response unasked`、DERP と UDP の両方から届いた同じ応答の 2 つ目）。
- tsget は、自分の回の 2 秒を過ぎて届いた返事を、次の回で違う返事として捨てていた。遅れた返事も数え、その数を `late` に出すようにした。
- その後の 1 回で `back=5 late=2 max_us=5070767`: 0 回目と 1 回目の返事が送ってから約 5 秒後に届き、後から送った 2〜4 回目の返事は先に、すぐ（最短 25 ms）届いた。このときハンドシェイクは 1 回で成立していて（`initiations=1`）、ホストからのハンドシェイクは 20 秒後だった。
- Tab5 の中の受け取りの列（Node → smoltcp → svc:ts）は届いた順にしか進まず、ts の昼寝は最長 20 ms。なので、Tab5 の中で 0・1 回目だけを止めておくことはできない。遅れはホスト（tailscaled）か途中で起きていると判断した。ホストのログとパケットは権限がなく見られないので、ここで止めた（推測: ホストがまだ直の経路を確かめていない間に、最初の返事を別の経路に出した）。
- 16 回中、おかしかったのは 2 回（1 回目の 5 個中 3 個と、`late=2` の回）。ほかの 14 回は 5 個とも 2 秒以内に返った（往復はハンドシェイクを含む 1 回目が約 230 ms、ほかは 5〜8 ms）。

### 14.10 `ts.silent` でつなぎ直す（2026-11-13）
§14.7 の直しで戻るところを、規則で接続を黙らせて確かめた。
- 方法: `netctl` に mode 12 を足した。守りの規則（mode 2）の先頭に `drop in on wlan0 proto tcp` を置く。Wi-Fi から入る TCP（制御サーバと DERP）を黙って捨てるので、RST も ICMP も返らない。途中の NAT が対応を忘れたときと同じ見え方になる。Wi-Fi のつながりと UDP（Matter、mDNS、直の経路）はそのまま。`netctl 2` で戻す。
- 結果（Tab5、ts は約 77 分動いていた）:
  - mode 12 を入れて約 149 秒後（最後に届いてからは 180 秒）に `ts.silent since_map_ms=180011 since_derp_ms=170274`。その前に `ts.ctl_lost` も読み出しのエラーもなかった（それまでは何も気づけていなかったことになる）。
  - その後のつなぎ直しは TCP の段で失敗し続けた（`ts.failed step=tcp`、待ちは 2→4→8→16→30 秒で、以後 30 秒ごと）。SYN の返事を捨てているので当然。
  - `netctl 2` で戻すと、次の試み（約 31 秒後）で `ts.server_key` → `ts.registered` → `ts.map`（peers=10）→ `ts.derp` まで約 6 秒で戻った。ホストからの `tailscale ping` は DERP 経由で届いた。
- 手順の失敗: 最初の試験では `netctl 4` と `netctl 2` を続けて送ったため、2 つ目が `ctl.failed error=no free slot`（前の netctl がまだ動いていた）で動かず、TCP を捨てる状態が約 10 分続いた。`netctl 4` の規則の数（7 のまま）で気づいて戻した。netctl を続けて送るときは、前の `proc.exit` を待つ。

