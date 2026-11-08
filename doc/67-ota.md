# 67. アプリとカーネルの OTA 更新

利用者の指示（2026-11-08）:「アプリやカーネルのOTAアップデートは可能ですか？」→「実装してください。」

## 1. これまで
- 更新の仕組みはそろっていた: カーネルの A/B（T5K、署名、`secure_version`、自己試験で確定、だめならロールバック、doc/16）、アプリの A/B（T5A、署名、最初の実行で確定、落ちたら前の版、doc/14、doc/16、doc/65）、電源が切れても古い目録が残る。
- 入り口はホストの制御チャネル（RTT か USB シリアル、doc/14 §3、doc/54）だけで、機器の上のアプリから更新できなかった。

## 2. 段取り
| 段 | 中身 |
|---|---|
| 1 | カーネルの `update` オブジェクト、更新のサービス `ota`（SD の像から）、`shell` の `ota`。済み |
| 2 | 網から取る（機器から HTTP で取りに行く） |
| 3 | 網の確かめ: 網で入れたカーネルは、`ota` が網に戻れたことを確かめてから確定する |

## 3. 段 1（2026-11-08）
- **`update`**（ABI 小版 25、`t5_abi::update`、強い許可）: 1 度に 1 プロセス。要求は `write` 1 回: `BEGIN_APP`/`BEGIN_KERNEL`（長さ）、`DATA`（位置と 1 KiB まで、256 バイトの倍数ずつ）、`END`、`REBOOT`、`CANCEL`。カーネルはサービスの巡回（コア 0、ロックを持って、フラッシュを書く所）で、ホストの `install`/`kupdate` と同じ関数（`store.install_*`、`kup.begin`/`data`/`end`）を呼ぶ。結果は `wait` の `DONE`（`status::OK`/`FAILED`、理由は `update.failed` と元の記録）。要求の途中で手を閉じると、始めたものを捨てる。1 回の巡回で 1 KiB なので、更新の間もアプリは止まらない（ホストの経路と同じく 1 巡 1 KiB 程度）。
- 検証は今までどおり（署名、ABI、`secure_version`）。ホストの制御コマンドと同時に使うと、後から始めた方が前のものを捨てる。
- **`ota`**（システムの鍵、`--permit listen:ota --permit update --permit svc:fs/r:`）: `svc:ota` で 1 通の要求に 1 通の返事。`app PATH`、`kernel PATH`（他方のスロットに書き、`reboot` で切り替わる。新しいカーネルは自己試験で確定）、`reboot`。`update` の手は、入れている間だけ開く。
- **`shell` の `ota`**: `ota app FILE`、`ota kernel FILE`、`ota reboot`。`svc:ota` につなげなければ `ota` を起動してからつなぐ。Tab 補完（サブコマンドとファイル）。
- 強い許可にしたので、アプリの鍵の像は `update` を持てない。

## 4. 試験（p4emu-run-36）
- SD の像（`build/sd-ota.img`）の `UPDATE` に、`greeter.t5a`、未知の鍵の `untrusted.t5a`、カーネルの `kernel.t5k`。`shell` の `ota app UPDATE/greeter.t5a` で `ota` が起動し入る（`store.installed`、`run greeter` が動く）、`untrusted.t5a` は `store.rejected error=UnknownKey`、`ota kernel UPDATE/kernel.t5k` は他方のスロットに書いて検証（`kupdate.ready`）、`ota reboot` で再起動（`update.reboot`、エミュレータはブートローダがないので終わる）。
