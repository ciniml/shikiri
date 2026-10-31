# 24. N4: パケットのフィルタと接続の追跡

[research/net-stack.md](research/net-stack.md) §3.3 と §4 の段階 N4。`net` が、すべての IPv4 パケットを規則で通す・捨てる・拒否する。規則は制御用のサービス `svc:netctl` から丸ごと差し替える。

## 1. 区切りと状態
| 内容 | 状態 |
|---|---|
| フィルタ（in、forward、out）、接続の追跡、`net.drop` の記録、規則ごとの数 | t5emu と実機で確認（§6） |
| 規則の文字列（§3）と `svc:netctl`、`netctl` アプリ | 組み込みの規則で確認。SD のファイル（`T5OS/NET.RUL`）からの読み込みは未確認 |
| 別のリンクにある自分のアドレスあて（doc/23 §7 の残り） | 解決（§5） |

## 2. フィルタ（`user/apps/net/src/filter.rs`）
- **フック**: パケットは次の 3 つのどれか 1 つだけを通る。127/8 は対象外。PREROUTING/POSTROUTING は NAT（N5）のときに足す（判断）。

  | フック | 場所 | リンク |
  |---|---|---|
  | `in`（Tab5 あて） | デバイスの受信（`link.rs`） | 入ってきたリンク |
  | `forward`（Tab5 を通る） | `Stack::forward_one`。経路を引いた後、TTL の処理の前 | 入ってきたリンクと出ていくリンク |
  | `out`（Tab5 から） | デバイスの送信（smoltcp が作ったものだけ） | 出ていくリンク |

  Tab5 自身が作る ICMP のエラー（TTL、経路なし、拒否）は `out` を通らない。
- **規則**: 上から順に見て、最初に一致した規則で決める。どれにも一致しなければ、フックごとの規定（policy）で決める。規則は最大 64 個。
- **動作**:
  - `pass`: 通す。
  - `drop`: 黙って捨てる。
  - `reject`: 捨てて、ICMP Destination Unreachable（code 13、administratively prohibited）を返す。`in` と `forward` だけで使える。`out` では `drop` と同じ。
- **接続の追跡**: 512 項目。キーの 5 組（プロトコル、両端のアドレスとポート）から計算した位置から 8 つを探す。ICMP の echo は、識別子を両端のポートとして扱う。
  - 通ったパケットは、自分の接続を作るか、寿命を延ばす。
  - 寿命: TCP は 600 秒（返事が来る前は 60 秒）、UDP は 120 秒（返事の前は 30 秒）、そのほかは 30 秒。FIN か RST を見たら 10 秒。
- **状態**:

  | 状態 | 意味 |
  |---|---|
  | `established` | 両方向に通ったことのある接続のパケット |
  | `related` | 既知の接続のパケットを引用した ICMP のエラー（Destination Unreachable、Time Exceeded、Parameter Problem） |
  | `invalid` | 接続がないのに SYN だけでない TCP。既知の接続を引用していない ICMP のエラー |
  | `new` | それ以外 |
- **DHCP の例外**: 応答（UDP 67 → 68）は、規則によらず `in` で通す。アドレスを得る前に来て、どの接続にも当たらないため（判断）。
- **記録**:
  - 規則に `log` を付けると、その規則で捨てたものを `net.drop` で出す。フィールドは hook、rule、reject、link、proto、saddr、daddr、sport、dport、state。
  - 規定で捨てたものは、`policy in drop log` と書いたときだけ記録する。マルチキャストなどで記録が溢れるため。
  - 記録は 1 秒に 10 個まで。超えた数は `net.drop_suppressed` で出す。
- **差し替え**: COMMIT を受けた時点で、規則の組を 1 回の代入で入れ替える。`net` は 1 本の流れで動いているので、パケットの処理の途中に半端な規則は見えない。差し替えても接続の表は残る。

## 3. 規則の書き方（`rules.rs`）
1 行に 1 つ。`#` から行末まではコメント。
```
policy in drop                      # 規定（pass / drop。drop の後に log を書くと記録）
pass in on host                     # PC（ホストリンク）からは何でも
pass in state established,related   # 返事と、それについての ICMP のエラー
pass forward on host out test0
reject in on wlan0 proto tcp port 22 log
drop forward from 192.168.50.0/24 to any
```
`ACTION HOOK [on LINK] [out LINK] [proto tcp|udp|icmp] [from CIDR] [to CIDR] [port N[-M]] [state S,...] [log]`
- `on`: `in` と `forward` では入ってきたリンク、`out` では出ていくリンク。`out`: 出ていくリンク（`forward` 用）。
- リンク: host、wlan0、test0。
- CIDR: `any` か A.B.C.D[/LEN]。`port`: 宛先ポート（TCP、UDP）。

## 4. `svc:netctl` と `netctl` アプリ
- `net` は `listen:netctl` で待ち、一度に 1 人だけ受け付ける。要求の形は `abi/src/net.rs` の `ctl`。
  - BEGIN の後、LINE で 1 行ずつ送り、COMMIT で反映する。読めない行があると、COMMIT が最初の悪い行の番号を返し、規則は変わらない。
  - CLEAR は規則を空にする（すべて通す）。STATS は、規則ごとの数（`net.rule_hits`）、規定で決まった数（`net.policy_hits`）、接続の表（`net.conntrack`）を記録する。
  - 使うアプリには `--permit svc:netctl` が要る。
- `netctl` アプリ（`--permit svc:netctl --permit svc:fs`）: `run netctl MODE`

  | MODE | 動作 |
  |---|---|
  | 0 | SD の `T5OS/NET.RUL` を読んで送る |
  | 1 | 試験用の規則（t5emu）を送る |
  | 2 | 守りの規則を送る |
  | 3 | 規則を空にする |
  | 4 | 統計を出させる |

  - 試験用の規則: Tab5 と試験用 LAN に向けて新しい通信を始められるのは PC だけ。試験用 LAN からは何も始められない。
  - 守りの規則: Tab5 に向けて新しい通信を始められるのは、ホストリンクの PC だけ。転送するのも、PC が始めた通信だけ。

## 5. 別のリンクにある自分のアドレス
doc/23 §7 の穴: 試験用 LAN から Tab5 のホストリンク側のアドレス（10.55.0.2）に送ると、転送の対象になって PC に流れていた（t5emu で、`net.drop` の hook が forward になって見つかった）。

- 各インタフェースに、ほかのリンクの自分のアドレスを /32 の別名として持たせる（`Stack::sync_addrs`。アドレスが変わるたびに作り直す）。どのリンクから来ても、そのリンクのインタフェースが受け取り、来たリンクから返事をする。転送の判定（`link::transit`）も、すべての自分のアドレスを自分あてとして扱う。フィルタは、入ってきたリンクの `in` として見る。
- smoltcp の 1 インタフェースあたりのアドレスの数を 4 にした（`iface-max-addr-count-4`）。
- 副作用: 各 Ethernet のリンクで、ほかのリンクの自分のアドレスへの ARP にも答える（代理 ARP と同じ）。Wi-Fi の LAN で 10.55.0.2 や 192.168.50.1 を探す機器はふつういないので、許容する（判断）。

## 6. 結果
### 6.1 t5emu（`make test-p4emu`）
試験用 LAN の相手は、2 秒ごとに Tab5（10.55.0.2）と PC（10.55.0.1）へ交互に ping を送る。10 回ごとに、送った数と返事の数を `net.peer_probes` で出す。PC 役は、その最初の記録（約 20 秒後）を合図に、Tab5 を通って試験用 LAN の相手に ping と TCP を送る。

| | Tab5 への ping | PC への ping | PC 役の試験 |
|---|---|---|---|
| 規則なし（`run net 1`） | 5/5 応答 | 4/5 応答 | 通る |
| 試験用の規則（`run netctl 1`） | 0/5（`net.drop` hook=in） | 0/5（`net.drop` hook=forward） | 通る（ping、TCP 16 KiB、Time Exceeded、Destination Unreachable） |

規則なしで PC への 1 回目が返らないのは、起動直後のため（未調査）。PC 役の smoltcp は ping に自動で答えるようにした（`auto-icmp-echo-reply`）。

### 6.2 実機（2026-11-01、iotap）
守りの規則（`run netctl 2`）を入れた状態で:
- LAN の PC（有線）から Tab5 の Wi-Fi のアドレスへの ping: 0/5。規則を消す（`run netctl 3`）と 5/5。
- LAN の PC から TCP の 7777 番への接続: タイムアウト。
- Tab5 からの DNS と HTTP（`nettest 4`）: 通る。返事は established の規則で通る。
- `run netctl 4` の統計: established の規則に 8 回、`in` の規定で捨てたものが 5 回（ping）、接続の表に 7 件。

## 7. まだ
- SD のファイルからの読み込み（mode 0）を実機で確かめる。`net` の起動時に自動で読む。
- TCP の reject に RST を返す。conntrack の TCP の状態を細かくする（今は返事の有無と FIN/RST だけ）。
- 規則を見る操作（今は数だけ）。
- NAT（masquerade）と下流の DHCP サーバ（N5）。
