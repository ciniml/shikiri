# 72. tailnet を `net` のリンクに（形 B）の進め方

2026-11-14。doc/71 §3 の形 B（T6 の 1 つ）の案。§7 は利用者が B0〜B2 を選んだ（2026-11-14）。作ったものは §8、B3（tailnet の ACL）は §9、B4（既定を形 B）は §10、B5（subnet router）は §11。

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

- 実機: ts と tsget を入れ直し、`keys cfg rm ts.form\n` で設定を消して ts を起動し直した。設定なしで `ts.form b=true`、`net.addr_fixed link=ts0`、ホストからの ping は 10 回中 9 回（最初の数回は経路ができるまで遅い）。

## 11. B5 subnet router（2026-11-15）
- 利用者の「B5すすめて」で進めた。Tab5 が tailnet に自分の LAN（などの subnet）を出す。Tailscale の subnet router と同じ形: 広告して、管理画面で承認されたものだけを運ぶ。
- 設定 `ts.routes`（IPv4 の CIDR、コンマか空白で区切る）。形 B のときだけ、register・map・update の `Hostinfo.RoutableIPs` に入れる（`control::Host`）。`ts.routes count=… first=…` を出す。
- 管理画面で承認されると、自分の `AllowedIPs` にその subnet が入る。`NetMap::routes_to(dst)`（自分のアドレスか、自分の `AllowedIPs` の中か）で、`ts` の `Pipe` は自分宛てのほかに承認された subnet 宛ても `ts0` に渡す（それ以外は今までどおり捨てて数える）。ACL（B3）は宛て先を含めて照らす。
- `net`: `ts0` から来た、自分宛てでないパケットは、ほかのリンクと同じく転送（`fwd`）に回る。返りは `ts0` の上の 100.x へ（`ts0` の ARP は `Port::Link` が答える）。
- ガードの規則（`netctl 2`、7 と 12 も）に `pass forward on ts0` と `masquerade on wlan0 from 100.64.0.0/10` を足した。Wi-Fi の LAN の機器は 100.64.0.0/10 への経路を知らないので、Tab5 のアドレスに見せる（Tailscale の既定 `--snat-subnet-routes` と同じ）。転送されるのは `ts` が渡したもの（広告し、承認され、ACL が許したもの）だけ。
- 試験: `NetMap::routes_to` の試験（tsclient 23）。`nettest 8 ARG`: `ts0` の相手が tailnet の相手（100.64.0.2）として ARG に毎秒 ping し、返りを `nettest.link_routed` に。p4emu-run-49: テスト LAN（`run net 1`）とガードの規則で、100.64.0.2 から 192.168.50.2 への ping が返る。
- 実機で端から端まで試すには、管理画面での承認と、経路を受け入れる相手（`--accept-routes`）が要る。ホストの設定は変えない約束なので、利用者と相談する。

- 実機（広告まで）: ts と netctl を入れ直し、`netctl 7`（SD の `NET.RUL` を書き直し）と `netctl 2`（規則 9）。`keys cfg set ts.routes 192.168.8.0/24\n` の後に ts を起動し直すと `ts.routes count=1 first=192.168.8.0/24`。ホストの `tailscale status` では、承認前なので tab5 の `PrimaryRoutes` は空、`AllowedIPs` は自分のアドレスだけ。承認と、LAN の外の相手からの試験は利用者が行う（ホストは同じ LAN にいるので経路を受け入れない）。

- 実機（端から端まで、2026-11-16）: 管理画面で承認しても、はじめは他の端末に経路が配られなかった。tailnet の ACL が grants で、宛て先に 192.168.8.0/24 を含む許可がなかったため（grants では、許可のない subnet route は承認済みでも配られない）。利用者が `autogroup:member` → 192.168.8.0/24 の grant（と autoApprovers）を足すと、LAN の外の端末から Tab5 を経て LAN の機器へつながった。Tab5 側の変更は要らなかった。ホストから見た tab5 の `PrimaryRoutes` は空のまま（ホストが grant の対象外かは未確認）。

## 12. 直の経路の確認と、15 秒ごとのハンドシェイク（2026-11-16）
- 利用者の「1からすすめて」（直の経路が使われていないように見えた件）で調べた。
- 直の経路は壊れていなかった。shell の `ts` の `derp` は、通信のない相手の直の経路を 15 秒で手放した後の表示（§14.1 の 15 秒）。ホストの `tailscale ping` は、続けて打つと直の経路（Tab5 の手前のルータの外側の IPv4、または Tab5 の ULA）になり、往復は約 20 ms。ICMP の ping の最短は約 5 ms。通信のない間の後の 1 パケット目は、経路ができるまで遅い（200〜400 ms）。
- 見つけたこと: `filtered`（ACL が捨てた数）とハンドシェイクが、15 秒ごとに 1 つずつ増え続けていた（約 1 時間で 251）。
  - 捨てたパケットを 1 行で出すようにした（`filter::describe`、イベント `ts.filtered`。始めの 8 つと、その後は 64 に 1 つ）。中身は、利用者のスマホからの IP プロトコル 99（TSMP）、53 バイト、種類 `'a'`（disco key の広告、IP ヘッダ 20 + 種類 1 + 鍵 32）。
  - 原因: WireGuard の受け手が返す keepalive（KEEPALIVE_TIMEOUT、whitepaper §6.5）がなかった。相手はハンドシェイクの後に TSMP を送り、Tab5 は返すものがないので何も送らない。相手は 15 秒（KEEPALIVE + REKEY_TIMEOUT）返事がないのでハンドシェイクをやり直し、また TSMP を送る、を繰り返していた。
- 直したもの:
  - `wg::Device`: 認証の通ったデータを受けて、何も送らないまま `KEEPALIVE_MS`（10 秒）経てば `keepalive_due`。`Node::poll` が空のメッセージを送る（統計 `keepalives`）。送れば消える。
  - `Node`: IP プロトコル 99（TSMP）は ACL の前で取り、数えるだけ（統計 `tsmp`）。本家は TSMP の ping に pong を返し、disco key の広告で相手の disco key を更新するが、今は netmap の disco key で足りている。
  - 試験: `wg::tests::passive_keepalive`、`filter::tests::describe`（tsclient 25）。
- 実機: ts を入れ直して約 130 秒で、ハンドシェイク 3（前は 15 秒に 1）、`tsmp=2`、`keepalives=1`、`filtered=0`。ホストからの ping は 10 回が 2 回とも全部返った。B3 の `filtered=1` もこれだった（IPv6 ではなかった）。

## 13. 直の経路を長く持つ（2026-11-16）
- 利用者の「directの経路の保持時間を長くしてください」で進めた。
- 今まで: 通信のある間（`ACTIVE_MS` 45 秒）は `HEARTBEAT_MS`（3 秒）ごとの disco の ping で経路を確かめ直し、確かめが `DIRECT_MS`（15 秒）途切れると経路を忘れて DERP だけに送っていた。通信のない間が 1 分を超えると、次の 1 パケット目からは経路を探し直すことになる。
- 本家に寄せた: 確かめの途切れた経路も `PATH_KEEP_MS`（10 分）覚えておく（本家の bestAddr が trustUDPAddrDuration を過ぎても残るのと同じ）。その間に送る WireGuard のメッセージ（データ、ハンドシェイク、keepalive）は、その経路への UDP と DERP の両方に送り、その経路へ disco の ping を出す（`HEARTBEAT_MS` に 1 回まで）。Pong が来れば、また UDP だけに戻る。統計 `kept_sends`。
- 試験: `node::tests::peers_reach_wireguard` に、`DIRECT_MS` の後はハンドシェイクが UDP と DERP の両方に出ること、`PATH_KEEP_MS` の後は DERP だけになることを足した。tsclient 25、emu、virt が通った。
- 実機: 90 秒あけた後の 1 パケット目は、直す前が 1,615 ms と 37 ms、直した後が 40 ms、400 ms、35 ms（数が少ないので目安）。`kept_sends=1`。ホストが直の経路で送ってくれば、Tab5 はその受け取りで経路を確かめ直すので、覚えた経路を使うのはホストが DERP で送ってきたときだけ。残る数百 ms は、ts の外（Wi-Fi の省電力など）の可能性があり、まだ調べていない。

## 14. tailnet から Tab5 への TCP（状態のページ）（2026-11-16）
- 形 B では tailnet から Tab5 に始める TCP が届かなかった。ガードの規則で `ts0` から入れるのは ICMP だけで、状態のページ（doc/71 §12、`http://<tailnet のアドレス>/`）は `ts` の中の smoltcp（形 A）に置いていた。B4 で形 B を既定にしたときに見落とした。
- 案 1（`pass in on ts0` で全部、ACL に一本化）と案 2（ポートを決めて足す）のうち、利用者が案 2 を選んだ。
- ガードの規則（`netctl 2`、7 と 12 も）に `pass in on ts0 proto tcp port 80` を足した（規則 10）。
- `ts` の状態のページ: 形 B では `svc:net` の `tcp_listen_at(0, link::TS0, 80)` で `ts0` だけで待ち受ける（`Web::step_net`）。Wi-Fi の LAN からは、ガードの規則とソケットの両方で届かない。答えた後は相手が閉じるのを待ってから閉じる（`WEB_LINGER_MS` 5 秒まで）。こちらから閉じると TIME_WAIT の間（約 10 秒）`net` のソケットが 1 つ埋まり、次の待ち受けが取れなかった。待ち受けに失敗したら `WEB_RETRY_MS`（0.5 秒）後にやり直す（`ts.web_listen` は失敗の続く間に 1 回）。許可に `svc:net/tcp-listen:80`。
- `net` のクライアントのソケットを 8 から 12 にした。待ち受けを足すと 8 個がすべて埋まり（`ts` 4、`matter` 3、待ち受け 1）、ほかのアプリ（nettest の ping）がソケットを取れなくなった。足りないときは `net.no_sockets`（どちらが尽きたか、使用数。最初と 64 回に 1 回）。バッファのプール（12、1 つ約 81 KB）はそのまま。`net` は PSRAM の枠 2048 KB のうち 1806 KB を使っていて、プールを足せるのは 2 つまで。
- 試験: p4emu、virt、emu が通った。
- 実機: netctl を入れて `netctl 7`・`netctl 2`（規則 10）、ts と net を入れ直して再起動。起動で `net.ready sockets=12`、`net.rules rules=10`。ホストから tailnet で状態のページを続けて 4 回取れた（2 回目から約 25 ms。ts の起動後の 1 回目は約 7 秒で、SYN の再送の形。まだ調べていない）。LAN のアドレスの 80 番は届かない。nettest の ping も通った。

## 15. ts を起動し直した後の最初の TCP（2026-11-16）
- §14 の残り: ts を起動し直した後、1 回目のページ取得だけが 7〜19 秒かかった。`ts.stats` では、ハンドシェイクの前に `no session` の捨てが 13 個あった。相手（ホスト）は、起動し直す前の ts と結んだセッションのまま送り続け、新しい ts はそのセッションを知らないので捨てていた。相手は返事がないので、KEEPALIVE + REKEY_TIMEOUT（15 秒）の後にハンドシェイクをやり直し、そこでやっと通っていた。
  - 前のセッションを相手が始めていた（相手が initiator）ときは、相手が送る前に自分からハンドシェイクをし直すことがあり、遅れは出なかった。遅れが出るのは、前のセッションを Tab5 が始めていたとき。
- 直し（tsclient `Node::received`）:
  - 知らないセッションの transport が来て、送り主の相手が分かれば（DERP の node key、UDP なら直の経路か netmap の endpoint）、すぐこちらから initiation を送る。initiation がまだ出ている間（REKEY_TIMEOUT）は送らない。統計 `unknown_sessions`。
  - 自分の initiation に返事が来て、待たせたパケットがなければ keepalive（空の transport）を送る。相手（responder）は、新しいセッションで何か届くまでそれで送らない（wireguard-go も返事の後に keepalive を送る）。
- 試験: `node::tests::unknown_session`（initiation がすぐ出る、出ている間は繰り返さない、返事に keepalive、相手はそれで新しいセッションで送れる）。tsclient の試験は 27。emu、virt、p4emu が通った。
- 実機: ts を入れ直し、前のセッションを Tab5 から始めた状態で ts を起動し直した。`no session` 1 つで initiation を出し（`unknown_sessions=1`）、28 ms でつながった。1 回目のページ取得は 1.03 秒（前は 8.1〜19.4 秒）。残りの 1 秒は、捨てた SYN をホストの TCP が再送するまでの時間（最初の再送が 1 秒）。捨てたパケットは解けないので、ここは縮まらない。

## 16. しばらく通信がなかった後の最初のパケット（2026-11-16）
- §13 の残り: 3 分ほど何も送らなかった後、ホストからの ping の 1 回目が 161〜424 ms（2 回目からは 5 ms）。Wi-Fi の省電力は切ってある（`wifi` の modem sleep は引数で頼んだときだけ）。disco の ping（`tailscale ping`）の 1 回目は 25〜70 ms。
- 3 つの原因が重なっていた:
  1. ハンドシェイクのやり直し。前のセッションが 180 秒（REJECT_AFTER_TIME）を過ぎていて、相手は initiation から始める。ts の返事に 43.5 ms かかった（`ts.handshake via us`、新しいイベント）。受け手の X25519 は 5 回で、1 回が約 9 ms。
  2. disco のメッセージを開くたび・閉じるたびに X25519 をしていた（`SalsaBox::new`）。起き抜けに相手は disco の ping をまとめて 10 個あまり送るので、それだけで 100 ms ほど。
  3. ホストが広告する endpoint のうち 192.168.10.2 は、Tab5 と同じ /24 にあるが ARP に答えない（ホストの別の口で、別の線の先）。そこへの disco の ping が `net` の UDP の待ち行列の先頭に残り、後ろの返事を止めた。ts が 1 秒後にソケットを作り直す（`ts.udp_rebound`）まで、ping の返事が 287〜1396 ms 遅れた回があった。
- 直し:
  - tsclient `disco::Boxes`: 相手の disco key ごとの箱を 16 まで覚える（Tailscale も覚える）。`Node` は `disco::open` と `seal` の代わりにこれを使う。
  - tsclient `wg::Peer::static_static`: 自分と相手の固定の鍵の DH を一度だけ計算して覚える（wireguard-go と同じ）。
  - tsclient `wg::Device::prepare`: 次のハンドシェイクの一時的な鍵を、手の空いたときに作っておく（一度だけ使う）。ts は主ループが眠る前に `Node::prepare` を呼ぶ。
  - smoltcp `udp::Socket::dispatch`（third_party）: 先頭が送れないとき捨てるまでを 3 秒から 1 秒にし、捨てた宛て先を 4 つまで 10 分覚える。覚えた宛て先へのデータグラムは 1 度試し（ARP も出る）、送れなければすぐ捨てる。送れたら忘れる。試験 `test_send_unresolved_dropped` を広げた。
  - ts の `UDP_STUCK_MS` を 1 秒から 3 秒に（smoltcp が先に捨てて覚えるように。作り直すと覚えたものが消える）。
- 試験: tsclient 27（`disco::tests::round_trip` に覚えた箱、`wg::tests::initiate_to_device` に一時的な鍵）、smoltcp の UDP 35、emu、virt、p4emu が通った。
- 実機（ts だけ入れ直した段階）: ハンドシェイクの返事は 43.5 → 35.6（DH を覚えた）→ 27 ms（一時的な鍵も）。待ち行列が止まらなかった回の ping の 1 回目は 87〜101 ms（前は 161〜166 ms）。
- 実機（net も入れ直して reboot）: 3 分あまり何も送らなかった後の ping の 1 回目が 3 回とも 72〜77 ms（2 回目からは 5 ms）。`ts.udp_rebound` は出なかった。ハンドシェイクの返事は 27 ms。残りは、ハンドシェイクの往復と Tab5 の 27 ms、disco の確かめ直し、Wi-Fi の起き抜け（disco の ping の 1 回目も 25〜70 ms）。
