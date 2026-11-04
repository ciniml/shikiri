# 52. アプリの権限とアプリ間通信のアクセス制御（検討）

利用者の指示（2026-11-05）:「アプリケーションの権限と、アプリケーション間通信の権限の確認を行う仕組みを考えたいです。wifi/ble/net/fsなどにアクセスする必要がないアプリからはアクセスできなくする、一部のみアクセスを許可するアクセスコントロールの仕組みです。検討してください。」、「ちなみにあるアプリケーションは別のアプリケーションのRAMを読むことができますか？」

## 1. 今あるもの（層ごと）
| 層 | 何を守るか | 仕組み | 状態 |
|---|---|---|---|
| L0 メモリ | ほかのプロセスとカーネルのメモリ | U モードと PMP（§6） | ある |
| L1 オブジェクト | デバイス、`proc:*`、`flash:cfg`、`ready:*` | 署名した像の Permissions TLV。`open` で名前が一致しなければ `Denied`、`proc.denied` | ある |
| L2 サービスへの接続 | `svc:NAME` へつなぐ、`listen:NAME` で名乗る | 同じ TLV | ある |
| L3 サービスの中の操作 | fs のどのパス、net のどのポート、cfg のどのキー | なし | **ない** |

- **L2 で既にできること**: `svc:net` の許可のないアプリは net を使えない、など。今の許可は Makefile の `PERMIT_<app>` にある。
- **L3 がない**: L2 を通れば、そのサービスのすべてが使える。例:
  - `svc:fs`: SD 全体（`matter` も `T5OS/START.CFG` や `NET.RUL` を書き換えられる）。
  - `svc:net`: どのポートでも、`LINK_RENEW`（Wi-Fi の DHCP のやり直し）まで。
  - `svc:ble`: スキャン、セントラル、ペリフェラルの全部。
  - `svc:wifi`: 網を変えられる（`JOIN`、`SAVE`）。
  - `svc:matter`: ペアリングや Wi-Fi の資格情報まで。
  - `proc:kill`: どのプロセスでも止められる。
- **原因**: サーバはクライアントが誰かを知らない。カーネルは `accept` でチャネルを渡すだけ。そのため `cfg` は名前を 2 つ（`cfg`、`cfgsec`）に分けて代わりにした（doc/50）。サービスごとにこれを繰り返すのは広がらない。
- **名乗りの偽装は防げている**: `svc:fs` に答えられるのは `listen:fs` の許可を持つ像だけ。クライアントは、相手が本物のサーバだと信じてよい。

## 2. 方針
1. **許可は今までどおり、署名した像の中に書く**（ビルド時に決まり、改ざんできない）。書き方を広げ、サービスの中の範囲（grant）も書けるようにする。
2. **カーネルは、接続してきたクライアントの身元と grant を、サーバに渡す**。カーネルは grant の意味を知らない（ただの文字列）。
3. **確かめるのはサーバ**: 操作の意味を知っているのはサーバだけだから。サーバは `accept` のときに grant を読んで覚え、要求ごとに照らす。外れたら断り、`<svc>.denied` をログに出す。
4. **既定は拒否**: grant のない操作はできない。ただし移行のあいだは、grant を 1 つも書いていない古い形の許可（`svc:NAME` だけ）を「全部」とみなし、`svc.legacy_client` で知らせる（§5）。

## 3. 仕組み
### 3.1 許可の書き方（Permissions TLV）
- `svc:NAME`: 今までどおり。つなぐだけ。移行が終われば grant なしになる。
- **`svc:NAME/GRANT`**: つなぐこと、と GRANT。例 `svc:fs/rw:T5OS/MATTER`、`svc:net/udp:5540`。
  - カーネルは、`svc:NAME` か `svc:NAME/…` が 1 つでもあれば接続を許す。
- `elf2t5a --permit 'svc:net/udp:5540'`。Makefile は、アプリごとの一覧ファイル（`user/apps/NAME/permits`。1 行 1 つ）から読む形に移す。レビューで差分が見えるように（判断）。

### 3.2 カーネルからサーバへ
- 新しい ctl（ABI 小版 20）、チャネルのサーバの端で:
  - `PEER_PID`: 相手の pid。
  - `PEER_INFO`: 相手の像の名前と build_id（`read` の形で）。
  - `PEER_GRANTS`: 相手の Permissions TLV のうち、このサービスの名前で始まるものの GRANT の部分を、NUL 区切りで。
- 中身は、ローダが署名を確かめた TLV を、起動のときにプロセスの構造体に写したもの（512 バイトまで）。ストアの像が入れ替わっても変わらない。
- `t5_rt`: `Channel::peer_grants()`、`Grants::allows(prefix)` など、サーバ側の小さな助け。
- 身元は**サーバへの接続ごとに固定**: チャネルの端をほかのプロセスに渡す仕組みは今はない。将来足すなら、渡したときの扱いを決める。

### 3.3 サービスごとの grant（案）
| サービス | grant | 今の使い手と必要なもの |
|---|---|---|
| `fs` | `r:PREFIX`、`rw:PREFIX`（8.3 の大文字で比べる。`..` を断り、パスを正規化してから） | `matter` `rw:T5OS/MATTER`、`init` `r:T5OS/START.CFG`、`netctl` `rw:T5OS/NET.RUL`、`cfg` `rw:T5OS/CONFIG`（引き継ぎ）、`mtbench`・`fstest` `rw:T5OS/TEST`、`shell` `rw:`（全部） |
| `net` | `udp:PORT[-PORT]`、`mcast`（`UDP_JOIN`）、`tcp-connect[:PORT]`、`tcp-listen:PORT`、`ping`、`resolve`、`info`（`IF_INFO`）、`link`（`LINK_RENEW`） | `matter` `udp:5540`、`udp:5353`、`mcast`、`info`。`shell` `tcp-connect`、`ping`、`resolve`、`info`。`wifi` `link`。`nettest` 全部 |
| `wifi` | `status`、`join`（`JOIN`、`SAVE`、`REVERT`） | `matter` `status`、`join` |
| `ble` | `scan`、`central`、`peripheral` | `matter` `scan`、`central`、`peripheral`。`blescan` `scan`。`blegatt` `central`。`bleecho` `peripheral` |
| `cfg` | `r:PREFIX`、`w:PREFIX`、`secret:PREFIX`（秘密の値を読む） | `wifi` `r:wifi.`、`w:wifi.`、`secret:wifi.`。`shell` `r:`、`w:`（秘密は書けるが読めない）。`cfgctl` `r:`、`w:test.`、`w:wifi.`（消すため）。**`svc:cfgsec` は要らなくなる** |
| `matter` | `read`（status、nodes、read、sub）、`control`（on/off/toggle、invoke、write）、`admin`（pair*、ocw、revoke、forget、netcred） | `shell` 全部、`mtreq` 全部（試験） |
| `netctl` | 今のまま（`svc:netctl` そのものが管理の口） | `netctl` |
| `font`、`tty` | 今のまま | `term`、`shell` |

- `proc:kill`: カーネルのオブジェクト。`proc:kill/own`（自分が起動したプロセスとその子だけ）と `proc:kill/any` に分ける。カーネルに親の pid を持たせる。`shell` は `own`。
- `proc:spawn`: `proc:spawn/NAME` で起動してよいアプリを絞れるようにする（`init` は全部、`term` は `shell` だけ）。
- `netdev:*`、`wlan:ctl`、`bt:hci`、`sd`、`flash:cfg`、`listen:*` は今のままで足りる（持つのは 1 つのサービスだけ）。

### 3.4 fs の既定の場所（案）
- どのアプリにも、grant なしで `T5OS/APPS/<アプリの名前>/` を読み書きさせる（自分のデータの場所）。
- 名前はカーネルの `PEER_INFO` から取るので、偽れない。
- 8.3 の制限: アプリの名前は 8 文字までに切る（衝突は避けられないので、ストアに入れるときに 8 文字で一意かを確かめる）。

### 3.5 サーバの側で守ること
- `accept` のときに grant を読み、クライアントの表に覚える（要求ごとにカーネルを呼ばない）。
- 断るときは、決まりごとに既にある「失敗」の答え（`fs::status::DENIED` のように 1 つ足す）を返す。`<svc>.denied app=… op=… arg=…` をログに出す。秘密や passphrase を `arg` に出さない。
- サーバそのものは信頼する（`listen:NAME` を持つ像は、その資源を全部持つ）。サーバのバグは、そのサービスの範囲で権限を越える。サーバは小さく保つ。

## 4. 信頼の元
- 今は開発用の鍵 1 つで署名した像は、どんな許可でも名乗れる。
- 次の段（案）: 鍵を 2 つに分ける。
  - **システムの鍵**だけが、強い許可（`listen:*`、`sd`、`wlan:ctl`、`bt:hci`、`netdev:*`、`flash:cfg`、`proc:kill/any`、`svc:cfg/secret:*` など）を名乗れる。
  - **アプリの鍵**の像が名乗れば、ローダが断る。
- 利用者が実行時に許す・取り上げる仕組み（同意の画面、設定の上書き）は、その後。上書きは「署名した許可より狭くする」だけに限る（判断）。

## 5. 段取り（案）
| 段 | 内容 | 試験 |
|---|---|---|
| A1 | カーネル: 接続の許可の前方一致（`svc:NAME/…`）、grant の写し、`PEER_PID`/`PEER_INFO`/`PEER_GRANTS`（ABI 小版 20）。`t5_rt` の助け | t5emu: grant の違う 2 つの試験アプリが同じサービスにつなぎ、サーバが違う grant を読む |
| A2 | `cfg` を grant に（`svc:cfgsec` をやめる）、`fs` のパスの grant と `T5OS/APPS/<name>/` | 範囲の外のパス・キーが断られる、`fs.denied`/`cfg.denied` |
| A3 | `net`、`wifi`、`ble`、`matter` の grant | 許されないポートの `UDP_BIND`、`JOIN`、`SCAN` が断られる |
| A4 | grant のない古い形を「全部」とみなすのをやめる（既定は拒否）。`proc:kill/own`、`proc:spawn/NAME` | 全アプリの一覧ファイルを見直し、実機で常駐の組み合わせが動く |
| A5 | 鍵を 2 つに（§4） | アプリの鍵で `listen:fs` を名乗る像が入らない |

- 各段の後に、Makefile（一覧ファイル）の許可を最小に絞り直す。
- 自己試験に、ほかのプロセスのスロットを読む `fault/load-other-slot` を足す（§6、今は試験がない）。

## 6. あるアプリは、ほかのアプリの RAM を読めるか
**読めない**（設計と実装の上では）。
- プロセスは U モードで動き、PMP の範囲は次のものだけ（`arch/riscv32/pmp.rs`）。ほかはどこを読んでも、アクセスフォールトで止まる。
  - 自分の text（R-X）
  - 自分の data とスタック（RW-）
  - 渡された共有の窓
- 自己試験で、カーネルの RAM、MMIO、カーネルのフラッシュ、text への書き込み、データの実行が止まることを、起動のたびに確かめている（`fault/*`）。
- ただし、ほかのプロセスのスロットを読む試験はまだない（§5 で足す）。
- システムコールに渡すポインタは、カーネルが自分の範囲かを確かめる（`check_user_range`。外なら `BadAddress`）。カーネルに代わりに読ませることもできない。
- スロットの使い回し: ローダは、新しいプロセスが触れる範囲（text、data、bss、スタック）を全部書いてから渡す（text は写し、残りは初期値か 0。`loader.rs:156-160`）。共有メモリの窓も渡す前に 0 で埋める（`shm.rs:102`）。前のプロセスの残り（`wifi` の passphrase など）は見えない。
- 例外（決めて共有しているもの）:
  - **共有メモリの窓**: `fs` は各クライアントに 64 KiB の窓を渡す。その窓は `fs` とそのクライアントの 2 者が読み書きできる（ほかのクライアントは読めない）。`net` のフレームのリングは `net` だけ。
  - **フレームバッファ**: `display` を開いた 1 つのプロセスだけ。
  - **カーネル**（M モード）はすべてを読める。
  - **デバッガ**（USB-JTAG のプローブ）はすべてを読める。プローブで止めた後は、隔離を信用できない（メモリ: プローブで止めるとプロセスが M モードで動きうる）。製品にするなら JTAG を閉じる（doc/51 §2.3 と同じ話）。
  - タイミングやキャッシュの横の通り道は考えていない。
