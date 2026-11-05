# 53. アプリが持ち込む Matter のエンドポイントと、画面（MT5）

利用者の指示（2026-11-05）:「すすめて」（doc/49 の段取りの MT5）。

## 1. 今の形と、足りないもの
- `matter` のデバイスは、エンドポイント 0（ノードのクラスタ）と 1（On/Off のライト）を、`device.rs` の中に固定で持つ。エンドポイント 1 の On/Off は、`matter.onoff` のログに出るだけで、何もつながっていない。
- ほかのアプリが「スイッチ」や「温度センサ」として Matter に出る道がない。
- Tab5 の画面から、Matter の状態を見たり操作したりできない。

## 2. 方針
- エンドポイントは `matter` が持つ。アプリは `svc:matter` につないで「この種類のエンドポイントがほしい」と言い、値を送り、コントローラからの操作を受ける。Matter のクラスタの作り（属性、コマンド、購読、イベント）は `matter` の中で simple-matter のものを使い、アプリは Matter を知らなくてよい。
- 種類は、simple-matter にあるクラスタで作れるものから 5 つ:

  | 種類 | デバイスタイプ | クラスタ（Descriptor のほか） | 値 |
  |---|---|---|---|
  | `light` | On/Off Light 0x0100 | Identify、Groups、On/Off | 0 / 1 |
  | `plug` | On/Off Plug-in Unit 0x010A | Identify、Groups、On/Off | 0 / 1 |
  | `contact` | Contact Sensor 0x0015 | Identify、Boolean State | 0 / 1（StateChange のイベントも） |
  | `temp` | Temperature Sensor 0x0302 | Identify、Temperature Measurement | 0.01 ℃ 単位、`null` |
  | `humidity` | Humidity Sensor 0x0307 | Identify、Relative Humidity Measurement | 0.01 % 単位、`null` |

- 枠はエンドポイント 2〜5 の 4 つ。エンドポイント 1（Tab5 そのもののライト）は今のまま。
- **持ち主**: 枠は「アプリの名前（カーネルが渡す PEER_NAME、doc/52）/ラベル」に結びつけ、KVS（`t5-eps`）に残す。同じアプリが同じラベルでつなぎ直せば、同じエンドポイント番号になる。コントローラ（スマホなど）はエンドポイントの番号で覚えるので、アプリが起動し直すたびに番号が変わってはいけない。
- アプリが切れても、エンドポイントは残る（値は最後のまま。コントローラからの On/Off は受け付け、次につないだときに知らせる）。消すのは管理の要求 `epdel N` だけ。
- PartsList（エンドポイント 0 の Descriptor）: simple-matter の `DescriptorCluster` は PartsList を `&'static` で持ち、変える API がない。使っている枠の組（16 通り）ごとの表を静的に作り、枠が変わったら Descriptor を作り直す。PartsList は購読の対象ではない（コアでも `subscribe: false`）ので、コントローラが新しいエンドポイントを知るのは、読み直したとき（つなぎ直しなど）。

## 3. 約束（`svc:matter`、doc/49 MT2 の続き）
- 許可は `svc:matter/endpoint`（doc/52 §3.3 に足す）。
- 最初の要求 `endpoint KIND [LABEL]`（LABEL は 8 文字まで、なければ `0`）:
  - 答えは `LINE "endpoint N KIND"` と `END ok`。その後も接続は残り、このエンドポイントの接続になる。
  - 断られたら（枠がない、同じ持ち主が別の種類で持っている、許可がない）、`LINE` に理由と `END` not ok。
- アプリ → `matter`: `set VALUE`（10 進、`null`）。答えはない（間違いは `matter.ep_bad` のログ）。
- `matter` → アプリ: `[STATE, テキスト]`（`t5_abi::matter::STATE` = 3）。On/Off の種類では、つないだときと、コントローラが変えたときに `on 0` / `on 1`。
- 要求 `eps`（`read`）: 枠の一覧（番号、種類、持ち主、値、つながっているか）。`epdel N`（`admin`）: 枠を空ける。
- 同時につながっていられるエンドポイントの接続は 4 つ（枠の数）。同じエンドポイントに 2 つ目がつないだら、前の接続を切る。

## 4. 画面（`mtpanel`）
- `display` と `touch` を開き、Matter の状態をタイルで描く。自分も `light` のエンドポイント（ラベル `panel`）を持ち込み、タイルに触ると On/Off が変わり、コントローラから変えられても描き直す。
- `display` を開けるのは 1 つのプロセスだけ（doc/17 §6）で、普段は `term` が持っている。`mtpanel` は `display` が Busy なら終わる。使うときは `term` を止めてから起動する（`START.CFG` で `term` の代わりに置いてもよい）。
- 文字は `term` と同じ Terminus の ASCII（16×32）。

## 5. 段取り
1. `matter`: 枠、種類ごとのクラスタ、KVS、PartsList の表、要求（`endpoint`、`set`、`eps`、`epdel`）、`STATE` の知らせ。
2. 試験のアプリ `mtep`（`run mtep KIND SECONDS`: 種類ごとに値を動かす、受けた `STATE` をログに）。t5emu の試験: `mtep` で `temp` と `plug` を出し、`matter` のコントローラから読みと toggle。
3. `mtpanel`。
4. 実機: chip-tool から新しいエンドポイントの読み、操作、購読。

## 6. 実装の記録（2026-11-05）
- **`matter`**（`endpoints.rs`、`device.rs`、`main.rs`）:
  - 枠は `Slot`（種類、持ち主、Identify、Descriptor、種類のクラスタ）。データモデルの `endpoints()` は、エンドポイント 0 と 1 に使っている枠を足した表を返す。枠が変わるたびに作り直し、エンドポイント 0 の Descriptor も PartsList の表から作り直す。
  - `housekeep` で、枠の On/Off の変化を On/Off の StateChange イベントにし、`Change::EpOnOff` でアプリに `on 0/1` を送る。contact の変化は Boolean State の StateChange イベントにする。
  - 接続: 要求の接続が `endpoint` なら、答えた後にエンドポイントの接続の表（4 つ）へ移す。要求の接続は空くので、ほかの要求は今までどおり 1 つずつ。エンドポイントの接続も `wait` に入れる。
  - 範囲: temp は -273.15〜327.67 ℃、humidity は 0〜100 %、ほかは 0 か 1。外れたら `matter.ep_bad`。temp の Min/Max は -40.00〜85.00 ℃、humidity は 0〜100.00 %。
- **`mtep`**（試験）、**`mtpanel`**（画面）、`mtreq` の OP 19〜23（`eps`、ep 2 の温度、ep 3 の toggle、PartsList、`epdel`）。
- **t5emu**（`make test-p4emu` の Matter の試験に足した）: `mtep 3`（temp、ep 2）と `mtep 1`（plug、ep 3）。PartsList `[1, 2, 3]`、ep 2 の温度 2250、ep 3 の toggle で `matter.ep_onoff` と `mtep.state on 1`。
  - 最初は `mtep` のラベルを全部 `t` にしていて、2 つ目が「同じ持ち主が別の種類」で断られた。ラベルを種類の名前にした。
- **実機**（`matter-test`、doc/49 §24）:
  - `matter` を入れ直して起動し直し、`mtep 3` と `mtep 1` で ep 2（temp）と ep 3（plug）。
  - smctl（fabric 1）: PartsList `[1, 2, 3]`、ep 2 の DeviceTypeList `[{0: 770, 1: 2}]`、温度 2150、ep 3 の ServerList `[3, 4, 6, 29]`、toggle で `matter.ep_onoff ep=3 on=true` と `mtep.state on 1`。
  - chip-tool（onnetwork、ノード 122、保存先は別のディレクトリで試験の後に消した）: PartsList、温度、ep 3 の toggle と読み、DeviceTypeList（On/Off Plug-in Unit）。対話の形で温度を購読し、2 秒ごとに 2050 → 2300 のレポートが届いた。unpair した。
  - `mtpanel`: `term` を止めて起動すると ep 4（light、`mtpanel/panel`）。smctl から ep 4 を on にすると `mtpanel.state on=true` で描き直した。描く速さは、文字を 1 画素ずつ `rect` で描くので遅い（起動からの CPU 879 ms）。タッチは利用者に確かめてもらう。

## 7. 残り
- PartsList の変化は購読で知らせない（コアの Descriptor の作り）。コントローラが新しいエンドポイントを知るのは読み直したとき。
- 枠の種類は 5 つ。Level Control（明るさ）、Color、Switch などはまだ。
- `mtpanel` は `display` を `term` と取り合う。切り替える仕組み（画面の持ち主を替える）はまだ。
- スマホのエコシステム（Google Home、Apple Home）でブリッジでない複数のエンドポイントがどう見えるかは、まだ見ていない。
