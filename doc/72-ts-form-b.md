# 72. tailnet を `net` のリンクに（形 B）の進め方

2026-11-14。doc/71 §3 の形 B（T6 の 1 つ）の案。§7 は利用者が B0〜B2 を選んだ（2026-11-14）。作ったものは §8、B3（tailnet の ACL）は §9、B4（既定を形 B）は §10。

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

## 8. 作ったもの（B0〜B2、2026-11-15）
- 利用者の選び: B0〜B2、つなぎ方は案 B-1、名前は案 N-1。

### B0 環と `ts0`
- `t5_abi::netlink`: `svc:netlink`。共有メモリは相手（`ts`）が用意して渡す。`net` は受け付けると `KICK` を送り、相手はそれを聞いてから渡す（渡すには両端がそろっていないといけない。カーネルは受け付け前の grant を `Off` で断る）。配置は `netdev::ring` と同じ（`net` がプロセスの側、相手がカーネルの側。相手は `netlink::Peer` で読み書きする）で、その後ろに名前の表（4 KiB）。
  - 最初は `net` が共有メモリを用意して渡す形にしたが、実機で `ts` の `svc:net` への接続が失敗した。1 つのプロセスが写せる共有メモリの窓は 2 つ（`Process::shm_mapped`）で、形 B の `ts` は `svc:ts` の枠、`svc:net` の枠、環の 3 つが要った。`ts` が `svc:ts` の枠の領域の後ろに環を置き（1 つの窓）、そこを `net` に渡す形に改めた。`net` の窓は自分の枠と環の 2 つ。チャネルの通知は `KICK`（環に足した）、`ADDR`（`[ADDR, prefix, 0, 0, IPv4]`）、`NAMES`。`net::link::TS0`（7）。
- `net`:
  - `svc:netlink` で待ち、許可 `svc:netlink/ts0` のある 1 つだけを受ける（ほかは `net.denied op=netlink`）。リンク番号 5（`fwd::TS0`、`IFACES` は 6）。最初の接続でリンクを作り、相手が去っても残す（アドレスを外すだけ。ソケットの持ち主が困らないように）。
  - リンクは Ethernet の形のまま（`EthLink`、`Port::Link`）にした。フィルタ、転送、リンクごとのソケット、経路の選び方がそのまま使える。`Port::Link` は、smoltcp が出す Ethernet の枠から IP を取り出して環に入れ、環から来た IP に枠を付ける。ARP の問い合わせには、どのアドレスも相手の作り物の MAC（`02:74:73:00:00:02`）と、ここで答える。アドレスは相手の `ADDR` で 100.x/10 にし、100.64.0.0/10 は `ts0` の上（相手がどのアドレスも引き受ける）。
  - 環は `wait` の合図を待たずに毎回の poll で見る（共有メモリを読むだけ）。`KICK` は `wait` を終わらせるためだけ。
  - リンクが 1 つ増えて `Stack`（34 KB、`EthLink` は 2.6 KB）が大きくなり、64 KiB のスタックがあふれた。あふれても fault にならず、テスト LAN の転送（p4emu-run-18〜20、23）が黙って止まった。スタックを 96 KiB にした。
- `nettest 8`: `ts0` の相手の役（100.64.0.1/10、名前 `tspeer`、リンクのほかのアドレスへの ping に答える）。
- p4emu-run-47: `nettest 8` と `nettest 3 100.64.0.2`（`svc:net` の ping が `ts0` を往復）、`nettest 9`（`svc:net` で `TSPeer.` を引いて ping）。

### B1 `ts` を管に
- 設定 `ts.form` が `b` なら、`ts` は各セッションの前に `svc:netlink` につなぐ（0.5 秒おきに 20 回。つなげなければそのセッションは形 A、`ts.form b=false`）。起動時は `ts` が `net` より先に動くので、1 回だけでは形 A になった。許可に `svc:netlink/ts0`。
- `Node::pipe`（tsclient）: smoltcp を回さず、アドレスも付けない。相手から解いたパケットは `tun.dev.rx` に、相手へのパケットは `tun.dev.tx` に置く（形 A と同じ置き場所）。`ts` の `Pipe::step` が、環 → `tun.dev.tx`、`tun.dev.rx` → 環（宛て先が自分の 100.x のものだけ。ほかは数えて捨てる）と移す。netmap の自分のアドレスが変われば `ADDR` を送る。
- 形 B では `svc:ts` の TCP・UDP・PING は `NO_ADDRESS`（ノードのスタックにアドレスがない）、`IF_INFO` は「ない」。`PEER_INFO`、`LOGIN_INFO`、`RESOLVE` は今どおり。`Net::for_host` は `svc:ts` の `IF_INFO` で形を見分け、形 B なら tailnet の宛て先も `svc:net` へ。
- `net` が落ちて `svc:netlink` が閉じたら、`ts` はセッションを終え、次は形 A で始める。

### B2 名前
- `NetMap::names`（tsclient）: `resolve` が答える名前をすべて並べる（管理画面の名前とノードの全体の名前、それぞれから検索ドメインを外したもの、ノードの短い名前。小文字、最後の点なし、先に出たものが勝つ）。試験で、並べた名前がすべて `resolve` と同じ答えになることを確かめる。
- `ts` は netmap の相手か DNS の設定が変わるたびに、表を共有メモリに書いて `NAMES` を送る。`net` は表を自分の中に写し（`tsnames`）、RESOLVE（IPv4）で DNS より先に見る。見つかればすぐ答える（`Slot::known`）。`ts0` の相手が去れば表を消す。

### 実機（2026-11-15）
- net・ts・shell・ota・netctl を入れ直し、`cfg set ts.form b` の後に reboot。`ts.netlink addr=…`、`net.addr_fixed link=ts0 prefix=10`、`ts.netlink_names count=22`、`net.ts_names count=22`。
- 最初はホストから Tab5 への ping が返らなかった（`ts.stats` で packets は増え、packets_out は 0）。ガードの規則（`netctl 2`、`policy in drop`）に `ts0` の規則がなく、`net` が捨てていた。形 A では `ts` の smoltcp が ping に答えていたので、同じところまでとして `pass in on ts0 proto icmp` を足した（`netctl 7` で SD の `T5OS/NET.RUL` を書き直し、`netctl 2` で読み込み。規則は 8）。Tab5 から始めた接続の返事は `pass in state established,related` で通る。tailnet から Tab5 へ始める TCP・UDP はまだ通さない（B3 で決める）。
- ホストから `ping 100.x`（Tab5）: 5 回中 4 回、往復 5〜8 ms（直の経路）。Tab5 の端末で `ifconfig` に `ts0` が 100.x で出て、`ping prometheus`（`svc:net` の RESOLVE が名前の表で引き、`ts0` を通る）が返った（利用者が確認）。
- 戻すには `cfg delete ts.form`（またはその値を `a`）で、次の起動から形 A。

## 9. B3 tailnet の ACL（2026-11-15）
- 利用者が「すすめて」と言い、B3 に進めた。tailnet の ACL（netmap の `PacketFilter`）を `ts` で、相手から来たパケットに照らす。形 A と形 B のどちらにも効く（`Node` が `tun.dev.rx` に置く前に見る）。
- `tsclient::filter`（Tailscale の `wgengine/filter` と同じ決め方、IPv4 だけ）:
  - TCP: 接続の最初（SYN で ACK なし）だけを規則に照らし、残りは通す（どちらかが始めた接続のもの）。
  - UDP: このノードが送った宛て先・ポートからの返事は 2 分の間通す（`note_out` で覚える、64 まで）。ほかは規則に照らす。
  - ICMP: echo の返事とエラーは通す。ほかは規則に、アドレスだけで（ポートなし）照らす。
  - ほかのプロトコルは、`IPProto` にそれを挙げた規則が要る。`IPProto` が空の規則は TCP・UDP・ICMP・ICMPv6。
  - 規則は `SrcIPs`（`*`、アドレス、CIDR、`a-b` の範囲。IPv6 は飛ばす）と `DstPorts`（アドレスとポートの範囲）。`DstPorts` のない規則（`CapGrant` など）は飛ばす。
  - 制御サーバから規則が来るまでは、何も通さない。
- netmap: `PacketFilter`（丸ごと）と、新しいサーバの `PacketFilters`（名前ごとの部分。`*` があればそれまでの部分を消す）を読む。今の capver（65）では `PacketFilter` が来る。`ts.map` に `acl_rules`、`ts.stats` に `filtered`（ACL が捨てた数）。
- `net` のガードの規則（`ts0` は ping だけ受ける）は持ち主の側の守りとして、そのままにした。tailnet から Tab5 に始める TCP・UDP を受けたくなったら、両方（ACL と `pass in on ts0 …`）が要る。
- 試験: `filter::tests`（規則、TCP の SYN と残り、UDP の返事と期限、ICMP、ほかのプロトコル、IPv6 は通さない、範囲の読み方）、`netmap::tests::filters`（`PacketFilter` と `PacketFilters` の部分と `*`）。tsclient の試験は 22。

### 実機で見つかった固まり（2026-11-15）
- B3 の `ts` を入れて起動し直すと、`ts` の loop が止まった（ホストからの ping も `tailscale ping` も返らず、shell の `ts` は `svc:ts` に答えがなく「not running」）。プローブで止めた所は `t5_rt::sleep_ms` で、呼び元は `Net::udp_send_ip` の「待ち行列に空きができるまで待つ」loop だった。
  - 宛て先は相手の ULA の endpoint（Wi-Fi の /64 の中にあるが、近隣探索に答えない）。smoltcp 0.14 は、次の段が分からないデータグラムを待ち行列の先頭に残し続け、後ろのものも出さない。`net` は空きなし（0）を返し続け、`ts` は永遠に待った。doc/71 §14.3 の、再現しなかった固まりもこれだった見込み（推測）。
- 直し（3 つ）:
  1. smoltcp（third_party）: UDP の先頭が 3 秒送れなければ捨てる（`UNRESOLVED_DROP`、試験 `test_send_unresolved_dropped`）。
  2. `t5_rt::Net::udp_send_ip`（と `udp_send`）: 空きを待つのは 100 ms まで（`UDP_SEND_WAIT_MS`）、その後は `TimedOut`。`matter` も同じ関数を使っていた。
  3. `ts`: UDP の送信が 1 秒の間 `TimedOut` のままなら、ソケットを作り直す（`ts.udp_rebound`。古い `net` でも先頭が消える）。TLS の読み出しも 15 秒で諦める（`ts.stalled`、`READ_STALL_MS`。止まっていたのはこちらではなかったが、同じ形の永遠の待ち）。
- 2 と 3 の後（smoltcp の直しの前）: ホストからの ping は 15 回中 8 回返り、`ts.udp_rebound` が出て `ts` は止まらなくなった。ACL は `acl_rules=1`、`filtered=1`（ACL が捨てたものが 1 つ。中身は未確認）。

- 3 つの直しを入れた後（net・ts・matter・shell・ota を入れ直して reboot）: 形 B で `acl_rules=1`。ホストから Tab5 へ 40 回の ping がすべて返り（平均 7 ms、直の経路）、同じ間に Tab5 の端末（`keys ping prometheus\n`）からの 4 回も返った。`ts.udp_rebound` も `ts.stalled` も出なかった。`filtered=1` は ACL が捨てた 1 つ（IPv6 のパケットは規則に照らさず通さないので、その類いと見られる。推測）。

## 10. B4 既定を形 B に（2026-11-15）
- 利用者の「すすめて」で B4 に進めた。`ts.form` の既定を `b` にした。`a` と書いたときと、`net` に `svc:netlink` がないとき（各セッションの前に 10 秒試す）は形 A。
- 形 A は残した（`svc:ts` のソケット、`ts` の中の smoltcp）。そのため §6 の「ヒープを減らす」はしていない。形 A を外すのは、形 B で困ることがないと分かってから。
- `t5_rt::Net::for_ip(ip)`: tailnet のアドレスへは、`ts` が自分のスタックを持つ（形 A）なら `svc:ts`、ほかは `svc:net`。`for_host` の数字のアドレスもこれを使う。`tsget` の TCP と UDP もこれで開く（形 B では `svc:ts` のソケットは `NO_ADDRESS`）。`tsget` の許可に `svc:net/tcp-connect` と `svc:net/udp:any` を足した。
- 実機は `cfg set ts.form b` 済みなので、動きは §9 の終わりと同じ。

