# 49. Matter（simple-matter）

利用者の指示（2026-11-04）:「並行してこのOS向けにsimple-matterを対応し、Matterコントローラやデバイスを実装する方針について検討してください。」、「BLEを先にすすめてください。Matterでいずれにせよ必要になります。」、「通ったら続けてください」

## 1. 方針
調査の全文は doc/research/matter-on-tab5-os.md。要点:

- **simple-matter**（利用者の Rust の Matter の実装、`ciniml/simple_matter`）を使う。コアは `no_std`、sans-IO、同期型（`handle_rx`、`poll`、`next_deadline`）。alloc は要らない。
- **`matter` サービスを 1 つ作る**。デバイスとコントローラの両方の役をこの中に入れ、アプリとシェルは `svc:matter` を通して使う。
  - mDNS の UDP 5353 は、1 つのプロセスしか結びつけられない。
  - fabric、CA、ノードの記録、SD への保存を 1 か所にまとめる。
  - コアは同期型なので、`wait` のループにそのまま載る。async は要らない（async は `ble` の中だけ、doc/48 §1）。
- **順序**: on-network（Wi-Fi の UDP と mDNS）を先に、BLE のコミッショニング（BTP）は `ble` の M3（セントラル）と M4（ペリフェラル）の上に後で。
- **依存の取り方**: git の依存で版を固定する（`rev = "a163275"`、GitHub の `ciniml/simple_matter`）。tab5-os だけでビルドできる。simple-matter を変えたら、rev を上げる。
- **段取り**:

  | 段 | 内容 | 状態 |
  |---|---|---|
  | MT0 | ワークスペースに入れる。乱数と KVS のアダプタ。実機で暗号の速さを測る | 済み（§2） |
  | MT1a | デバイスの役（On/Off）。試験アプリでループバックのコミッショニング、CASE、再起動後の CASE。t5emu の試験に入れる | まだ |
  | MT1b | `net` にマルチキャストの参加、受けた宛先、送り元の固定。mDNS のレスポンダ。PC の chip-tool か smctl から on-network でコミッショニング | まだ |
  | MT2 | コントローラの役と、シェルの `mt` コマンド。運用中の NanoC6 をコミッショニング | まだ |
  | MT3 | Tab5 のデバイスの窓を開ける（OCW）、複数の fabric | まだ |
  | MT4 | BLE のコミッショニング（`ble` の M3/M4 の上） | まだ |
  | MT5 | アプリがエンドポイントを持ち込む仕組みと、GUI | まだ |

## 2. MT0
- **`user/mtport`**（ライブラリ）: simple-matter がプラットフォームに求めるもののうち、2 つ。
  - `KernelRng`: `crypto::Rng` を、カーネルの `random`（doc/47。SAR ADC のエントロピー源とヘルステスト）で。
  - `FileKvs`: `kvs::Kvs` を `svc:fs`（SD）の上に。
    - キーごとに `T5OS/MATTER/<キーの CRC-32 の 16 進 8 文字>.KVA` と `.KVB` の 2 ファイル（FAT は 8.3 の名前で、rename がない）。
    - レコードは「`T5KV`、世代、キーの長さ、値の長さ、CRC-32、キー、値」。キーも入れる（名前はハッシュなので）。
    - 書くときは、新しい正しいレコードを持たない方のファイルに、次の世代で書く。書いている途中で電源が切れても、前の値が読める。
    - 読むときは、両方を読み、CRC とキーが合うもののうち世代の新しい方。
    - ファイルの操作の失敗は `Error::InvalidState`（コアに I/O のエラーはない）、長すぎるキーや値は `Error::NoSpace`。キーは 32 バイト、値は 4 KiB まで。
- **`mtbench`**（`--permit random --permit svc:fs --stack 32768`）: `run mtbench`。simple-matter の RustCrypto の暗号を測り、KVS を確かめる。
- **実機**（2026-11-04、CPU 360 MHz、ソフトの暗号）:

  | 処理 | 時間 |
  |---|---|
  | SHA-256 | 3.9 MB/s（16 KiB で 4.1 ms） |
  | HMAC-SHA256（64 B） | 99 µs |
  | HKDF（48 B） | 219 µs |
  | AES-CCM 1 KiB | 暗号化 1.9 ms、復号 1.9 ms |
  | P-256 の鍵の生成 | 0.5 ms |
  | ECDSA の署名 / 検証 | 49 ms / 46 ms |
  | ECDH | 22 ms |
  | PASE: デバイスの検証子（PBKDF2 1000 回） | 219 ms |
  | PASE: prover の準備（PBKDF2 1000 回） / verifier の応答 / prover の確認 | 242 / 118 / 70 ms |
  | KVS（1500 B）: 書く / 読む / 書き換え / 消す | 23 / 6 / 18 / 4 ms |

  - KVS は、書き換えで A と B が入れ替わり、新しい方が読めること、消したら `None` になることも確かめた。
  - PASE は、どちらの側も 1 秒に収まる。CASE は署名、検証、ECDH が数回で、0.3 秒ほどの見込み。コミッショニングには十分な速さなので、P4 の暗号の加速器は当面使わない。
- 大きさ: `mtbench` のコードは 89 KB（暗号と SPAKE2+、KVS。スタックのコアは含まない）。

## 3. MT1: デバイスの役（on-network）
利用者の指示（2026-11-04）:「通ったら続けてください」

- **`matter` サービス**（`user/apps/matter`、`--permit svc:net --permit svc:fs --permit random --stack 131072`）:
  - simple-matter の `simple-matter-cffi`（同じ Tab5 の ESP-IDF 版 `tab5_ctrl_app` が使っている、C の ABI のつなぎ）を、Rust から直接呼ぶ。`sm_init`、`sm_udp_rx`、`sm_poll`、`sm_next_deadline`、`sm_mdns_rx`、`sm_mdns_poll`、`sm_set_addrs`、`sm_take_event`。どれも sans-IO（受けたものを渡すと、送るものと宛先が返る）。
  - 設定: On/Off ライト（エンドポイント 1）、開発用のパスコード 20202021、discriminator 3840、試験用の DAC（VID 0xFFF1、PID 0x8001）、製品名「Tab5」、シリアルは MAC から（`T5-` と 12 桁）。
  - KVS は `mtport::FileKvs`、乱数は `KernelRng`。コールバックの C の関数を Rust で書いた。
  - `svc:net` で UDP 5540（Matter）と 5353（mDNS）を Wi-Fi のリンクに結び、mDNS のソケットで ff02::fb と 224.0.0.251 のグループに入る。
  - ループ: 両方のソケットから読めるだけ読んで渡し、`sm_poll` と `sm_mdns_poll` が 0 を返すまで送り、イベントを出す。何も来なければ、次の期限か 20 ms まで眠る（UDP の受信の通知がないため）。5 秒ごとに Wi-Fi のアドレスを読み直して `sm_set_addrs`。
  - 大きさ: コード 318 KB、BSS 52 KB（スタックのコア、暗号、BLE と コントローラの部分も入っている）。
- **`net`**: `UDP_JOIN`（`net::op`、`Net::udp_join`）を足した。UDP のソケットが結ばれたリンクのインタフェースが、マルチキャストのグループに入る（MLD か IGMP の報告を出す）。インタフェースあたりのグループの数を 8 にした（smoltcp の既定は 4）。`net.joined` を出す。
- **フィルタ**: `netctl` の GUARD_SET に `pass in on wlan0 proto udp port 5540` と `... port 5353` を足した。
- **カーネル: 共有メモリの窓を 2 つに**（doc/19 §2）:
  - `matter` は `net` と `fs` の両方のクライアントで、それぞれの切れ端を写像する。1 プロセス 1 つまでだったので、2 つ目の `MAP` が `Busy` になった。
  - 2 つ目は PMP のエントリ 11〜12（空いていた）。`pmp::Region::shm` を 2 つの配列に、`Process::shm_mapped` も 2 つに。3 つ目は `Busy`。
  - 自己試験（`echo` の役 4）は「2 つ目は `Busy`」を確かめていたので、「2 つ目は写像でき、3 つ目は `Busy`」に直した。
  - 最初、自己試験を直す前のカーネルを実機に入れて、自己試験に失敗して元のカーネルに戻った（M5 の仕組みどおり）。ストアのアプリ（`echo`）を先に入れ直してから、カーネルを入れた。
- **パニックの処理の重なり**（§4 でコアを直接使う形にして解消）:
  - `simple-matter-cffi` は、feature `panic-abort` のときだけ `no_std` になり、そのとき自分の `#[panic_handler]`（`loop {}`）を持つ。`t5_rt` の処理と重なる。
  - 当面の手当: `t5_rt` に feature `no-panic-handler` を足し、`matter` だけそれを使う。cargo の feature はワークスペースの 1 回のビルドの中でまとまるので、`matter` は `default-members` から外し、別のターゲットのディレクトリ（`user/target/matter`）で別にビルドする（`Makefile`）。
  - このため、`matter` がパニックすると、ログを出さずに止まる（`loop {}`）。
- **実機**（2026-11-04、Wi-Fi は iotap、PC は有線で同じ LAN、smctl は別の状態のディレクトリ）:
  - `smctl discover commissionable`: 「0000E4B3239A1F24、discriminator 3840、VID/PID 0xfff1/0x8001、addrs fe80::e6b3:23ff:fe9a:1f24 と 192.168.2.112」が見えた。avahi-browse でも見えた。C6 は ff02::fb と 224.0.0.251 あてのフレームを上げてくる。
  - `smctl pairing onnetwork-long 1 20202021 3840`: PASE、ArmFailSafe、Attestation（DAC の連鎖、署名、CD、VID/PID の照合）、CSR、AddTrustedRootCertificate、AddNOC、CASE、CommissioningComplete が通った。見つけてから終わるまで 3.5 秒。
  - `onoff toggle`、`read on-off`、`off`、`on` が通り、Tab5 に `matter.event kind=1`（On/Off の変化）が出た。CASE は最初が全部の手順で 0.28 秒、2 回目からは Sigma2Resume で 0.02 秒。
  - `matter` を止めて動かし直すと、`fabrics=1` で起動し、smctl はそのまま操作できた（fabric、ACL、resumption を SD から戻せた）。

## 4. コアを直接使う形へ（cffi をやめた）
利用者とのやりとり（2026-11-04）:「simple-matter-cffiが必要なのですか？」→ 必須ではなく、コアを直接使う方針を提案し、「はい。」

- **理由**: `simple-matter-cffi` は、コアの部品を C から呼べる形にまとめて組み立てているだけ。この OS の `matter` は Rust なので、C の ABI は要らない。§3 のパニックの処理の重なり、別のビルド、`unsafe` のコールバックがなくなり、要らない部分（合成、カスタムクラスタ、BLE、Thread）も入らない。
- **`matter/src/device.rs`**: cffi の `Light`、`Owned`、`Shim::housekeep`、`rebuild_mdns`、`restore_and_advertise` を、合成と BLE と Thread を除いて Rust でそのまま書いた。
  - EP0: Access Control、Basic Information、General Commissioning、Network Commissioning（`wlan0`）、Administrator Commissioning、Operational Credentials（試験用の DAC）、Group Key Management、Descriptor。EP1: Identify、Groups、On/Off、Descriptor。
  - スタックは `MultiAdminStack`（複数のコントローラを同時に）。PASE は `dev_pase::dev_pase_config()`（20202021 の検証子を前もって計算したもの）。
  - fabric、ACL、グループ、CASE の resumption を、世代が変わったら KVS に保存し、起動時に戻す。fabric の数に合わせて、コミッショニングの広告（commissionable）と運用の広告（operational）を切り替える。最後の fabric が消えたら、また commissionable にする。コミッショニングの窓のイベントも cffi と同じに扱う。
  - mDNS の応答の宛先: QU の問い合わせは送り元へ、それ以外は来たファミリのグループへ。定期の告知は、IPv4 のアドレスがあれば 224.0.0.251、なければ ff02::fb（cffi と同じ）。
- **`Device` は静的な領域に置く**: `Device` は 26 KB。最初の版はスタックの上で作って動かしていて、実機で CASE の途中に壊れた値でパニックした（`transport/session/fixed.rs:88`、5 要素の表の長さが 768）。スタックがあふれたと見て、cffi と同じく静的な領域に置いた（`Device::init` が `&'static mut` を返す）。パニックが `t5_rt` の処理でログに出るようになったので、すぐ場所が分かった。
- **元に戻したもの**: `t5_rt` の `no-panic-handler`、`matter` の別のビルド（`default-members`、別のターゲットのディレクトリ）。`matter` はふつうのアプリとしてビルドする。
- 大きさ: コード 252 KB、BSS 37 KB（cffi のときは 318 KB と 52 KB）。
- **実機**（2026-11-04）:
  - cffi の版が SD に保存した fabric を、そのまま戻せた（`fabrics=1`）。smctl から読み、toggle、読みが通った。
  - `smctl pairing unpair 1`（RemoveFabric）で `matter.fabric_removed fabrics=0`。すぐ commissionable の広告に戻り、`pairing onnetwork-long` で 3.8 秒でコミッショニングし直せた（`matter.commissioned fabrics=1`）。On/Off も通った。

## 5. 残り
- MT1 の t5emu の試験（Wi-Fi がないので、ホストリンクか、ループバックで）。
- UDP の受信の通知（今は 20 ms ごとに見に行く）。
- MT2: コントローラの役（`ControllerStack`、`Commissioner`）と、シェルの `mt` コマンド。
