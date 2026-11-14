# 72. tailnet を `net` のリンクに（形 B）の進め方

2026-11-14。doc/71 §3 の形 B（T6 の 1 つ）の案。この文書は案で、まだ作っていない。決めることは §7。

## 1. 今（形 A）とその困りごと
- `ts` は自分の中に smoltcp を持つ（`tsclient::tun::Tun`、MTU 1280）。WireGuard で解いた IP パケットはこの smoltcp に入り、ほかのアプリは `svc:ts`（`svc:net` と同じ要求）でソケットを借りる。
- 困りごと:
  1. **アプリが 2 つの網を使い分ける**: `svc:net` と `svc:ts` を選ぶのはアプリの側（`Net::for_host`、doc/71 §14.16）。選ばないアプリ（`matter`、`wifi` の設定、将来のアプリ）からは tailnet に出られない。
  2. **フィルタが 2 か所**: `net` のフィルタ（doc/24、ガードの規則）は tailnet のパケットを見ない。`ts` の中では Tailscale の PacketFilter も効かせていない（doc/71 の T3 では「要る」としたが未実装）。
  3. **ソケットが少ない**: `svc:ts` はクライアント 2、ソケット 8、バッファは `ts` の 1 MiB のヒープから。
  4. **転送ができない**: Tab5 を subnet router にする（試験の LAN や Thread の網を tailnet に出す）には、`net` の転送（`fwd`、`nat`）とつながっていないといけない。
- 形 A の良い点: `net` に手を入れずに済んだ。`ts` が落ちても `net` は無傷。

## 2. 形 B の形
```
 アプリ ──svc:net──▶ net（smoltcp、フィルタ、転送）
                       ├─ wlan0（netdev:wlan0）
                       ├─ host、eth0、ap0、test0
                       └─ ts0 ◀──共有メモリの環 2 本──▶ ts（WireGuard、DERP、disco、制御）
```
- `net` に IP のリンク `ts0` を足す（`Medium::Ip`、自分のインターフェース）。アドレスは tailnet の 100.x/32 と fd7a:115c:a1e0::x/128。経路は 100.64.0.0/10 と fd7a:115c:a1e0::/48 を `ts0` へ（subnet router の経路は後で netmap の AllowedIPs から足す）。
- `ts` は IP スタックを持たない「パケットの管」になる。`ts0` から来た IP パケットを宛て先で相手を選んで WireGuard で送り、相手から解いたパケットを `ts0` へ返す。disco、STUN、DERP、制御は今のまま（UDP 41641 と TCP は今どおり `svc:net` で）。
- アプリは `svc:net` だけを使う。`Net::for_host` の `svc:ts` への振り分けは要らなくなる（名前の解決だけ残る、§4）。

## 3. `ts` と `net` のつなぎ方
チャネルの 1 通は 256 バイト、待ち行列は 4 通（`chan::MSG_MAX`、`QUEUE`）なので、パケットは運べない。テスト LAN（`testlan`）やカーネルの netdev の環（doc/35 §5）と同じく、共有メモリの環にする。

- **案 B-1（推し）: `net` が `svc:netlink` で待ち、`ts` がつなぐ**。`net` は共有メモリ（16 枠 × 1536 バイトの環を向きごとに 1 本、`netdev::ring` と同じ配置で約 50 KB）を `ts` に渡す（`Shm::grant`）。どちらかが環に足したら、チャネルに 1 バイトの「叩く」を送り、相手は `wait` で起きる。
  - 制御の通知もこのチャネルで: `ts` → `net` に「アドレス（v4、v6）」「経路（AllowedIPs）」「切れた」。`net` → `ts` は今は要らない。
  - 許可: `ts` に `svc:netlink/ts0`（署名した許可、doc/52）、`net` に `listen:netlink`。ほかのアプリは `ts0` を名乗れない。
  - `ts` が落ちたら: チャネルが閉じ、`net` は `ts0` を下ろす（アドレスと経路を外す）。`init` が `ts` を起こし直せば、またつなぐ。
- 案 B-2: カーネルに `netdev:ts0` を作り、`ts` が書き手、`net` が読み手になる。カーネルに「プロセスが実装する netdev」を足すことになり、大きい。B-1 で足りる。
- 案 B-3: `ts` が `net` の中に入る（1 つのプロセス）。`net` が TLS と WireGuard を抱え、落ちたときの被害も大きい。とらない。

## 4. 名前（MagicDNS）
- `net` の RESOLVE は DHCP の DNS にしか聞かない。tailnet の名前を `svc:net` で引けるようにするには:
  - 案 N-1（推し）: `ts` が `svc:netlink` のチャネルで名前の表（名前 → v4、v6。検索ドメイン）を `net` に渡し、`net` の RESOLVE が先にそれを見る。表は相手の数ほど（10 で数百バイト）なので、共有メモリの決まった場所に置き、変わったら叩く。
  - 案 N-2: `net` が 100.100.100.100 に問い合わせ、`ts` が DNS で答える。`ts` に DNS のサーバ（パケットの管の中で 100.100.100.100:53 を自分で答える）が要る。Tailscale の本来の形だが、Tab5 では N-1 より大きい。
- 当面は `Net::for_host` も残す（形 A に戻したときのため）。

## 5. フィルタ
- `ts0` は `net` のフィルタの 1 つのリンクになる。ガードの規則（doc/24 §4、`netctl 2`）に `ts0` の規則を足す（例: tailnet からは ping と決めたポートだけ受ける）。
- Tailscale の PacketFilter（tailnet の ACL）は `ts` の中で、`ts0` に渡す前に照らす（既定は拒否。netmap の `PacketFilter` を読む）。2 段になるが、ACL は tailnet の管理者のもの、`net` の規則は Tab5 の持ち主のもの、と役目が違う。

## 6. 段取り
| 段 | 中身 | 確かめ |
|---|---|---|
| **B0** 環 | `t5_abi` に環と叩きの決まり（`netlink`）。`net` の `svc:netlink`、`ts0` のリンク（`Medium::Ip`、アドレスと経路を受ける）。試験用に `nettest` が `ts0` を名乗り、ping に答える | t5emu: `nettest` 経由で `net` の ping が往復 |
| **B1** 管 | `ts` の `Node` が `Tun` の代わりに環とやりとりする形を持つ（`ts.form` の設定で A と B を切り替え、既定は A のまま）。出ていくパケットの宛て先から相手を選ぶ（AllowedIPs） | ホスト: tsclient の試験（宛て先 → 相手）。実機: `svc:net` の `ping 100.x` と `nc` が tailnet を通る |
| **B2** 名前 | 案 N-1: 名前の表を `net` に渡し、`net` の RESOLVE が先に見る | 実機: どのアプリの `resolve("laptop")` も 100.x |
| **B3** フィルタ | `ts` で PacketFilter、`net` のガードの規則に `ts0` | 実機: ACL で許さない相手からの ping が届かない |
| **B4** 既定を B に | `ts.form` の既定を B、`svc:ts` はソケットの要求をやめて PEER_INFO・LOGIN_INFO だけに。`ts` のヒープを減らす（smoltcp のソケットの分） | 自己試験、t5emu、実機で一通り |
| **B5** subnet router（必要なら） | `net` の転送で `ts0` と LAN をつなぎ、netmap に経路を広告する（`Hostinfo.RoutableIPs`）。管理画面で承認が要る | 実機: tailnet の PC から試験の LAN の機器に届く |

- B0 と B1 は `net` に手を入れる。`net` は落ちると全部が止まるので、`ts0` のない構成（`ts` が動いていない）で今と同じに動くことを毎段で確かめる（virt、p4emu の全部）。
- 速さ: 形 A は「WireGuard → ts の smoltcp → 共有メモリ → アプリ」、形 B は「WireGuard → 環 → net の smoltcp → 共有メモリ → アプリ」で、コピーが 1 回増え、`ts` と `net` の行き来が増える。tailnet の速さは DERP と暗号で決まっているので（数百 KB/s〜）、気にならない見込み（推測。B1 で測る）。

## 7. 決めること
1. 形 B に進めるか。形 A のままでも、`Net::for_host` を使うアプリには tailnet が届いている（doc/71 §14.16）。形 B の得は、すべてのアプリ、フィルタの一本化、subnet router。
2. つなぎ方: 案 B-1（`svc:netlink` と共有メモリの環）でよいか。
3. 名前: 案 N-1（表を `net` に渡す）でよいか。
4. どこまで: B0〜B2（どのアプリからも tailnet）で止めるか、B3（ACL）、B4（既定を B）、B5（subnet router）まで行くか。
