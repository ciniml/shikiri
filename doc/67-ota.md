# 67. アプリとカーネルの OTA 更新

利用者の指示（2026-11-08）:「アプリやカーネルのOTAアップデートは可能ですか？」→「実装してください。」

## 1. これまで
- 更新の仕組みはそろっていた: カーネルの A/B（T5K、署名、`secure_version`、自己試験で確定、だめならロールバック、doc/16）、アプリの A/B（T5A、署名、最初の実行で確定、落ちたら前の版、doc/14、doc/16、doc/65）、電源が切れても古い目録が残る。
- 入り口はホストの制御チャネル（RTT か USB シリアル、doc/14 §3、doc/54）だけで、機器の上のアプリから更新できなかった。

## 2. 段取り
| 段 | 中身 |
|---|---|
| 1 | カーネルの `update` オブジェクト、更新のサービス `ota`（SD の像から）、`shell` の `ota`。済み（§3） |
| 2 | 網から取る（機器から HTTP で取りに行く）。済み（§5） |
| 3 | 網の確かめ: 網で入れたカーネルは、`ota` が網に戻れたことを確かめてから確定する。済み（§6） |

## 3. 段 1（2026-11-08）
- **`update`**（ABI 小版 25、`t5_abi::update`、強い許可）: 1 度に 1 プロセス。要求は `write` 1 回: `BEGIN_APP`/`BEGIN_KERNEL`（長さ）、`DATA`（位置と 1 KiB まで、256 バイトの倍数ずつ）、`END`、`REBOOT`、`CANCEL`。カーネルはサービスの巡回（コア 0、ロックを持って、フラッシュを書く所）で、ホストの `install`/`kupdate` と同じ関数（`store.install_*`、`kup.begin`/`data`/`end`）を呼ぶ。結果は `wait` の `DONE`（`status::OK`/`FAILED`、理由は `update.failed` と元の記録）。要求の途中で手を閉じると、始めたものを捨てる。1 回の巡回で 1 KiB なので、更新の間もアプリは止まらない（ホストの経路と同じく 1 巡 1 KiB 程度）。
- 検証は今までどおり（署名、ABI、`secure_version`）。ホストの制御コマンドと同時に使うと、後から始めた方が前のものを捨てる。
- **`ota`**（システムの鍵、`--permit listen:ota --permit update --permit svc:fs/r:`）: `svc:ota` で 1 通の要求に 1 通の返事。`app PATH`、`kernel PATH`（他方のスロットに書き、`reboot` で切り替わる。新しいカーネルは自己試験で確定）、`reboot`。`update` の手は、入れている間だけ開く。
- **`shell` の `ota`**: `ota app FILE`、`ota kernel FILE`、`ota reboot`。`svc:ota` につなげなければ `ota` を起動してからつなぐ。Tab 補完（サブコマンドとファイル）。
- 強い許可にしたので、アプリの鍵の像は `update` を持てない。

## 4. 試験（p4emu-run-36）
- SD の像（`build/sd-ota.img`）の `UPDATE` に、`greeter.t5a`、未知の鍵の `untrusted.t5a`、カーネルの `kernel.t5k`。`shell` の `ota app UPDATE/greeter.t5a` で `ota` が起動し入る（`store.installed`、`run greeter` が動く）、`untrusted.t5a` は `store.rejected error=UnknownKey`、`ota kernel UPDATE/kernel.t5k` は他方のスロットに書いて検証（`kupdate.ready`）、`ota reboot` で再起動（`update.reboot`、エミュレータはブートローダがないので終わる）。

## 5. 段 2: 網から取る（2026-11-08）
- `ota app http://HOST[:PORT]/PATH`、`ota kernel http://...`。機器から取りに行く（受け身の口を開けないので、`net` のフィルタ（doc/24）の守りのまま使え、外から更新を始められることもない）。
- HTTP/1.0 の `GET`。答えは 200 で `Content-Length` があること（カーネルに長さを先に渡すため）。名前は DNS で引く（`svc:net/resolve`）。20 秒データが来なければやめる。
- 平文の HTTP でよい理由: 像は署名を確かめ、`secure_version` で古いものを断る。途中で書き換えられても入らない（秘密は運ばない）。
- `ota` の許可に `svc:net/tcp-connect`、`svc:net/resolve`。スタックを 24 KiB に（HTTP の道で 2 KiB の頭の置き場などが重なり、既定の大きさで溢れて命令アクセスフォールトで落ちた。試験で見つけた）。
- **エミュレータ**: ホストの相手（`emu/src/hostpeer.rs`、10.55.0.1、ホストリンク）に、`--http-dir DIR` で 8080 番の HTTP のファイル置き場を足した（1 本ずつ、`GET` に 200 か 404）。

## 6. 段 3: 網に戻れてから確定（2026-11-08）
- 網で取ったカーネルが自己試験は通るが網を壊すと、遠くからは戻せない。そこで、網で入れたカーネルは網が使えることを確かめてから確定する。
- `ota` は HTTP で取ったカーネルの `BEGIN_KERNEL` にサーバの IPv4 とポートを付ける。カーネルは otadata の項目の label（ブートローダは使わない 20 バイト、CRC の外）に `T5NC`、IPv4、ポートを書く。
- 新しいカーネルは、自己試験が通っても確定せず、`update::NET_CONFIRM_MS`（300 秒）待つ（`kupdate.awaiting_net`）。その間も確定前の扱い（パニックは再起動、見張りあり）。
- `ota` は起動のとき（機器では自分で起動、`AUTO_APPS`）`update` を `read` し、待っているならそのサーバに TCP でつながるまで 3 秒ごとに試し、つながったら `CONFIRM`（`kupdate.net_reached`、`kupdate.confirmed`）。
- 時間内に来なければ、カーネルは自分を無効にして再起動し、ブートローダが前のカーネルに戻す（`kupdate.no_network`、`kupdate.rejected`）。
- SD から入れたカーネル（`ota kernel FILE`）とホストの `kupdate` は、今までどおり自己試験で確定する。

## 7. 試験
- p4emu-run-36: SD から（§4）。
- p4emu-run-37: 網から。ホストの相手が `build/` を HTTP で出し、`ota app http://10.55.0.1:8080/apps/greeter.t5a` が入って動く、ない物は 404（`ota.http_status`）、カーネルの包みは他方のスロットに（`kupdate.ready`）。`ota` が落ちない（pid 100 以上のフォールトがない）、`ota.ready` が 1 回だけ。
- p4emu-run-38: otadata が `pending_verify`、label に 10.55.0.1:8080（`elf2t5a otadata OUT 1 1 10.55.0.1:8080`）で起動。自己試験の後に待ち、`ota` がつながって確定する。
- p4emu-run-39: 同じでサーバがない。300 秒後に無効にして再起動（`--expect-reset`）。

## 8. 実機（Tab5、2026-11-08）
- カーネルとアプリ（`ota` は自分で起動する版）を入れた。`make install-apps` は、doc/65 の直しで全部入った。
- PC で `python3 -m http.server 8089 --directory build` を立てた（8000 番は PC の別のサービスが使っていた）。
- **PC の Wi-Fi のアドレス（192.168.8.107）には届かなかった**（`error cannot connect`）。Tab5 からルーター（192.168.8.1）とインターネット（1.1.1.1）には ping が届くが、PC には届かず、PC の ICMP の受信の数も 0 だった。同じ AP（BSSID 2e:ea:03:08:e7:62）につながっていて、利用者によるとクライアントアイソレーションは無効。PC の USB の Wi-Fi のアダプタが、他の機器のマルチキャスト（mDNS）を 15 秒で 1 つも受けていなかったので、ブロードキャスト（Tab5 の ARP の問い合わせ）を受けられていないのではないか（推測、未確認）。
- PC の有線のアドレス（192.168.2.14、ルーターの外側）にはルーター経由で届いたので、そこでサーバを立てた。
- `ota app http://192.168.2.14:8089/apps/greeter.t5a` が入って動き、`ota kernel http://192.168.2.14:8089/kupdate/good.t5k` と `ota reboot` の後、新しいカーネル（スロット 1、otadata の seq 112）は確定前で起動し（`kupdate.pending`）、起動から 29 秒以内に `valid` になった（`ota` がサーバに届いて `CONFIRM`）。サーバの記録に、Tab5 の 2 つの取得（ルーターの NAT の後の 192.168.2.129 から、HTTP/1.0）がある。
- 再起動のたびに `term` は起動の一覧（SD の `START.CFG`）にないので、ホストの `run term` が要る。

## 9. まだ
- 定期的な更新の確認（版を比べる目録、`ota check`）。今は人が `ota` で頼む。
- HTTPS（TLS）。署名があるので必須ではない。
