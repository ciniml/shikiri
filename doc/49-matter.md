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
