# 25. N5: NAT（masquerade）と DHCP サーバ

[research/net-stack.md](research/net-stack.md) §4 の段階 N5。`net` が、下流のリンクの機器を、上流のリンクの自分のアドレスの陰に隠して外へ出す（masquerade）。下流のリンクでアドレスを配る（DHCP サーバ）。

## 1. 区切りと状態
| 内容 | 状態 |
|---|---|
| masquerade（TCP、UDP、ICMP echo） | t5emu（ホストリンクへ）と実機（Wi-Fi へ）で確認 |
| DHCP サーバ | t5emu と実機で、試験用 LAN の相手が借りるのを確認 |
| research の完了条件「PC を USB 側につなぎ、DHCP でアドレスを得て、Tab5 を通して外の Web が見える」 | USB の下流のリンク（CDC-NCM など）がまだないので、試験用 LAN の相手で代えた（§4） |

## 2. masquerade（`user/apps/net/src/nat.rs`）
規則の文字列に `masquerade on LINK [from CIDR]` を書く（最大 4 つ、doc/24 §3）。

- **外向き**: 転送するパケットの出口が LINK で、送信元が CIDR に入っていれば書き換える（`Stack::forward_one`。フィルタの forward と TTL の処理の後）。
  - 送信元のアドレスは、LINK の自分のアドレスにする。
  - 送信元のポート（TCP、UDP）または echo の識別子（ICMP の echo request）は、30000 から始まる番号。smoltcp の自分のソケットの一時ポート（49152〜）とは重ならない。
  - 対応表は 2048 項目（doc/58 R5 で 256 から）。内側の端と相手の、起動時の乱数を鍵にしたハッシュから 16 の枠を見る。ポートは 30000 + 枠の番号なので、返事はポートから探さずに対応にたどり着き、鍵のためにポートは当てられない。
  - 寿命は TCP 600 秒、UDP 30 秒（答えの後にも送り続ける流れは 120 秒、doc/58 R5）、ICMP 30 秒で、通るたびに延ばす。
  - 満ちたら、見る枠の中の答えのない対応のうち、一番早く切れるものを捨てて入れる（doc/58 R5）。
- **内向き**: LINK の自分のアドレスとそのポートあてで、相手も一致するパケットは、元の宛先に戻して転送に回す（`link.rs` の受信。フィルタの `in` より前）。フィルタは、元に戻した後のアドレスで forward として見る。接続の追跡も、書き換える前のアドレスで記録されているので、返事は established になる。
- **チェックサム**: IP ヘッダは計算し直す。TCP、UDP、ICMP は差分で直す（RFC 1624 の式 3）。TCP と UDP は、IP アドレスも擬似ヘッダの分として足す。チェックサムが 0（なし）の UDP はそのまま。
- 統計: `netctl 4` で `net.nat`（作った対応の数、表が足りなかった数、捨てて入れた数）。
- まだ: 外から来る ICMP のエラー（途中のルータの Time Exceeded など）の中身を戻していない。traceroute は、最後の段しか見えない。

## 3. DHCP サーバ（`dhcpd.rs`）
規則の文字列に `dhcp-server on LINK range FIRST-LAST [dns ADDR]` を書く（1 つ、範囲は 256 個まで）。規則の組が変わるたびに作り直す。

- LINK のインタフェースの UDP 67 番で待つ。ブロードキャストの DISCOVER には OFFER を、REQUEST には ACK（使えないアドレスなら NAK）を、いずれもブロードキャストで返す。RELEASE で貸し出しを消す。
- 同じ MAC には、貸し出しが生きている間は同じアドレスを渡す。貸し出しは 1 時間、表は 64 項目。
- 渡すもの: サブネットマスク、ルータ（LINK の自分のアドレス）、DNS サーバ（`dns` の指定、なければ Wi-Fi の DHCP で得た DNS サーバ、それもなければ LINK の自分のアドレス）。
  - Tab5 は DNS サーバを持たない。Wi-Fi のアドレスが決まる前に貸すと、使えない DNS サーバを渡してしまう（§4 の実機の例）。DNS の中継は後で。
- フィルタ: DHCP サーバへの要求（UDP 68 → 67）は、DHCP クライアントへの応答と同じく、規則によらず `in` で通す。

## 4. 試験用 LAN の相手の DHCP（`run net 3`）
`run net` の bit1 を立てると、試験用 LAN の相手（doc/23 §4）は、静的な 192.168.50.2 の代わりに DHCP でアドレスを得る（`net.peer_dhcp`）。得たら一度、`http://1.1.1.1/` を取りに行き、応答の 1 行目を `net.peer_http` で出す。2 秒ごとの ping（Tab5 と PC へ）も、アドレスを得てから始める。最初の返事は `net.peer_answered` で出す。

`netctl` に 2 つの組を足した:

| MODE | 中身 |
|---|---|
| 5（t5emu 用） | `masquerade on host from 192.168.50.0/24`、`dhcp-server on test0 range 192.168.50.100-192.168.50.149` |
| 6（実機用） | `masquerade on wlan0 from 192.168.50.0/24`、`masquerade on wlan0 from 10.55.0.0/24`、同じ DHCP サーバ |

## 5. 結果
### 5.1 t5emu（`make test-p4emu`）
t5emu の PC 役に、何もせず答えるだけの使い方（`--host-peer-start none`）を足した。その間に受けた ping の送信元を記録し、`--host-peer-pinged-by` で確かめる。PC 役は ping に自動で答える（doc/24 §6.1）。

```
net.dhcp_lease mac=027435000002 addr=192.168.50.100 seconds=3600
net.peer_dhcp addr=192.168.50.100 router=192.168.50.1 dns=192.168.50.1
net.peer_answered target=tab5
net.peer_answered target=pc
net.peer_probes sent_tab5=5 answered_tab5=5 sent_pc=5 answered_pc=4
t5emu: host peer (passive): pinged by ["10.55.0.2"]
```
- 相手は DHCP で 192.168.50.100 を得た。
- PC 役から見た ping の送信元は 10.55.0.2 だけ。masquerade がなければ 192.168.50.100 から来る。
- 貸し出しまで約 10 秒かかったのは、相手の最初の DISCOVER がサーバの設定より前に出て、smoltcp の DHCP クライアントが待ってから送り直したため。

### 5.2 実機（2026-11-01、iotap）
`run net 3`、`run netctl 6` で:
```
net.dhcp_lease mac=027435000002 addr=192.168.50.100 seconds=3600
net.peer_dhcp addr=192.168.50.100 router=192.168.50.1 dns=192.168.50.1
net.dhcp link=wlan0 addr=192.168.2.112 ...
net.peer_http status=HTTP/1.1 301 Moved Permanently ms=3182
```
- 試験用 LAN の相手が、Tab5 の DHCP サーバからアドレスを借り、Wi-Fi の masquerade を通って 1.1.1.1 から HTTP の応答（https への転送の指示）を受けた。
- 3.2 秒かかったのは、相手の接続が、Tab5 の Wi-Fi の DHCP より先に始まったため。最初の SYN は出口がなく捨てられ、TCP の再送で通った。
- 相手への DNS サーバは 192.168.50.1（Tab5）になった。Wi-Fi のアドレスより先に貸したため（§3）。
- PC（ホストリンク）から masquerade で外へ出る確認は、PC に Tab5 経由の経路（root）が要るので、していない。

## 6. まだ
- USB の下流のリンク（CDC-NCM）。research の N5 の完了条件はそれを待つ。
- DNS の中継（Tab5 を DNS サーバとして渡せるように）: doc/29 で済み。
- ICMP のエラーの中身の変換、ポートの転送（外から内への DNAT）。
- 設定（規則、NAT、DHCP サーバ）を `net` の起動時に SD から自動で読む。
