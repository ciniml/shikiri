# 29. DNS の中継

doc/25 §3 の残り:「Tab5 は DNS サーバを持たないので、Wi-Fi のアドレスが決まる前に貸すと、使えない DNS サーバを渡してしまう」。下流の機器の名前の問い合わせを、Tab5 が上流の DNS サーバへ中継する。

## 1. 区切りと状態
| 内容 | 状態 |
|---|---|
| DNS の中継（UDP、IPv4） | 実機で確認（§3） |
| DHCP サーバが DNS サーバとして Tab5 自身を渡す | 実機で確認 |
| IPv6 の下流への DNS（RA の RDNSS） | 未 |

## 2. 仕組み（`user/apps/net/src/dnsrelay.rs`）
- DHCP サーバ（`dhcp-server on LINK ...`、doc/25 §3）を置くと、同じリンクに中継も立つ（`net.dns_relay`）。規則に別の行は要らない（判断）。今は LINK が試験用 LAN で、Wi-Fi があるときだけ。
- DHCP サーバは、DNS サーバとして LINK の Tab5 のアドレスを渡す（`dns ADDR` の指定があればそちら）。以前は、Wi-Fi の DHCP で得た DNS サーバを直接渡していた。
- 中継は LINK の UDP 53 番で待つ。問い合わせを受けたら、識別子（先頭 2 バイト）を中継の番号に付け替え、Wi-Fi の DHCP で得た DNS サーバの 53 番へ、Wi-Fi の UDP 53053 番から送る。答えが来たら、識別子を元に戻して、問い合わせた機器へ返す。
- 待っている問い合わせは 16 個まで、5 秒で捨てる。Wi-Fi に DNS サーバがまだないときの問い合わせは捨てる（聞いた側が送り直す）。中身は見ず、覚えもしない（キャッシュなし）。
- 数は `net.dns_relayed`（中継した数、捨てた数）で、1、2、4、8… 回目に出す。
- フィルタ: 中継は Tab5 自身のソケットなので、LINK の UDP 53 番は `in` の規則で通す必要がある（`netctl 6` の組には規則がないので通る）。
- 試験用 LAN の相手: DHCP で受けた DNS サーバに `one.one.one.one` の A を一度問い合わせ、`net.peer_dns` で出す。

## 3. 結果（2026-11-01、実機、`run net 3`、`run netctl 6`）
```
net.dhcp_server link=test0 first=192.168.50.100 last=192.168.50.149
net.dns_relay link=test0
net.dhcp_lease mac=027435000002 addr=192.168.50.100 seconds=3600
net.peer_dhcp addr=192.168.50.100 router=192.168.50.1 dns=192.168.50.1
net.dns_relayed count=1 dropped=0
net.peer_dns name=one.one.one.one ip=1.1.1.1 ms=17
```
相手は DNS サーバとして Tab5（192.168.50.1）を受け取り、中継を通して 17 ms で名前を解決した。

## 4. まだ
- IPv6: 下流の RA に RDNSS を載せる。中継を IPv6 でも待つ。→ doc/46 §2（上流の RDNSS を下流の RA に載せる。中継は IPv6 の問い合わせももとから受けていた）。
- TCP の DNS（大きな答え）。今は UDP だけ。
- キャッシュ。
- ホストリンク（PC）向けの中継。今は試験用 LAN だけ。
