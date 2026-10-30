# 20. N1: ネットワークサービス（ループバック）

[research/net-stack.md](research/net-stack.md) の段階 N1。利用者の方針（2026-10-31）: スタックは U モードのサービス。カーネルは変えていない（N0 のサービスチャネルと共有メモリだけを使う）。

## 1. 構成
- アプリ `net`（`--permit shm --permit listen:net`、スタック 32KiB）: smoltcp 0.14.0（0BSD。features: `medium-ip`、`proto-ipv4`、`socket-tcp`、`socket-udp`。alloc なし）。
  - リンクはループバックだけ（127.0.0.1/8、`Medium::Ip`、MTU 1500）。smoltcp の `phy::Loopback` は alloc（`VecDeque<Vec<u8>>`）を使うので、16 フレームの固定の輪を自作した。輪が一杯のときに送られたフレームは捨てる（実際のリンクと同じ）。
  - ソケットは 8 個まで。バッファは静的（TCP は送受 16KiB、UDP は送受 8KiB・16 パケット）。`net` のメモリは約 470KB（アプリの 6MiB のスロットに収まる）。
  - クライアントは 4 人まで。各自に共有メモリの 64KiB の切れ端を渡し、データはそこを通す。要求と返事は 12 バイトのメッセージ（`t5_abi::net`）: TCP_LISTEN、TCP_CONNECT、TCP_STATE、SEND、RECV、CLOSE、UDP_BIND、UDP_SEND、UDP_RECV。
  - **サービスの中では待たない**。SEND/RECV は、今できる分だけ（0 もある）を答え、クライアントがやり直す。`t5-rt` の待つ版は 1ms 眠ってやり直す。
  - 待ち: 要求のチャネルとリスナを `wait` し、時間切れは smoltcp の `poll_delay`（上限 50ms。再送などのタイマのため）。
  - クライアントが去ったら（`PEER_CLOSED`）、その人のソケットを中断する。閉じた TCP のソケットは、smoltcp が終わる（Closed/TimeWait）まで残してから空ける。
- `t5-rt` の `Net`: `tcp_listen`、`tcp_connect`、`tcp_state`、`wait_established`、`send`（できる分）、`send_all`、`recv_now`（待たない）、`recv`（待つ。0 は相手が閉じた）、`close`、`udp_bind`、`udp_send`、`udp_recv`。
- 試験アプリ `nettest`（`--permit svc:net`）: 127.0.0.1 で UDP の 2 つのソケットの間を 100 往復（長さを変え、送り元のアドレスとポートも確かめる）。TCP は自分の待ち受けにつなぎ、1MiB を送りながら受け、送った分と受けた分の FNV-1a 64 ビットのハッシュを比べる。最後に相手側が終わり（0 バイト）を受けることを確かめる。
- smoltcp の待ち受けのソケットは、つながるとそのまま接続のソケットになる（受け取りの待ち行列はない）。次の接続を待つには、もう一度 `tcp_listen` する（今の API の制限）。

## 2. 結果
- t5emu（40MHz）: UDP 100 往復（平均 3.7ms）、TCP 1MiB が 1.6 秒（621KiB/s）、ハッシュ一致。`make test-p4emu` に入れた。
- 実機（360MHz、2026-10-31）: UDP 100 往復（平均 0.55ms）、**TCP 1MiB が 209ms（約 4.9MB/s ≒ 39Mbps）**、ハッシュは t5emu と同じ値で一致。研究ノートの見込み（10〜30Mbps）より速い。ループバックなので、リンクの速さはまだ入っていない。インストールと実行は、利用者が足した許可（`t5log rtt --probe <Tab5>`）で Claude が行った。

## 3. まだ
- `net` を kill して起動し直したときの、クライアントのつなぎ直し（research の N1 の完了条件の一つ）。
- 待ち受けの受け取りの待ち行列（listen backlog）。
- 次は N2a（USB Serial/JTAG の上のホストリンク）。
