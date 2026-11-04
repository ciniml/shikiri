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
- **依存の取り方**: git の依存で版を固定する（`rev = "1544fae"`、GitHub の `ciniml/simple_matter`。MT0〜MT4 のデバイスの側までは a163275）。tab5-os だけでビルドできる。simple-matter を変えたら、rev を上げる。
- **段取り**:

  | 段 | 内容 | 状態 |
  |---|---|---|
  | MT0 | ワークスペースに入れる。乱数と KVS のアダプタ。実機で暗号の速さを測る | 済み（§2） |
  | MT1a | デバイスの役（On/Off）。試験アプリでループバックのコミッショニング、CASE、再起動後の CASE。t5emu の試験に入れる | まだ |
  | MT1b | `net` にマルチキャストの参加、受けた宛先、送り元の固定。mDNS のレスポンダ。PC の chip-tool か smctl から on-network でコミッショニング | まだ |
  | MT2 | コントローラの役と、シェルの `mt` コマンド | 済み（§6。運用中の機器ではなく PC の試験用のデバイスで） |
  | MT3 | Tab5 のデバイスの窓を開ける（OCW）、複数の fabric | 済み（§8） |
  | MT4 | BLE のコミッショニング（`ble` の M3/M4 の上） | 済み（デバイスの側は §10、コントローラの側は §12） |
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

## 5. 残り（§4 の時点）
- MT2 は §6 で済んだ。

## 6. MT2: コントローラの役
利用者の指示（2026-11-04）:「はい。」（コアを直接使う方針）の後、そのまま MT2 へ。

- **`matter/src/controller.rs`**: コアの `ControllerStack` と `Commissioner` を、cffi のコントローラ（`CtrlShim`、`pump`、`start_operation`）にならって使う。cffi の 3,500 行のうち、次だけにした。
  - 見つける: `_matterc._udp` を QU（ユニキャストの応答を頼む）で 1 秒ごとに問い合わせ、答えの TXT の discriminator で絞る。10 秒で見つからなければ失敗。IPv4 のアドレスを優先する（リンクローカルの IPv6 はスコープが要るため）。
  - コミッショニング: `Commissioner`（証明書の確かめは smctl の既定と同じ `VerifyNoPaa`）。
  - 操作: On/Off の invoke（on、off、toggle）と、属性の読み（スカラ）。生きたセッションがあればそのまま、なければ CASE（resumption の材料があれば Sigma2Resume）を張ってから。タイムアウトしたら、そのセッションを捨てて次は CASE から。
  - 保存（デバイスと同じ SD の KVS。キーは cffi と同じ）: CA（`cast`）、ノードの一覧（`nods`）、ノードごとの resumption（`rsm` と 16 進のノード ID）。CA は初回に作る（fabric ID は乱数、コントローラのノード ID 0x7AB50001、VID 0xFFF1）。
  - 1 度に 1 つの要求。コントローラは自分の UDP ポート 5541 を使う（返事はそこへ来て、フィルタの established で通る）。
- **`svc:matter`**（`t5_abi::matter`、`--permit listen:matter`）: 1 つの接続で 1 つの要求（テキスト）。`status`、`nodes`、`pair NODE PASSCODE DISCRIMINATOR`、`on`/`off`/`toggle NODE`、`read NODE EP CLUSTER ATTR`。答えは `LINE` のテキスト、最後に `END`（成否）。答えたら接続を閉じる（閉じたチャネルは `wait` のイベントでしか分からず、`read` は `Busy` のままなので、待たない）。
- **シェルの `mt`**: `mt pair 2 20202021 3841` のように、残りの単語を `svc:matter` に送り、答えを表示する（`--permit svc:matter`）。
- **試験アプリ `mtreq`**（`--permit svc:matter`）: `run mtreq OP NODE`。キーボードで打てない試験のため、数だけで要求を組む（0 status、1 nodes、2 pair（20202021、discriminator 3841）、3 on、4 off、5 toggle、6 OnOff の読み）。
- **デバイスとコントローラで 1 つの KVS**: `fs` への接続を 2 つにすると、共有メモリの窓が 3 つ要る（2 つまで）。`RefCell<FileKvs>` を静的な領域に置いて両方が借りる。
- **つまずいたこと**:
  1. 最初の要求のあと、次のクライアントが受け付けられなかった。去ったクライアントのチャネルを持ち続けていた（上の「答えたら閉じる」）。
  2. 見つからなかった。`_L<d>` のサブタイプの問い合わせに、simple-matter の例のデバイスは答えなかった（smctl と同じく全体を問い合わせて絞る形にした）。さらに、PC のデバイスがマルチキャストで返す 792 バイトの答えが、Tab5 にほとんど届かなかった（PC で見ると毎回答えている。有線から Wi-Fi への大きなマルチキャストが落ちていると見られる。未確認）。QU にして、ユニキャストで返してもらうと、すぐ見つかった。小さなマルチキャスト（smctl の問い合わせ、デバイスの告知）は届いていた。
  3. AddNOC に返事がなく、30 秒で失敗した。前の返事（AddTrustedRoot）への ACK を出す前に、23 ms で AddNOC を送っていた（`handle_rx` の中ですぐ次へ進めていた）。cffi は、次へ進めるのを `poll` の中で、トランスポートに保留がない（`transport_deadline()` が空）ときだけにしている。同じにすると通った。PC のデバイスは、ACK の来ていない返事を抱えたまま次の要求を受け、fabric は足したが返事を出さなかった。
- **実機**（2026-11-04）: PC で simple-matter の `onoff-light` の例（`SM_DISCRIMINATOR=3841`、状態を保存しない）を試験用のデバイスとして動かし、Tab5 から:
  - `pair 2 20202021 3841`: 見つけてから PASE、証明書の確かめ、CSR、AddTrustedRoot、AddNOC、CASE、CommissioningComplete まで 3.8 秒。
  - `read 2 1 6 0` → 0、`on 2` → status 0、読み → 1、`toggle 2` → status 0、読み → 0。PC のデバイスは「light is now ON」「OFF」を出した。
  - `nodes` → 「node 2 at 192.168.2.14:5540 (session)」。
  - `matter` を動かし直すと「nodes=1 new_ca=false」で起動し、`on 2` は CASE の resumption（`resumed=true`）で通った。
  - 運用中の機器（NanoC6 など）はコミッショニングしていない（よその fabric に入れることになるため）。

## 7. 残り（§6 の時点）
- MT3 は §8 で済んだ。

## 8. MT3: コミッショニングの窓と複数の fabric
利用者の指示（2026-11-04）:「順に進めて」。

- **コントローラから窓を開ける**: `controller.rs` の `Op::OpenWindow` と `Op::Revoke`（コアの `start_open_commissioning_window` と `start_revoke_commissioning`。どちらも timed invoke）。`svc:matter` の要求は `ocw NODE [SECONDS]`（既定 300 秒、180〜900 に丸める）と `revoke NODE`。
  - passcode、discriminator、salt は `KernelRng` で毎回作る（`onboarding::random_passcode` と `random_discriminator`）。答えの最初の行に「passcode … discriminator … code（11 桁の手入力のコード） for … s」を出し、窓が開いたら `status 0x00`。デバイスに渡るのは検証子だけ（PBKDF2 1000 回、Tab5 で約 220 ms）。
  - シェルは `mt ocw 2`、`mt revoke 2`。`mtreq` は OP 7（ocw）と 8（revoke）。
- **デバイスの側**は MT1 のときから窓の処理（`AdminCommissioningCluster` と `CommissioningWindow`、開いたら PASE の設定と CM=2 の広告、閉じたら止める）を持っていた。fabric は 5 つまで。ここでは実機で確かめた。
- **デバイスの再送バッファ**: コアの `MultiAdminStack` は exchange 6、再送バッファ 3。`MatterStack` を直接使い、どちらも 16 にした（`device.rs` の `Stack`）。
  - 返事を送る再送バッファがないと、コアは返事を出さずに捨てる。
  - ACK の来ない返事は、10 回送り終えるまで（実機で約 95 秒）バッファを持ち続ける。そのセッションが追い出されても放さない（`close_session` も exchange を閉じない）。
  - smctl は、コマンドが失敗すると返事に ACK せずに終わる（open-window は前に 2 つの読みをするので、最大 3 つ残る）。再送バッファが 3 つでは、窓のないときの revoke（WindowNotOpen）を 1 度しただけで、次の open-window が TimedRequest に返事をもらえず 30 秒でタイムアウトした（約 95 秒続く）。
  - 16 でも、8 秒に失敗を 8 回続けると尽きた（32 回のうち 2 回が CASE のタイムアウト）。ACK を返すコントローラなら残らない。直すべきはコア（バッファがないときに古い再送を捨てる、セッションを閉じたらその exchange も閉じる）。
- **実機**（2026-11-04）:
  - Tab5 がコントローラ: PC の `onoff-light` の例をノード 2 としてコミッショニングし、`ocw 2` → status 0（PC の例は「enhanced commissioning window open (CM=2, discriminator 1668)」）。smctl（別の状態のディレクトリ、別の fabric）が表示の passcode と discriminator でコミッショニングして 3.0 秒、toggle も通り、PC の例の `CommissionedFabrics` は 2。`revoke 2` → status 0 で窓が閉じた。
  - Tab5 がデバイス: smctl の fabric（ノード 1）から `admincommissioning open-window 1 300 1234`、別の smctl の fabric がノード 5 としてコミッショニング（3.8 秒）。両方の fabric から On/Off を操作し、`CommissionedFabrics` は 2。`matter` を動かし直しても fabrics=2 のまま。窓を開けると Tab5 は CM=2、discriminator 777 で広告した。開いている窓をもう 1 度開けると Failure（Busy）、閉じた窓の revoke も Failure（WindowNotOpen）。最後に 2 つ目の fabric を unpair し、1 つ目はそのまま使えた。
- **コアの別の問題**（試験用のデバイスで見えた。記録だけ）: simple-matter のデバイスは、非暗号のセッションの送信カウンタを毎回 1 から始める（仕様は乱数）。コントローラは、同じ相手のアドレスへの古い非暗号のセッションを使い回し、その受信の窓も引き継ぐ。PC の例を起動し直してから、同じ `matter` で 2 度目のコミッショニングをすると、PBKDFParamResponse を重複として捨てて PASE が進まなかった（`matter` を動かし直すと通る）。CASE でも同じことが起きうる（MT3 の試験の途中で、CASE が 1 度 40 秒かけて失敗した）。

## 9. 残り（§8 の時点）
- MT4: BLE のコミッショニング（`ble` の M3/M4 の上）。
- 購読（subscribe）、書き込み、任意のコマンド（引数つき）。
- MT1/MT2 の t5emu の試験（Wi-Fi がないので、ホストリンクで）。
- UDP の受信の通知（今は 20 ms ごとに見に行く）。
- simple-matter のコアの直し（§8: 再送バッファが尽きたときと、セッションを閉じたときの exchange の扱い、非暗号のカウンタと非暗号のセッションの使い回し）。

## 10. MT4: BLE のコミッショニング（デバイスの側）
利用者の指示（2026-11-04）:「すすめて」。

- **作り**（`matter/src/blelink.rs`）:
  - コアの `ble` の feature（`Btp` と `PeerAddr::Ble`）を使う。
  - デバイスが commissionable の間（最初の窓、開けられた窓）は、`svc:ble` に `ADVERTISE` を送る（doc/48 M4）。
    - サービスは 0xFFF6、特性は C1（18EE2EF5-…-9D11、書き込み）と C2（…-9D12、読みと indication）。
    - 広告のデータはコアの `AdvData::encode_adv`（flags と、discriminator、VID、PID の入った service data。15 バイト）。間隔 100 ms。
  - `device.rs` は commissionable の状態（discriminator と CM）を 1 つ持ち、mDNS と BLE の両方をそこから作る。前は、窓が開いている間にアドレスが変わると、mDNS を作り直したときに commissionable の広告が消えていた。
  - C1 への書き込み（`PEER_WRITE`）を `Btp::process_incoming` へ渡し、組み上がったメッセージを `handle_rx(…, PeerAddr::Ble)` へ渡す。
    - 返事と `poll` の BLE 宛ての送信は、待ち行列（3 つ）から 1 つずつ `Btp::send` へ入れる。
    - BTP のフラグメントは C2 の indication で 1 つずつ送る（`SENT` を待ってから次）。
  - C2 の購読（CCCD の bit 1）より前は何も送らない。BTP の無通信や ACK のタイムアウトでは `STOP`（切断）する。
  - `ble` はクライアントを 1 つずつしか扱わない。そのため、チャネルは commissionable の間と、コミッショナがつないでいる間だけ持つ。窓が閉じて相手が切れたら閉じ、ほかのアプリが `ble` を使えるようにする。
  - `ble` が動いていなければ 5 秒ごとに試す。`ADVERTISE` に 5 秒答えがなければ（BT のコントローラが起動していないと、`ble` は HCI の答えを待ち続ける）、やめてやり直す。
  - 相手が閉じたことは `wait` のイベントでしか分からないので、2 秒ごとに見る。ループの眠りも、`ble` のチャネルがあるときはそれを `wait` する（何か来たらすぐ起きる）。
- **`svc:ble` の `MTU` イベント**（`[MTU, att_mtu]`、doc/48 M4 に追加）: `ble` は、接続の ATT MTU が変わっていれば、`PEER_WRITE` の前に知らせる。BTP の handshake で、こちらの MTU として渡す（コアは相手が出した MTU と小さい方を使う）。
- **PC の側**: smctl を `--features ble`（btleplug）でビルドした。simple_matter の a163275 を `git archive` で取り出し、別のターゲットのディレクトリへ。
  - `pairing ble-handoff NODE PASSCODE DISCRIMINATOR` を使う。PASE から AddNOC までは BLE、CASE と CommissioningComplete は UDP（運用の mDNS で引く）。
  - Tab5 は Wi-Fi に既につながっているので、ネットワークの設定（Wi-Fi の資格情報）は要らない（NetworkCommissioning はこれまでどおり）。
- **実機**（2026-11-04）:
  - BT のコントローラは、`wifi` を引数 1 で動かし直して起動した（`kill` の後 `run wifi 1`、doc/48 §3）。最初は、BT が起動していないまま `run ble` していた。`ble` は `ADVERTISE` の HCI の答えを待ったまま、CPU を使い続けた。
  - smctl（ノード 1 の fabric）から `admincommissioning open-window 1 300 1234` を送ると、Tab5 は discriminator 1234 で BLE の広告を始めた（`matter.ble_advertising`）。
  - 別の smctl の fabric が `pairing ble-handoff 6 …` で見つけ、接続、BTP（window 6）、PASE、Attestation、CSR、AddNOC を BLE で行った。続けて CASE と CommissioningComplete を UDP で行い、fabrics=2 になった。
  - その fabric から UDP で toggle が通った。終わったら unpair し、fabrics=1 に戻した。
  - **かかった時間は 54〜64 秒**（PC の 2 つのアダプタのどちらでも）。Attestation だけで 28〜34 秒。
    - smctl（btleplug）は ATT MTU を知らない（`att_mtu=None`）。そのため BTP の handshake で 23 を出し、フラグメントは 20 バイトになった。Tab5 の側の MTU は 247（`matter.ble_mtu`）。
    - 1 回のコミッショニングで C1 への書き込みが 111、indication が 171。
    - 接続の間隔は 50 ms（LE Enhanced Connection Complete の 0x0028）。それに対して、indication 1 つに平均 205 ms、最大 399 ms かかった（`ble` の中で `indicate_raw` の時間を測った。測った後で外した）。接続のイベント 4 回に 1 回ほどしか通っていない。C6 が無線を Wi-Fi と分け合っているためと思われる（doc/48 §9 の接続の失敗と同じ根）。
    - MTU を出すコミッショナ（chip-tool やスマホ）なら、フラグメントは 244 バイトになり、数は 1/12 ほどになる見込み（まだ試していない）。
  - ATT MTU は接続から約 2 秒後に 247 になった（最初の C1 への書き込みの前）。
- **simple-matter の直しの確認**（2026-11-04。§8 の問題を simple-matter の側が直し、実機での確認を頼まれた。tab5-os の依存は a163275 のまま、手元で試しただけ）:
  - 44f3dce と 96c0171（どちらも main、未 push）を試した。デバイスのスタックは `MultiAdminStack` と同じ 6/3（exchange 6、再送バッファ 3）に戻している。
  - 非暗号のカウンタ（44f3dce）: Tab5 のコントローラが PC の例をコミッショニングした（3.8 秒）。例を起動し直した後、`matter` を動かし直さずに、もう一度コミッショニングできた（3.7 秒）。
  - ACK の来ない返事:
    - 44f3dce では、古い smctl（a163275。ACK しない）で窓のないときの revoke の直後に open-window すると、まだ 3/3 Timeout だった。残った返事がまだ 1 度も再送されていないため。
    - 96c0171（slot が尽きたら、再送前の応答もあきらめる）では、同じ試験が 3/3 成功した。revoke 8 回（2.2 秒）→ open-window を 4 回くり返す試験も、古い smctl と新しい smctl の両方で 4/4 成功した。
  - smctl の表示: 「status Failure (cluster status 4 = WindowNotOpen)」。
  - smctl の BLE の MTU（8c3990d。BlueZ から読む）: att_mtu 242、フラグメント 239 で、ble-handoff は 19.6 秒と 19.9 秒だった（スキャンと接続の約 4.5 秒を含む）。C1 への書き込みは 17、indication は 20。Attestation から CSRRequest まではまだ約 8.6 秒かかる（原因は見ていない）。
  - その 8.6 秒は、コアの `Commissioner::drive` の問題だった（応答を受けた呼び出しで次の要求を出さず、smctl の BLE のループが BTP の ACK のタイマー、約 2.3 秒まで止まっていた）。simple-matter の 1544fae で直った。その smctl で ble-handoff は 12.2 秒と 11.4 秒、Attestation は約 2 秒（Tab5 はコミット済みの版のまま）。Tab5 のコントローラも同じ `Commissioner` を使う。
  - 利用者が simple-matter の側を push した（2026-11-04、「simple-matter側の変更はpushしました。」）。tab5-os の依存を 1544fae に上げ、デバイスはコアの `MultiAdminStack`（exchange 6、再送バッファ 3）に戻した（§8 の 16 の手当てをやめた）。
    - 実機: 前の fabric を読み戻して toggle が通った。古い smctl の試験（revoke 8 回 → open-window を 4 回）は 4/4 成功した。ble-handoff は 13.2 秒で、その fabric からの toggle と unpair も通った。Tab5 のコントローラは PC の例（1544fae）を 3.7 秒でコミッショニングし、toggle が通った。
- `ble` のログ: HCI のイベントの頭を 24 バイトから 32 バイトにした（接続の間隔まで見えるように）。

## 11. 残り（§10 の時点）
- MT4 のコントローラの側: Tab5 が BLE で PC の `ble-onoff-light`（simple-matter-ble、BlueZ）をコミッショニングする（`ble` の M3、BTP の central）。
- 購読（subscribe）、書き込み、任意のコマンド（引数つき）。
- MT1/MT2 の t5emu の試験（Wi-Fi がないので、ホストリンクで）。
- UDP の受信の通知（今は 20 ms ごとに見に行く）。
- simple-matter のコアの直し（§8）: 済み（1544fae に上げた。§10）。

## 12. MT4: BLE のコミッショニング（コントローラの側）
利用者の指示（2026-11-04）:「すすめて」。

- **要求**: `pairble NODE PASSCODE DISCRIMINATOR`（シェルの `mt pairble …`、`mtreq` の OP 9 は `pairble NODE 20202021 3840`）。PASE から AddNOC までを BLE で行い、CASE と CommissioningComplete はネットワーク（UDP）で行う。smctl の `ble-handoff` と同じ流れ。
- **`blelink` のセントラルの側**（`svc:ble` の M3 の要求。デバイスの側と同じチャネルを使い、同時にはどちらか 1 つ）:
  - 受け身のスキャン（10 秒、間隔 100 ms、窓 30 ms）で、広告の 0xFFF6 の service data の discriminator が合う相手を探す。見つけたら `STOP`、`DONE` を待って `CONNECT`。
  - `SERVICES` で 0xFFF6、`CHARS` で C1 と C2 を探す。
  - BTP の handshake は chip と同じ順で、C1 に要求を書き（応答あり）、それから C2 を indication で購読する。応答の indication で確立する。
  - その後は、BTP のフラグメントを C1 への書き込み（応答あり、1 つずつ）で送り、C2 の indication を受ける。
  - リンクができるまで 30 秒、BTP の無通信や ACK のタイムアウトで失敗にする。見つからなければ「not found over BLE」。
  - デバイスの側を使った後は BTP を peripheral の役で作り直す。最初の版は、セントラルを使った後に BTP が central の役のまま残り、次にデバイスとしてつながれたとき handshake を受け付けなかった（`InvalidState`）。
- **コントローラ**（`controller.rs`）:
  - `Commissioner::suspend_before_case` で、AddNOC の後の CASE を止めておく。宛先は `PeerAddr::Ble`。BLE 宛ての送信は別の待ち行列から `blelink` へ渡す。
  - `Phase::Case` で止まったら BLE を閉じる。運用の mDNS（`build_resolve_operational`、QU、1 秒ごと、10 秒まで）でノードのアドレスを引き、`set_peer` と `resume` をして、あとは on-network のコミッショニングと同じ。
  - BLE の部分は 90 秒まで。
- **`forget NODE`**（`mtreq` の OP 10）: コントローラのノードの表から外し、CASE の resumption も消す（デバイスには何も送らない）。ノードの表は 8 つまでで、試験用のデバイス（毎回 fabric を持たずに起動し直す）で埋まっていた。
- **PC の側**: simple-matter-ble の `ble-onoff-light`（BlueZ の peripheral、`SM_BLE_ADAPTER=hci1`、discriminator 3840、UDP と mDNS も動かす）を 1544fae でビルドした。
- **実機**（2026-11-04）:
  - Tab5 が PC の `ble-onoff-light` をノード 10 としてコミッショニングし、14.4 秒かかった。内訳は、スキャン 3.3 秒、接続 3.1 秒、BTP の確立 1.7 秒（ATT MTU 247、フラグメント 244）、PASE から AddNOC まで 5.4 秒、mDNS 0.1 秒、CASE と CommissioningComplete 0.8 秒。toggle（status 0）と OnOff の読みが通り、PC の側も「light is now ON」。
  - PC の側を起動し直して、もう 1 度（ノード 11）: 17.8 秒（スキャンが長かった）。
  - 相手がいないとき: 10 秒のスキャンの後「commissioning failed: not found over BLE」で、`ble` を放した。
  - 続けて（同じ `matter` で）Tab5 をデバイスとして smctl から ble-handoff: 13.3 秒、toggle も通った。上の BTP の役の問題はここで見つけて直した。
  - 試験の後、試験用のノード（2、7〜11、13）は `forget` で消した（nodes=0）。

## 13. 残り（§12 の時点）
- 購読（subscribe）、書き込み、任意のコマンド（引数つき）。
- MT1/MT2 の t5emu の試験（Wi-Fi がないので、ホストリンクで）。
- UDP の受信の通知（今は 20 ms ごとに見に行く）。
- BLE: Wi-Fi の資格情報を BLE で渡すコミッショニング（`AddOrUpdateWiFiNetwork` と `ConnectNetwork`）。Tab5 はどちらの側も、相手がもうネットワークにいる前提。

## 14. 引数つきのコマンド、書き込み、購読（コントローラ）
利用者の指示（2026-11-04）:「すすめて」。

- **要求**（`svc:matter`、シェルの `mt`）:
  - `invoke NODE EP CLUSTER CMD [ARG...]`: 引数は順に context タグ 0, 1, … のフィールドになる（6 つまで）。値は数（10 進か 0x の 16 進）、`-N`、`true`、`false`、`null`。整数は最小幅で書く。`on`、`off`、`toggle` は引数のない `invoke`。
  - `write NODE EP CLUSTER ATTR VALUE`: 値の書き方は同じ。答えは WriteResponse の状態（`status 0x..`）。
  - `sub NODE EP CLUSTER ATTR [MIN_S [MAX_S]]`（既定 0 と 60 秒）: 答えは「subscribed: id …」。届いたレポートは `matter.report`（ノード、購読 ID、パス、値）としてログに出す。クライアントが閉じた後も、購読が切れるか `matter` が終わるまで続く。切れたら（最大の間隔と 30 秒の猶予を過ぎたら）`matter.subscription_lost`。
  - 使い方の表示は、要求ごとに 1 行にした（1 行 127 バイトに収まらなくなった）。
- **作り**（`controller.rs`）: `Val`（値）と `Op::Write`、`Op::Subscribe`、`Op::Invoke` の引数。コアの `start_write`、`start_subscribe`、`start_invoke`（引数はクロージャで書く）。
  - コアの IM のイベントは 1 つしか持たない。そのため購読のレポートと喪失は、どの状態でも先に取り出してログに出し、要求の答えと混ぜない。値の読み方は `read` と同じ（真偽は 0 か 1、整数、null）。
- **`mtreq`**: OP 11 `write NODE 1 3 0 10`（Identify の IdentifyTime）、12 `invoke NODE 1 3 0 5`（Identify、5 秒）、13 `sub NODE 1 6 0 1 10`（OnOff、1〜10 秒）。
- **実機**（2026-11-04、PC の `onoff-light` の例、1544fae）:
  - Tab5 がノード 20 としてコミッショニングした（3.7 秒）。
  - `write 20 1 3 0 10` → status 0。PC の側で「[identify] identifying」、10 秒後に「stopped」。
  - `invoke 20 1 3 0 5`（Identify、引数 IdentifyTime 5）→ status 0。PC の側で identify が始まり、止まった。
  - `sub 20 1 6 0 1 10` → 「subscribed: id 1, reports at least every 10 s」。その後の `toggle 20` で `matter.report node=20 id=1 ep=1 cluster=6 attr=0 value=1`。
  - PC の例を止めると、約 50 秒後に `matter.subscription_lost id=1`。
  - 試験の後、ノード 20 は `forget` で消した。

## 15. 残り（§14 の時点）
- MT1/MT2 の t5emu の試験（Wi-Fi がないので、ホストリンクで）。
- UDP の受信の通知（今は 20 ms ごとに見に行く）。
- BLE: Wi-Fi の資格情報を BLE で渡すコミッショニング（`AddOrUpdateWiFiNetwork` と `ConnectNetwork`）。Tab5 はどちらの側も、相手がもうネットワークにいる前提。
- 購読: 構造体や文字列の値はログに出さない（数と真偽と null だけ）。イベントの購読はまだない。

## 16. t5emu の試験
利用者の指示（2026-11-04）:「すすめて」。

- **やり方を変えた**: 計画は「ホストリンクで」だったが、t5emu には SD カードがない（`fs` が動かず、KVS がない）。また、PC の側（t5emu の中の smoltcp の相手）に Matter のコントローラを作るのは大きい。そこで `matter` の中のコントローラが、同じプロセスの中のデバイスを 127.0.0.1 でコミッショニングする形にした。PASE、CASE、IM、購読、`svc:net` の UDP の上で、デバイスとコントローラの両方を通す。
- **`run matter 1`**（試験の形）:
  - ソケットはどのリンクにも開く（`link::ANY`。ループバックが通る）。wlan0 を待たず、MAC は作ったもの（02:00:00:00:00:01）。
  - ストアはメモリの中（`store.rs` の `MemKvs`。プロセスが終われば消える。SD の `FileKvs` と `Store` の enum で切り替える）。BLE は使わない。
  - mDNS のグループには入れない（警告だけ）。
- **`pairaddr NODE PASSCODE IP PORT`**: 探さずにそのアドレスをコミッショニングする（smctl の `pairing address` と同じ）。`mtreq` の OP 14 は `pairaddr NODE 20202021 127.0.0.1 5540`。
- **`make test-p4emu` の最後**: `run net`、`run matter 1`、`mtreq` で pairaddr、sub、toggle、read。期待するイベントは `matter.ready`、`matter.commissioned`、`matter.onoff`、`matter.report`（`--expect-event` は名前だけを見るので、成功のときだけ出るものを選んだ）。
- **結果**: エミュレータの時間で、コミッショニングは 16.9 秒（PASE だけで約 6 秒。暗号が遅い）。購読、toggle（デバイスの `matter.onoff on=true` と、レポートの `value=1`）、読み（`1`）が通った。実時間では約 2 分 44 秒かかる。
- 最初は `run net` の直後に `run matter 1` を始めて、`net` がまだ待ち受けておらず失敗した（`matter.failed step=1`）。3 秒待つようにした。
- 実機（引数なし）は今までどおり: fabric を読み戻し、smctl からの toggle が通った。

## 17. 残り
- UDP の受信の通知（今は 20 ms ごとに見に行く）。
- BLE: Wi-Fi の資格情報を BLE で渡すコミッショニング（`AddOrUpdateWiFiNetwork` と `ConnectNetwork`）。Tab5 はどちらの側も、相手がもうネットワークにいる前提。
- 購読: 構造体や文字列の値はログに出さない（数と真偽と null だけ）。イベントの購読はまだない。
- t5emu の試験は、デバイスとコントローラが同じプロセスで同じコア。別の実装（chip-tool など）との相互接続は実機でしか見ていない（PC の smctl と例）。
