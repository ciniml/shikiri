# 41. シェルの `ifconfig`・`top`・`nc`

利用者の指示（2026-11-03）:「よさそう。すすめて。」（doc/40 の `ps`・`apps`・`kill` の後）。シェルから、ネットワークの状態、CPU の使い方、TCP の相手との会話を見られるようにした（判断: 判断を仰ぐほどの分かれ道がない、小さな続き）。

## 1. ネットワークサービスの `IF_INFO`（`t5_abi::net`）
- 要求の `op` に `IF_INFO`（14）を足した。`a` はインターフェースの番号（0 = `host`、1 = `wlan0`、2 = `test0`、`fwd::LINK_NAMES` の順）。
  - 答えの v0 は、今そのインターフェースがあれば 1（なければ 0）。v1 は、スライスに置いた記録のバイト数。最後より先の番号には `NO_ADDRESS`。
  - 記録（`net::iface`）: 名前（8）、MAC（6、なければ 0）、IPv4 のプレフィックス長、IPv6 アドレスの数、IPv4 アドレス、ルータ、DNS サーバ、受けたフレーム数、送ったフレーム数、続いて IPv6 アドレスごとに 16 バイト + プレフィックス長 + 3 バイトの 0（最大 4 つ）。
  - 値は smoltcp の `Interface::ip_addrs()` と、`EthLink` の `router`・`dns_server`、`EthNic` の `rx`・`tx`（`host` は `Links` の `from_host`・`to_host`）から取る（`user/apps/net/src/main.rs` の `if_info`）。
- `t5_rt::Net::if_info(k)` が、記録をスライスの先頭に置く。
- 権限は `svc:net` のまま（どのクライアントも見られる）。アドレスは秘密ではないので、これでよいとした（判断）。

## 2. シェル
| コマンド | 内容 |
|---|---|
| `ifconfig` | インターフェースごとに MAC、IPv4/プレフィックス、ルータ、DNS、IPv6 アドレス（ゼロの続きは `::`）、フレーム数 |
| `top` | 1 秒ごとに画面を書き直し、プロセスごとの CPU 使用率（1 コアを 100%、前の回との `cpu_ms` の差）と CPU 時間を出す。キーを押すと終わる |
| `nc HOST PORT` | TCP でつなぐ（名前は DNS で引く）。届いたものを表示し、打った行を Enter で CRLF をつけて送る。Ctrl-C か Ctrl-D で終わる。相手が閉じれば終わる |

- `top` は `proc:list`（doc/40）を毎回開き直して、新しい写しを得る。
- `nc` は行ごとに送る（Backspace はローカルで消す）。届いたデータの LF は CRLF にして表示する。
- キーの待ち方: `svc:tty` のチャネルを、時間切れつきの `wait` で待ってから読む（`keys`）。

## 3. 試験
- `make test-virt`、`make test-p4emu` が通ること（これまでの試験。`ps`・`apps`・`kill` は doc/40 の場合で確かめる）。
- `ifconfig`・`top`・`nc` は、実機で確かめる。
