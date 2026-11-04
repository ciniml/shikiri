# 50. 設定のサービス `cfg`

利用者の指示（2026-11-05）:「SDやP4側のNVSなどに設定を保存する仕組みを作る必要がありそうですね。簡単な物でいいので設定を管理するプロセスを設計して、そこで管理するようにしてください。」

きっかけは doc/49 §20 の残り: Matter のコミッショナが渡した Wi-Fi の網を `wifi` が覚えておけず、`wifi` の起動し直しや再起動で、ビルド時に埋め込んだ網（`~/.config/tab5-os/wifi.txt`）へ戻っていた。

## 1. 方針
- **プロセスは 1 つ**: `cfg` が名前つきの値（キーと値）を持ち、ほかのサービスは `svc:cfg` で読み書きする。ファイルの形や書き方は `cfg` の中だけにある。
- **保存先は SD カード**（`svc:fs` 経由。判断）。
  - P4 のフラッシュには `nvs` の区画（0x9000、16 KiB。boot/partitions.csv）と `userdata`（0xC20000、約 4 MiB）がある。ただ、カーネルが書いてよいのはストア、otadata、使っていないカーネルのスロットだけ（kernel/src/flash.rs の `writable`）。
  - フラッシュに置くには、カーネルの新しいオブジェクトと、まだ書いたことのない区画への書き込みが要る。tab5-hw-safety のメモの範囲の外なので、今回は見送った。
  - SD なら、今ある `svc:fs` と、`mtport::FileKvs` で確かめた A/B の書き方がそのまま使える。
  - `cfg` の外の決まりごと（`t5_abi::cfg` の要求と答え）は保存先に依らない。フラッシュに移すときは `cfg` の中だけを替える。
- **秘密の値**: Wi-Fi の passphrase のような値は、`SECRET` の印をつけて持つ。
  - `svc:cfg` のクライアントは、秘密の値を書けるが、読めない（あることと印だけが返る）。
  - 読めるのは `svc:cfgsec` のクライアントだけ。`svc:cfgsec` の許可は `wifi` にしか与えない（Makefile の `PERMIT_wifi`）。
  - カーネルはサーバにクライアントの身元を教えないので、サービスの名前を 2 つに分けて、カーネルの許可（`--permit svc:…`）で読める者を決める（判断）。
  - 値はログに出さない。キーだけを出す（`cfg.changed key=…`）。
  - SD のファイルの中では平文。カードを抜けば読める（§6）。

## 2. 決まりごと（`t5_abi::cfg`）
要求 1 つに答え 1 つ。クライアントは 1 つずつ要求を送る。

| 要求 | 答え |
|---|---|
| `[GET, key_len, key…]` | `[VALUE, flags, value…]`（`svc:cfg` で秘密の値なら値なし）か `[NONE]` |
| `[SET, flags, key_len, key…, value…]` | `[OK]` か `[ERR, code]` |
| `[DEL, key_len, key…]` | `[OK]`（なかったときも）か `[ERR, code]` |
| `[LIST, index]` | `[ENTRY, flags, key_len, key…, value…]`（秘密は `GET` と同じ）か `[END]` |

- キーは 1〜32 バイトの `a-z 0-9 . _ -`。値は 0〜128 バイト。表は全部で 4 KiB まで。
- 一度秘密にしたキーは、消すまで秘密のまま（印なしの `SET` で上書きしても）。
- `ERR` のコード: `BAD`（形が違う、キーに使えない文字）、`TOO_LONG`、`FULL`、`NOT_SAVED`。
  - `NOT_SAVED` は、メモリの中では変わったが SD に書けなかったこと。`cfg` が終われば消える。
- `t5_rt::Config` がクライアントの側: `connect(secret)`、`connect_within(secret, ms)`（起動の直後は `cfg` がまだいないことがある）、`get`、`set`、`remove`、`entry`。

## 3. ファイル
- `T5OS/CONFIG.A` と `T5OS/CONFIG.B`。中身は表全体（`cfg::Table::image`）:
  - 先頭 16 バイト: `"T5CF"`、世代（u32）、長さ（u16）、0（u16）、レコードの CRC-32（u32）。
  - レコード: `[flags, key_len, value_len, key…, value…]` を並べる。
- 変えるたびに、新しいほうでない方のファイルへ、世代を 1 つ上げて全体を書く。書いている途中で切れても、もう一方が前の値のまま残る（`mtport::FileKvs`、アプリのストアのカタログと同じ考え）。
- 起動したときは、両方を読み、壊れていない新しいほうを使う。壊れたほうは `cfg.bad_copy` で知らせる。
- `svc:fs` のクライアントは 4 つまで（`fs` の `CLIENTS`）。`cfg` は読むときと書くときだけつなぐ。
- 書き方の試験は `t5_abi` のホストのテスト（`cd abi && cargo test`: 値の置き換え、秘密の印が残ること、上限、表が一杯、ファイルの往復、壊れたファイル、世代の比較）。

## 4. `cfg` と道具
- `run cfg MODE`:
  - 0: SD の値。SD が読めなければ値なしで始め、変えると `NOT_SAVED` を返す。起動の直後は `fs` を 10 秒まで待つ。
  - 1: メモリの中だけ（t5emu には SD がない）。
- 許可は `--permit listen:cfg --permit listen:cfgsec --permit svc:fs`。
- `init` の組み込みのリストの先頭に `cfg` を足した（`wifi` が起動の直後に網を読むため）。SD の `T5OS/START.CFG` は `run init 1` で書き直す。
- シェルの `cfg`: `cfg`（一覧。秘密は `(secret)`）、`cfg get KEY`、`cfg set KEY [VALUE]`（値は残りの語を空白 1 つでつなぐ）、`cfg secret KEY VALUE`、`cfg rm KEY`。
  - シェルは `svc:cfg` だけなので、秘密の値は読めない。`cfg secret` で打った値は画面には出る。
- `cfgctl`（数だけで動かす試験用。`--permit svc:cfg`）:
  - 0: 自己試験（`test.*` の値で、設定、読み、置き換え、秘密の値が返らないこと、上限、一覧、消す）。最後に `test.kept` = `1` を残す。
  - 1: 全部のキーをログへ（`cfgctl.entry`。値は秘密でないものだけ）。
  - 2: `wifi.ssid`、`wifi.psk`、`test.kept` を消す（`wifi` は次の起動から組み込みの網へ）。
- カーネル: サービスの listener の上限を 8 から 16 に（QEMU の virt は RAM が足りず 8 のまま。virt で動かすサービスは少ない）。実機の常駐（fs、net、netctl、wifi、font、tty、ble、matter）で 8 が埋まっていて、`cfg` の `Listener::open` が `NoMem` になった（`cfg.failed step=1`）。

## 5. Wi-Fi の網を覚える
- `wifi` は起動したとき、`svc:cfgsec` で `wifi.ssid` と `wifi.psk` を読む（`cfg` を 3 秒まで待つ）。あればその網に、なければビルド時の網につなぐ（`wifi.configured ssid=… from=settings|built-in`）。
- `svc:wifi` に 2 つの要求を足した:
  - `SAVE`: 最後の `JOIN` の網が今つながっている網なら、`wifi.ssid` と秘密の `wifi.psk` に書き、次の起動からその網へ。答えは `[SAVED, ok]`。
  - `REVERT`: `JOIN` で移った後なら、起動したときの網（または保存した網）へ戻る。答えは `JOIN` と同じ `JOINED` か `FAILED`。
- `JOIN` だけではまだ保存しない。Matter では、ConnectNetwork は fail-safe の中で行われ、コミッショニングが終わらなければ取り消されるべきものだから（判断）。
  - `matter` は、コアの `on_commissioning_complete` のフックで `SAVE`、fail-safe が CommissioningComplete の前に終わったら（期限切れか ArmFailSafe(0)）`REVERT` を送る。どちらも、この `matter` が `JOIN` を送った後のときだけ。
  - 同じ周回で CommissioningComplete の後に fail-safe の終わりが来ても、保存は取り消さない（`Device::take_commissioning`）。
- passphrase が通るのは、`matter` → `wifi` → チップと、`wifi` ↔ `cfg` ↔ SD だけ。ログには出さない。

## 6. 実機（2026-11-05）
- `cfgctl 0` の自己試験: SD に書けた。1 回の保存は約 8 ms。`cfg` を起動し直すと、世代 9 を A から読み、`test.kept` が残っていた。
- **Matter で渡された網が残る**:
  - smctl の `pairing ble-wifi` で、Tab5 を BLE でコミッショニングし、`matter-test` を渡した。CASE まで 12.8 秒。
  - `wifi.join` → `matter-test` に 0.36 秒でつながる → CommissioningComplete の後に `wifi.saved ok=true`（`cfg.changed key=wifi.ssid`、`key=wifi.psk`）。
  - `cfgctl 1`: `wifi.ssid` = `matter-test`、`wifi.psk` は `secret=true` で値なし。
  - `wifi` を起動し直すと `from=settings` で `matter-test` へ。smctl から `matter-test` の上で toggle できた。
  - **再起動**: `cfg` が最初に起動し（`START.CFG` は `run init 1` で書き直した）、起動から 6.4 秒で `matter-test` につながった。`matter` を起動して toggle できた。
  - smctl の出力と RTT のログに passphrase は 0 回。
- 戻し: 試験の fabric（ノード 7）を unpair、`cfgctl 2`、`wifi 1` を起動し直して `from=built-in` で `iotap`。fabric 1 の toggle 0.45 秒。
- 気づいたこと:
  - 再起動の後、`netctl` は `T5OS/NET.RUL` の規則 4 つを入れる。今の守りの組（`netctl 2`）は 6 つ（Matter の UDP 5540 と 5353 が増えた）。再起動のたびに `netctl 2` が要っていた。利用者の承認を得て `run netctl 7` で `NET.RUL` を書き直した（doc/51）。
  - `ble` を `wifi 1` が BT を起動する前に起動すると、`ble` は CPU を使い切り、広告しない（前からの問題。BT が起動してから `ble` を起動し直せば直る）。
  - `wifi` を起動し直すと `wifi.bt_failed step=init`（esp_err 259）。前の `wifi` が起動した BT がチップに残っているためで、BLE は動く。
  - `make install-apps` のログに t5log の stderr（要求の番号）が入らず、入らなかったアプリの名前が `?` になっていた。stderr も `tee` に通した。

## 7. 残り
- 置き場所（P4 のフラッシュ）、暗号化、C6 に残る網は doc/51 で検討した。
- `REVERT`（fail-safe が CommissioningComplete の前に終わったとき）は、実機でも t5emu でも、まだ通していない。
- フラッシュ（`nvs` の区画）への保存。カーネルの新しいオブジェクトと、新しい区画への書き込みの判断が要る。
- 秘密の値は SD の中で平文。暗号化するなら鍵の置き場所（eFuse、フラッシュの暗号化）から決める。
- C6 は、`wifi` が送った網（passphrase を含む）を自分のフラッシュ（NVS）にも覚えている（推測。`wifi_init_config` の `nvs_enable` = 1）。`cfgctl 2` はこれを消さない。
- 値が変わったことをクライアントに知らせる仕組みはない（読むのは起動のときだけ）。
