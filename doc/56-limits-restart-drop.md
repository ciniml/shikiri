# 56. プロセスごとの上限、落ちた常駐の立て直し、権限を手放す

利用者の指示（2026-11-06）:「構造がかなりマイクロカーネルOSぽくなってきましたが、他のOSと比較して権限管理まわりや保護機構はどうですか？ QNXなど参考になりそうと思っています。」への答えで、3 つの改善を勧めた。続けて「終わったら権限周りの改善を提案通りすすめてください。」

## 1. 比べて足りなかったもの
doc/52 の権限（署名した許可、接続相手の身元、サービスの中の grant、鍵を 2 つに）は、QNX（procmgr abilities、secpol、resmgr の `ConnectClientInfo`）、Fuchsia（マニフェストとハンドル）、Tock（TBF の Permissions）と同じ筋で、既定で拒否になっている。比べて足りなかったのは次の 3 つ:

1. **資源の上限**: チャネルは全体で 32、`fs` のクライアントは 4。どちらもプロセスごとの上限がない。正しい許可を持つアプリでも、開き続けるだけでほかを止められた。QNX の rlimit、Zircon の job の policy に当たるものがない。
2. **落ちたサービスを立て直さない**: 常駐（`net`、`matter` など）が落ちると、それきりだった。QNX の HAM、Minix 3 の reincarnation server、Hubris の supervisor に当たるものがない。
3. **権限を減らせない**: 許可は生まれたときのまま。QNX の ability（使い終わったら手放す、施錠する）、OpenBSD の pledge/unveil に当たるものがない。

CPU の取り分の保証（QNX の adaptive partitioning）は、ここでは入れない（§6）。

## 2. プロセスごとの上限（チャネル）
- **対象はアプリの鍵の像だけ**（判断）。システムの鍵の像（サービス）は、クライアントの数だけチャネルの端を持つので、上限を掛けない。信頼の線は doc/52 §4 の鍵の線に合わせる。
  - `ident` に、像がシステムの鍵か（`system`）を写す（ローダが署名を確かめたときに分かる）。
- 上限（`kernel/src/channel.rs`）:

  | 上限 | 値 | 守るもの |
  |---|---|---|
  | 1 つのプロセスが端を持つチャネル | 8 | 1 つのアプリがチャネルを使い切る |
  | 1 つのサービスへの同時の接続 | 2 | `fs` のクライアント（4）などのサービスの枠を 1 つのアプリが使い切る |
  | アプリの像が持つチャネルの合計 | 全体の半分（Tab5 で 16、QEMU で 8） | アプリが組になって使い切る。残りはサービスどうしの分 |

- 数えるのは `open("chan:…")` と `open("svc:…")` のとき。超えたら新しいエラー `Error::Limit`（17）と `chan.limit`（どの上限か、いくつ持っているか）。
- 今のアプリで一番多いのは `mtpanel` の `matter` への 2 本（エンドポイントの接続と、3 秒ごとの `eps`）。
- メモリはスロット（2 MiB）で、共有メモリの窓（`shm`）はサービスだけが持つので、今は上限を足さない。ログを出しすぎるアプリは `log.dropped` で落とされるだけで、ほかを止めない。

## 3. 落ちた常駐の立て直し
- **カーネルが見張る**（判断）。QNX の HAM はユーザ空間だが、ここでは:
  - `init` はリストを起動し終えると終わる。
  - 起動の順番（Needs、doc/51 §3）も、アプリの更新の確定と巻き戻し（doc/16 §4.2）も、カーネルのサービスのループにある。
  - そこに表を 1 つ足すのが一番小さい（`kernel/src/supervise.rs`）。Hubris の supervisor に近い。
- **見張るもの**: `proc:spawn` のハンドルに `ctl(spawn::RESTART, 1)` をしてから起動したもの。`init` は自分のリスト（モード 0 と 2）でこれをする。ホストの `run` やシェルから起動したものは見張らない。
- **起動し直す条件**:
  - フォールトで止まったとき、または 0 以外のコード（パニックの `0xDEAD` を含む）で終わったとき。
  - 0 で終わったとき（`netctl` のように仕事を終えて終わるもの）と、`kill` で止められたときは見張りをやめる。人が止めたものを起こさない。
- **間合い**:
  - 0.5 秒、1 秒、2 秒、4 秒と延ばす。5 回目の失敗で `proc.restart_gave_up` を出してあきらめる（`spawn::RESTARTS` = 4）。
  - 60 秒動き続けた後の失敗は、数え直す。
- 起動し直すときは、最初と同じ引数と親（`by`）で、Needs を待つ道（`request_start`）を通る。たとえば `net` が落ちると、`matter` は `net` の接続を失って終わり、`svc:net` が戻るのを待ってから起動し直される。
- ログ: `proc.restart_planned`（何回目、何 ms 後）、`proc.restarted`（新しい pid）、`proc.restart_gave_up`。
- アプリの更新との組み合わせ: 確定前の版がフォールトすると前の版へ巻き戻る（doc/16 §4.2）。その後の起動し直しは前の版で動く。

## 4. 権限を手放す（`permit_drop`）
- システムコール `PERMIT_DROP`（0x007、ABI 小版 20）と `t5_rt::permit_drop(entry)`。
  - `entry` そのものと `entry/…` を、カーネルの写し（`ident`）から消す。消えた数を返す。戻す方法はない。
  - 広い許可は残す: `svc:fs` を持つプロセスが `svc:fs/rw:X` を手放しても、`svc:fs` は消えない（「X 以外の全部」は書けないため）。
  - ログ: `proc.permit_dropped`。
- **開いているものはそのまま**（判断、QNX の ability やケーパビリティと同じ）:
  - 開いたハンドルと、サーバが受け付けた接続（サーバは accept のときに grant を読む、doc/52 §3.5）は、手放した後も使える。
  - 手放したもので変わるのは、これから開くもの（`open`、`svc:` への接続、`proc:spawn/NAME` の確かめ、その後の接続での `PEER_GRANTS`）。
  - 使い方は「準備で開く → 要らなくなった許可を手放す → 外から来るものを扱う」。
- 使ったところ: `mtpanel` は、エンドポイントの接続ができたら `display`、`touch`、`svc:matter/endpoint` を手放す。以後に新しく開けるのは `eps`（`svc:matter/read`）だけになる。
- 今の常駐は、使う許可を動いている間ずっと使う。たとえば次のとおりで、手放せるものは少ない。
  - `wifi` は、つなぎ直すたびに秘密の値を読む。
  - `term` は、`shell` を起動し直す。
  - `matter` は、コミッショニングのたびに `wifi/join` を使う。
  - 外からのデータを扱う常駐を分ける（たとえば Matter のコミッショニングの部分を別のプロセスにする）ときに、効いてくる。

## 5. 試験
- **自己試験**（起動のたび、アプリの鍵の `fault`）:
  - `fault/limits`: `chan:q1`〜`chan:q9` を開く。9 つ目が `Limit` になる。
  - `fault/permit-drop`: 次の 5 つを確かめる。
    - `chan:q1` が開ける。
    - `chan:q` を手放しても何も消えない。
    - `chan:q1` を手放すと、`chan:q1` は `Denied` になる。
    - 同じものを 2 度目に手放すと 0 を返す。
    - `chan:q2` は開ける。
- **t5emu**（`make test-p4emu`、`run init 2`）: 試験のリストの `hello 3` は毎回 3 で終わる（`hello` は引数をそのまま終了コードにした）。期待するイベントは次の 3 つ。
  - `proc.restart_planned`
  - `proc.restarted`
  - `proc.restart_gave_up`
- **`fstest`**（実機）: 同じサービスへの 3 本目の接続が `Limit` になる（`fstest.limit_ok`）。
- **実機**: §7。

## 6. 残り
- **CPU の取り分**: QNX の adaptive partitioning に当たるもの。今のスケジューラは、準備のできたプロセスを順に回すだけなので、回り続けるアプリが `term` や `net` の時間を削る。区画ごとの予算を足すのは、別の段にする。
- **権限を狭めて起動する**: 親が子の許可を、署名したものより狭めて起動すること（doc/52 §4 の「狭める上書き」）は、まだない。
- **アプリの見張り**: アプリの鍵の像を見張るか（シェルから起動したものなど）は、まだ決めていない。今は `init` のリストだけ。
- **ログの量**: ログを出しすぎるプロセスを、プロセスごとに抑える仕組みはない（全体で落とすだけ）。

## 7. 実機の記録（2026-11-06、`matter-test`）
- **カーネルの入れ替え**:
  - 1 回目は `kupdate.rejected` になった。自己試験はストアの `fault` を動かすが、それがまだ古く、モード 7 と 8 を知らずに 1 で終わったため。
  - 新しいアプリ（ABI 小版 20）は古いカーネル（19）に入らないので、アプリを先に入れることもできない。
  - 直し: ストアの `fault` が ABI 小版 20 より古ければ、2 つの試験を `selftest.skipped` で飛ばす。
  - 2 回目は `kupdate.confirmed`（slot 0）。
- **アプリ**:
  - `make install-apps` は、font のほかは全部入った（font は `store full` でいつもどおり、中身は変わっていない）。
  - `fault` はストアの一覧に入っていない（起動のときの像から来たもの）。`install` は `catalog full` で断られたので、`remove fault` と `install` で入れ直した。
  - `run fault 0`（0 で終わる）も通した。
- **起動し直した後**:
  - 自己試験の `fault/limits`（`chan.limit limit=channels held=8`）と `fault/permit-drop` が通った（`failures=0`）。
  - `init` のリストは、cfg、wifi、net、netctl、font、term、matter、ble の順に起動した。
  - 起動の後、約 70 秒の間に、`netctl` は 0 で終わり（見張りをやめる）、ほかに `proc.restart_*` は 1 つもなかった。
- **シリアルから**:
  - `run fstest`: `chan.limit name=fs limit=connections to the service held=2` と `fstest.limit_ok`。100 KiB の書きは 1143 ms、読みは 105 ms。
  - `kill`（`term`）: `shell` は 0 で終わった。`term` は `kill` で止めたので、起動し直されなかった。
  - `run mtpanel 8`: `proc.permit_dropped` が 3 つ（`display`、`touch`、`svc:matter/endpoint`、それぞれ 1 つずつ）出た後で、`mtpanel.ready ep=4`。
- **画面の自動の試験**（doc/55、`tools/guitest-mtpanel.py --smctl …`）: 手放した後の `mtpanel` で **12/12**。3 秒ごとの `eps` の一覧も正しく描かれ、`proc.denied` はなかった。
