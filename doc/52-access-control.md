# 52. アプリの権限とアプリ間通信のアクセス制御

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

## 7. 実装（2026-11-05）
利用者の判断:「1, 2, 3すすめて。」（§5 の A1 から、`T5OS/APPS/<名前>/`、鍵を 2 つに。A1〜A5 を一度に入れた。アプリは全部この中にあるので、古い形の許可を「全部」とみなす移行の段は飛ばし、初めから既定は拒否にした（判断））

### 7.1 カーネル
- **許可の一致**（`t5a::permits`）: 名前そのもの、または `名前/…`。
  - `t5a::permits_all`（名前そのもの）、`t5a::grants`（`名前/` の後ろ）。
- **`ident`**: 起動のときに、プロセスの pid、親の pid、アプリの名前、署名した Permissions TLV（512 バイトまで、`chan::GRANTS_MAX`）を写す。終われば消す（`Process` の `Drop`）。
  - `open` の許可はこの写しで確かめる。前はストアの像を指していたので、動いている間に像が入れ替わると変わりえた。
  - `proc:spawn` で起動したものには親を記録する（`proc.spawned` の `by`）。
- **接続相手**: サービスのチャネルの ctl。
  - `PEER_PID`
  - `PEER_NAME`（バッファへ）
  - `PEER_GRANTS`（このサービスの `svc:NAME/GRANT` の GRANT を NUL 区切りで）
  - `t5_rt` に `Channel::peer_pid`、`peer_name`、`peer_grants` と `Grants`（`has`、`values`、`prefix_of`）。
- **`proc:kill`**:
  - `proc:kill` か `proc:kill/any` はどれでも止められる。
  - `proc:kill/own` は `ident::descends_from` で、自分が起動したもの（とその子）だけ。外は `write` が `Denied`（`proc.kill_denied`）。
- **`proc:spawn`**: `proc:spawn` はどれでも、`proc:spawn/NAME` はその名前だけ（`proc.spawn_denied`）。
- **鍵を 2 つに**:
  - `keys/app-*.pub` はアプリの鍵、ほかはシステムの鍵（`kernel/build.rs`）。
  - ローダは、アプリの鍵の像が強い許可（`t5a::strong`）を持てば断る（`LoadError::StrongPermit`、`loader.strong_permit`）。
  - カーネルの更新（T5K）はシステムの鍵だけ（`kupdate` の "app key"）。
  - 強い許可:
    - そのものの名前: `sd`、`wlan:ctl`、`bt:hci`、`flash:cfg`、`kbd`、`proc:kill`、`proc:kill/any`、`proc:spawn`、`svc:net/link`
    - 前方一致: `listen:*`、`netdev:*`、`ready:*`、`svc:fs/*`（自分のフォルダの外のファイル）、`svc:cfg/secret:*`、`svc:wifi/join*`
- 自己試験を 3 つ足した:
  - `fault/load-other-slot`、`fault/store-other-slot`: 隣のスロットを読む、書く。アクセスフォールト。
  - `verify/app-key-strong`: アプリの鍵で `--permit sd` の `hello-strong` が `StrongPermit` で断られる。
- `elf2t5a` は 512 バイトを超える許可を断る。

### 7.2 サービス
| サービス | grant | 断ったとき |
|---|---|---|
| `fs` | `r:PATH`、`rw:PATH`（成分の単位の前方一致、空は全部）。自分のフォルダ `T5OS/APPS/<名前8文字>`（パスでは `~`、初めて使うときに作る）。許された場所の上のディレクトリは作れる。パスは大文字にし、`.` と `..` は `BAD_NAME` | `status::DENIED`、`fs.denied app op path` |
| `net` | `udp:PORT`、`udp:any`、`tcp-listen:PORT`、`tcp-listen:any`、`tcp-connect[:PORT]`、`mcast`、`ping`、`resolve`、`info`、`link`。自分のソケットへの要求はそのまま | `status::DENIED`、`net.denied` |
| `wifi` | `status`（STATUS）、`join`（全部） | `[DENIED, 要求]`、`wifi.denied` |
| `ble` | `scan`、`central`、`peripheral` | `FAILED` のコード 6、`ble.denied` |
| `cfg` | `r:PREFIX`、`w:PREFIX`、`secret:PREFIX`。`LIST` は読めるキーだけ。**`svc:cfgsec` はやめた** | `err::DENIED`、`cfg.denied app op key` |
| `matter` | `read`、`control`、`admin` | `denied: …` と `END` の失敗、`matter.denied` |

- 各アプリの許可（Makefile の `PERMIT_<app>`）を、使うものだけに絞り直した。
  - `matter`: UDP 5540、5541、5353、`mcast`、`info`、`T5OS/MATTER`、BLE の 3 つ、`wifi` の 2 つ。
  - `shell`: SD 全体、`tcp-connect`、`ping`、`resolve`、`info`、`proc:kill/own`、`cfg` の `r:` と `w:`（秘密は読めない）。
  - `term`: `proc:spawn/shell` だけ。
  - `init`: `T5OS/START.CFG`。`netctl`: `T5OS/NET.RUL`。`cfg`: `T5OS/CONFIG.A` と `.B`（引き継ぎ）。
  - `wifi`: `svc:net/link`、`cfg` の `wifi.` の読み、書き、秘密。
  - `cfgctl`: `cfg` の `r:`、`w:test.`、`w:wifi.`。
  - `fstest`: 何も持たない（自分のフォルダだけ）。
  - `nettest`: `link` のほかの全部。
  - `blescan`/`blegatt`/`bleecho`: それぞれの役割だけ。
- システムの鍵で署名するもの（`SYSTEM_APPS`）: fs、net、wifi、netctl、init、term、shell、ble、matter、cfg、font、echo、keylog、hcitest、mtbench。ほかはアプリの鍵（`~/.config/tab5-os/app-ed25519.key`。`make keys` が作る。リポジトリには公開鍵だけ）。
- `fstest` は自分のフォルダで試し、続けて SD の根、`T5OS/START.CFG`、`T5OS/OTHER` の作成が `DENIED`、`~/..` が `BAD_NAME` になることを確かめる（`fstest.denied_ok`）。
- `cfgctl 0` は、`other.key` の書き込みと削除が `DENIED` になることも確かめる。

## 8. 試験の結果（2026-11-05）
- `make test-virt`、`make test-p4emu` が通った。
  - 自己試験の 3 つ（隣のスロットの読み・書きでアクセスフォールト、アプリの鍵で `sd` を求める像が `StrongPermit`）も通った。
  - shell の試験の `kill 99` は `proc.kill_denied` に変えた（shell が起動したプロセスではない）。
  - QEMU の virt は RAM が足りないので、チャネルを 16 に（1 つ 2 KiB）。
- **実機**: カーネルを入れ替え、全アプリを入れ直した（アプリの鍵の像を含む。入らなかったものなし）。
  - 自己試験: 隣のスロットの読み・書きがアクセスフォールト。`hello-strong` は `loader.strong_permit permit=sd` で断られた。
  - 再起動の後、常駐（cfg、wifi、net、netctl、font、term、shell）は断られることなく動いた。`wifi 1`、`ble`（`ready:bt` を待つ）、`matter` も同じ。
  - `fstest`（アプリの鍵、grant なし）:
    - 自分のフォルダ `T5OS/APPS/FSTEST` で 100 KiB の書き（1163 ms）と読み（125 ms）。
    - SD の根の一覧、`T5OS/START.CFG` の読み、`T5OS/OTHER` の作成は `fs.denied`、`~/..` は `BAD_NAME`。
  - `cfgctl 0`: `other.key` の書き込みと削除が `cfg.denied`。ほかは今までどおり。
  - `mtreq 0`（`read`）、`blescan`（`scan`）、smctl から fabric 1 の toggle（`matter` の UDP 5540）。
  - **BLE のコミッショニングで Wi-Fi を渡す流れ**:
    - smctl の ble-wifi で 12.4 秒（1 回目でつながった）。`matter` は BLE の 3 つと `wifi/join`、`wifi` は `cfg` の `w:wifi.` で保存した。
    - `wifi` を起動し直すと、`secret:wifi.` で封をした passphrase を開いて `matter-test` へ。toggle も通った。
    - unpair、`cfgctl 2`（`w:wifi.`）で消し、`iotap` へ戻した。
    - passphrase は、smctl の出力にも RTT のログにも 0 回。
  - 戻した後: fabric 1 の toggle、ファイアウォール（rules=6、PC からの ping は届かない）。

## 9. 残り
- サーバの中の確かめは、サーバのコードの正しさに頼る。サーバの数は少なく保つ。
- 動いている間の取り上げ、利用者の同意、署名した許可を狭める上書きは、まだない（§4）。
- 8.3 の名前の制限で、アプリの名前の最初の 8 文字が同じだと、フォルダを共有してしまう。ストアに入れるときに確かめる仕組みはまだない。
- `fs` のクライアントは 4 つまで（`CLIENTS`）。アプリが増えれば足りなくなる。
- 開発のあいだは、システムの鍵もアプリの鍵も同じ PC の `~/.config/tab5-os/` にある。
