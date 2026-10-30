# 16. M5: 更新（カーネルの A/B とロールバック、アプリの A/B、IPC）

- 日付: 2026-10-30
- 状態: **M5a を実装した。t5emu と実機で、正しい更新の確定、パニックからのロールバック、自己試験の失敗からのロールバックを確認した（§6.4、§6.7）。実機でまれに SYSTIMER の読み出しで止まる問題が見つかった（§6.7、未解決）**
- 前提: doc/05（更新の調査）、doc/14（T5A、アプリストア、制御チャネル、インストール）、boot/（ロールバック有効の ESP-IDF 6.0 ブートローダ、パーティション表）
- 出典の表記: `~/esp-idf/6.0/components/` からの相対パス。（推測）は推測、（判断）は設計判断。

## 0. 範囲と順序
| 段 | 内容 | 完了の条件 |
|---|---|---|
| **M5a** | **カーネルの更新**: 制御チャネルで受け取った新しいカーネルを、動いていない側のスロット（ota_0 / ota_1）に書き、署名を確かめてから otadata を切り替えて再起動する。新しいカーネルは自己試験に通ったら自分を確定する。確定しないまま落ちたら、ブートローダが元のカーネルに戻す | 実機で、正しいカーネルへの更新と、壊れたカーネルからの自動ロールバックの両方を見せる |
| M5b | アプリの A/B: 更新したアプリの前の版をストアに残し、新しい版が確定しなければ前の版に戻す | 落ちるアプリを入れると、前の版に戻る |
| M5c | IPC チャネル（ハンドルの新しい種類）。プロセス間でメッセージを送る | 2 つのアプリがチャネルでやりとりする |

この文書は M5a を詳しく決め、M5b・M5c は方針だけ書く（実装の前に節を足す）。

## 1. ブートローダの仕組み（使うところだけ）
- otadata は 2 セクタ（0xD000、0xE000）。各セクタの先頭が `esp_ota_select_entry_t { u32 ota_seq; u8 seq_label[20]; u32 ota_state; u32 crc; }`（32 バイト）[bootloader_support/include/esp_flash_partitions.h:66-84]。
- `crc` は `ota_seq` の 4 バイトだけの CRC-32。`esp_rom_crc32_le(UINT32_MAX, …)` で、zlib の CRC-32 と多項式・反転は同じだが **初期値が 0**（レジスタの初期値が `~UINT32_MAX`）[bootloader_support/src/bootloader_common_loader.c:66-69]。esp-bootloader-esp-idf 0.6.0 も `init: 0, refin/refout: true, xorout: 0xFFFF_FFFF` としている [non_rom.rs:5-14]。
- **有効なエントリ**: `ota_seq` が 0xFFFF_FFFF でなく、状態が INVALID（3）でも ABORTED（4）でもなく、`crc` が合うもの [bootloader_common_loader.c:71-79]。2 つとも有効なら `ota_seq` の大きいほう [同:144-160]。起動するスロットは `(ota_seq - 1) % 2`（0 = ota_0、1 = ota_1）[bootloader_utility.c:440-444]。
- **ロールバック**（`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`。boot/sdkconfig.defaults で有効にしてある）[bootloader_utility.c:395-450]:
  1. 起動のたびに、まず `PENDING_VERIFY`（1）のエントリを `ABORTED` に書き換える（前回の起動で確定されなかった）。
  2. 有効なエントリのうち新しいほうを選ぶ。その状態が `NEW`（0）なら `PENDING_VERIFY` にしてから起動する。
  3. アプリ（ここではカーネル）が `VALID`（2）に書き換えれば確定。確定しないまま再起動すると、1 によって ABORTED になり、古いほうのエントリが選ばれる。
- 選んだスロットのイメージが壊れていれば、ブートローダはほかのスロットを順に試す [bootloader_utility.c:578-628]。
- otadata が 2 つとも初期状態（0xFF）なら ota_0 を起動し、`ota_seq = 1, VALID` をセクタ 0 に書く [bootloader_utility.c:410-421,490-505]。

## 2. 更新パッケージ T5K（判断）
```text
+---------------------------+ 0
| ESP app image             | espflash save-image の出力そのまま（ヘッダ、セグメント、
|                           | チェックサム、SHA-256）。長さ L は 16 の倍数
+---------------------------+ L
| T5K ヘッダ（16 バイト）    | "T5K1", u16 header_version = 1, u16 header_size = 16,
|                           | u32 signed_size = L + 16, u32 total_size
+---------------------------+ L + 16 = signed_size   <- 署名は [0, signed_size) を覆う
| フッタ: Signature TLV      | T5A と同じ（鍵 ID 8 バイト + Ed25519 署名 64 バイト）
+---------------------------+ total_size
```
- **パッケージ全体を、そのままスロットの先頭から書く**。ブートローダは ESP image の後ろを読まないので、末尾の T5K は起動の邪魔にならない。署名は、書き込んだ後にフラッシュ上のバイトで確かめる（RAM に全体を持たなくてよい。T5A のインストールと同じ）。
- 版と `secure_version` は、ESP image の中の `esp_app_desc_t`（最初のセグメントの先頭。magic 0xABCD_5432）から読む。カーネルの `APP_DESC` がそのまま唯一の出所になる（TLV に重ねて書かない）。
- ESP image の検査（署名が合った後に行う）: magic 0xE9、セグメント数 1〜16、各セグメントが image に収まる、`chip_id` = 18（ESP32-P4）、`min_chip_rev_full` ≤ 100 ≤ `max_chip_rev_full`（Tab5 は v1.0）、`hash_appended` = 1、セグメントを歩いた長さ（+ チェックサムの詰め物 + SHA-256 の 32 バイト）が L と一致、`esp_app_desc_t` の magic [bootloader_support/include/esp_app_format.h:76-122; esp_app_format/include/esp_app_desc.h:21-41]。
- 付加された SHA-256 とチェックサムは、カーネルでは計算しない（判断）。Ed25519 の署名が image 全体を覆っているので、書き込みの誤りも改ざんも署名で分かる。SHA-256 はブートローダが起動のたびに確かめる。
- 鍵はアプリと同じ信頼鍵（`TRUSTED_KEYS`）。カーネル用に鍵を分けるのは、鍵の階層（doc/05 §5.3）を作るときにする。
- ツール: `elf2t5a kernel IN.bin OUT.t5k --key K`（署名だけを足す）。Makefile が `build/t5-kernel-esp32p4v1.t5k` を作る。

## 3. カーネルの手順（M5a）
### 3.1 自分のスロットを知る
- ブートローダは、カーネルのイメージをフラッシュの MMU のエントリ 0 から対応付けている。エントリ 0 の物理ページ × 64KiB が、自分のスロットの先頭（0x1_0000 か 0x21_0000）（推測。M3b でエントリ 0〜31 がカーネルに使われていることは確認済み）。どちらでもなければ、更新は受け付けない。
- 起動時に `kupdate.boot` で、自分のスロット、otadata の 2 エントリ（seq、state、crc の正否）、自分の版と secure_version を記録する。**ロールバックが起きた後**（新しいほうのエントリが ABORTED か INVALID で、もう一方のスロットを指している）なら、`kupdate.rolled_back` を出す。

### 3.2 受信と書き込み（制御チャネル）
- `kupdate_begin(length)`（新しいコマンド 11）→ `install_data`（今までのデータフレームをそのまま使う）→ `install_end`。カーネルは、最後に始まったほう（アプリかカーネル）のセッションにデータを渡す。
- 書き込み先は **動いていないスロット** だけ。`flash.rs` の書き込みの許可を「ストア + otadata + 動いていないスロット」にする。動いているスロットとブートローダ、パーティション表には、今までどおり書けない。
- 各 4KiB セクタは、データがそこに来たときに消去する（アプリのインストールと同じ）。スロットの残り（パッケージの後ろ）は消さない（判断。ブートローダは L の後ろを読まない）。

### 3.3 確認と切り替え（`install_end`）
1. 受け取った長さが `total_size` と一致する。
2. フラッシュの線形の窓の上で、署名を確かめる。
3. ESP image を検査する（§2）。
4. **ダウングレードの防止**: 新しい `secure_version` ≥ 動いているカーネルの `secure_version`。
5. otadata を書く: 新しい `ota_seq` は「今の最大の seq より大きく、`(seq - 1) % 2` が書き込んだスロットになる最小の値」（2 つとも初期状態なら、スロット番号 + 1）。書く先は、**今有効なエントリのない側のセクタ**（有効なエントリが 1 つもなければセクタ 0。esp_ota_ops.c の `esp_rewrite_ota_data` と同じ考え方）。状態は `NEW`。セクタを消去してから 32 バイトを書き、読み戻して確かめる。
6. `kupdate.ready`（スロット、seq、版）を出す。**再起動はしない**。ホストが `reboot`（新しいコマンド 12）を送る（判断。更新と再起動を分けておくと、書き込みだけを試せる）。

### 3.4 新しいカーネルの確定
- 起動時に、選ばれたエントリ（自分のスロットを指す、新しいほうの有効なエントリ）が `PENDING_VERIFY` なら、**自己試験が通ったときに `VALID` に書き換える**（`kupdate.confirmed`）。
- 自己試験が失敗したら `INVALID` に書き換えて再起動する（`kupdate.rejected`）。ブートローダは古いほうを起動する。
- パニックや固まった場合は、RWDT（約 4 秒）でリセットされ、ブートローダが ABORTED にして古いほうを起動する（§1）。
- エントリの書き換えは、同じセクタの消去 → 書き込み。消去と書き込みの間に電源が切れると、そのセクタは初期状態になり、もう一方のセクタのエントリ（古いカーネル）が選ばれる。**古いカーネルに戻るだけで、起動できなくはならない**（推測。もう一方のセクタが有効である限り）。

### 3.5 再起動
- ROM の `software_reset`（0x4FC0_0094、`esp_rom_software_reset_system`）[esp_rom/esp32p4/ld/esp32p4.rom.ld:52; esp32p4.rom.api.ld:48]。システム全体がリセットされ、ブートローダがクロックを設定し直す（推測。§3.6 の試験で reset_cause を記録する）。

### 3.6 試験
- **t5emu**: エミュレータはブートローダを持たないので、受信、書き込み、署名の確認、otadata の書き込みまでを試す。エミュレータの MMU のエントリ 0〜31 を、ブートローダと同じく ota_0 に向けておく。書き込みの保護は「otadata と ota_1 とストア」だけを許す（ota_0 やブートローダへの書き込みは止める）。`reboot` は ROM の `software_reset` で止まる（`Stop::SoftwareReset`）。
- **単体試験**（abi）: otadata の CRC（ブートローダが書いた実物の値と比べる）、seq の選び方、ESP image の検査。
- **実機**（Tab5）:
  1. 正しい更新: 版を上げたカーネル（0.2.0）を `kupdate` → `reboot` → ota_1 で起動し、自己試験の後に確定。もう一度 `reboot` しても ota_1 から起動する。
  2. 戻る更新（自己試験の失敗）: 自己試験を必ず失敗させるカーネル（cargo の feature）を入れる → INVALID にして再起動 → 元のカーネルが起動し、`kupdate.rolled_back`。
  3. 戻る更新（パニック）: 起動直後にパニックするカーネル → RWDT → ブートローダが ABORTED に → 元のカーネル。
  4. 署名の壊れたパッケージ、secure_version の低いパッケージは、otadata を書かずに拒否される。

### 3.7 実機の扱い（作業の約束）
- 試験の後、カーネルは ota_0 と ota_1 のどちらで動いているか分からない。**`espflash write-bin 0x10000`（ota_0 だけ）で書いても起動しない**ことがある（otadata が ota_1 を指していれば）。開発時の書き込みは、①カーネル自身の `kupdate`（`make kupdate`）か、②ota_0 に書いて otadata を消す（`make flash-kernel-p4`。otadata が初期状態なら ota_0 が起動する）にする。
- 元のファームウェアのバックアップ（~/tab5-backups/）はそのまま。ブートローダとパーティション表は書き換えない。

## 4. M5b: アプリの A/B（方針）
- 同じ名前のアプリをインストールしたとき、前の版のエントリを消さずに `previous` として残す（カタログのエントリに状態を足す。エクステントも残す）。
- 新しい版は `PendingVerify` で入る。最初の起動から一定時間（例: 2 秒）異常終了しないか、アプリが `confirm` を呼べば `Valid` にし、前の版を消す。`PendingVerify` のまま異常終了すれば、前の版に戻す。
- 詳しい状態遷移と、カタログの形式の変更（版を上げる）は、実装の前にここへ足す。

### 4.1 カタログの変更（形式 v2）
- エントリの `state` に `Previous = 4` を足す。同じ名前のエントリは、「今の版」（`Valid` か `PendingVerify`）1 つと、`Previous` 1 つまで。カタログの `version` を 2 にする。カーネルは v1 と v2 を読み、v2 を書く（v1 には `Previous` がないので、そのまま読める）。
- 名前で引く（`run`、自動起動、XIP のエクステント）のは、今の版だけ。`Previous` は戻るためだけに残す。
- カタログの書き込みは今までどおり A/B の 2 セクタで、1 回の書き込みで状態が切り替わる（途中で電源が切れても、どちらかの状態のまま）。

### 4.2 状態遷移
| きっかけ | 前 | 後 | イベント |
|---|---|---|---|
| install（同じ名前の版がない） | なし | `Valid` | `store.installed` |
| install（`Valid` の版がある） | 旧 `Valid` | 新 `PendingVerify` ＋ 旧 `Previous`（それより前の `Previous` は消す） | `store.installed pending=true` |
| install（`PendingVerify` の版がある） | 新 `PendingVerify` ＋ `Previous` | 新しいほうを `PendingVerify` に（確かめていない版は消す）、`Previous` は残す | 同上 |
| `PendingVerify` の版のプロセスが、普通に終わる（終了コードは問わない）か、2 秒動き続ける | `PendingVerify` ＋ `Previous` | `Valid`、`Previous` を消す | `store.confirmed` |
| `PendingVerify` の版のプロセスが、例外で止められる（`proc.fault`） | `PendingVerify` ＋ `Previous` | `Previous` を `Valid` に戻し、新しい版を消す | `store.rolled_back` |
| ホストからの `kill` | — | 変えない（アプリのせいではない） | — |
| `remove NAME` | どれでも | 両方消す | `store.removed` |
| `rollback NAME`（新しい制御コマンド。試験と手動用） | `PendingVerify` か `Valid` ＋ `Previous` | `Previous` を `Valid` に戻す | `store.rolled_back` |

- 消したエントリのエクステントは、空きに戻る。ただし動いている XIP のプロセスのテキストは、今と同じく `busy` で守る（doc/15 §4）。
- 再起動をまたいでも `PendingVerify` のまま。次に動かしたときに決まる。
- secure_version: install のときの比較は、今の版と `Previous` の大きいほうと比べる。`PendingVerify` の版から `Previous` に戻ることは許す（新しい版が動かなかったので、まだ「上げた」ことにはしない。ESP-IDF の anti-rollback も、確定したときに eFuse を上げる）。
- アプリから確定するシステムコール（`confirm`）は、M5b では足さない。2 秒の規則で足りないアプリが出てきたら足す（ABI の minor を上げる）。

### 4.3 試験（エミュレータ）
- greeter 0.1 の上に greeter 0.2 を入れる → `PendingVerify` ＋ `Previous` → 動かすと普通に終わる → `store.confirmed`、`list` で 1 つだけ。
- greeter の上に、例外で止まるアプリ（`fault` を `--name greeter --version 0.3.0` で署名したもの）を入れる → 動かすと `proc.fault` → `store.rolled_back` → もう一度動かすと元の greeter が `hello.greet` を出す。
- `rollback`、`remove` で両方消えること、XIP のアプリでの同じ流れ。

## 5. M5c: IPC チャネル（方針）
- ハンドルの種類に「チャネル」を足す。`open("chan:NAME")` で両端をつなぎ、`write` でメッセージ（最大 256 バイト程度）を送り、相手は `wait` で受信を待って `read` で取り出す。権限は Permissions TLV の名前で与える。
- カーネルの中にメッセージのキュー（固定長）を持つ。共有メモリ（`shm_map`）は M6 の表示で考える。

### 5.1 モデル
- チャネルは、名前のついた **2 つの端（A と B）** を持つ双方向の管。`open("chan:NAME", READ|WRITE|WAIT)` で、最初に開いたプロセスが端 A、次が端 B を得る。両端が埋まっていれば `Error::Busy`。名前は Permissions TLV に `chan:NAME` として書いたアプリだけが開ける（今の `open` の仕組みそのまま。両方のアプリに要る）。
- 片方の端が閉じられた（`close`、またはプロセスの終了）あとで、同じ名前を開くと、空いた端につながる（サーバが再起動できる）。両端が閉じられたらチャネルは消える。
- メッセージは**境界を保つ**（1 回の `write` が 1 回の `read` になる）。1 つ最大 256 バイト。各端の受信キューは 4 つ。
- カーネルの中の表は固定: チャネル 8 本 × 2 端 × 4 メッセージ × 256 バイト ≒ 16 KiB（カーネルの .bss、内部 SRAM）。

### 5.2 システムコール（今ある番号を使う。ABI 0.6）
| 呼び出し | 動き |
|---|---|
| `write(h, buf, len)` | 相手の端の受信キューにコピーする。`len` > 256 は `Error::Size`、キューが一杯なら `Error::Busy`（待たない）、相手がいない・閉じたなら `Error::Off`。成功で `len` を返す |
| `read(h, buf, len)` | 自分の受信キューの先頭を取り出してコピーし、長さを返す。`len` が足りなければ `Error::Size`（取り出さない）。空なら `Error::Busy` |
| `wait([h..], timeout)` | `chan::READABLE`（値 = キューの数）か、キューが空で相手が閉じていれば `chan::PEER_CLOSED` |
| `ctl` | なし（`NoSupport`） |

- 書き手がキューの空きを待つ手段（`WRITABLE` のイベント）は今は作らない。`Busy` なら `yield` してやり直す。必要になったら足す。
- 読み出しと書き込みは、今と同じく、ユーザの範囲をソフトウェアで確かめてからコピーする（doc/07 §4.1）。
- プロセスが終わったら（exit・例外・kill）、そのときにハンドルをすべて閉じる（今はスロットを片付けるまで残っている）。相手はすぐ `PEER_CLOSED` を受ける。

### 5.3 カーネルの記録
- `chan.open`（pid、名前、端）、`chan.closed`（pid、名前、端、受信キューに残っていた数）。`ps` の `proc.info` の `handles` は今のまま。

### 5.4 試験
- 新しいアプリ `echo`: arg0 = 0 でサーバ（`chan:echo` を開き、受けたメッセージの各バイトに 1 を足して返し、相手が閉じたら終わる）、arg0 = 1 でクライアント（長さを変えて N 個送り、返ってきたものを確かめ、結果を `echo.done` で出して閉じる）。
- 自己試験 M5c（QEMU・t5emu・実機で同じ）: サーバとクライアントを同時に動かし、クライアントが全部確かめて終わり、サーバが `PEER_CLOSED` で終わること。ほかに、Permissions にない名前は `Denied`、3 つ目の `open` は `Busy`、257 バイトの `write` は `Size`、一杯のキューへの `write` は `Busy`。

## 6. M5a の実装の記録（2026-10-30）

### 6.1 実装したもの
- `abi/src/otadata.rs`: otadata のエントリ（読み書き、CRC、有効性）、ブートローダと同じ選び方（`active`）、次のエントリの置き場所と seq（`next`）。単体試験で、Python の `binascii.crc32(data, 0xFFFFFFFF)`（初期値 0 の CRC と同じ）と値を比べた。
- `abi/src/t5k.rs`: ESP app image を歩く（セグメント、チェックサムの詰め物、SHA-256、`esp_app_desc_t`）、チップの判定、T5K のヘッダと署名の切り出し。
- `abi/src/ctl.rs`: コマンド `kupdate_begin`（11）、`reboot`（12）、`kinfo`（13）。`install_commands(image, kernel)`。ホストの `kupdate FILE` は、`install FILE` と同じくフレームに展開する。`reboot` には返事がない（ホストは待たない）。
- `elf2t5a kernel IN.bin OUT.t5k --key K`、`elf2t5a verify-kernel`。`make image-p4` が `build/t5-kernel-esp32p4v1.t5k` も作る。
- カーネル:
  - `kernel/src/kupdate.rs`: 受信、書き込み、署名と image の確認、ダウングレードの拒否、otadata の書き込みと読み戻し、起動時の記録（`kupdate.otadata`、`kupdate.slot`、`kupdate.rolled_back`）、自己試験の後の確定・拒否。
  - `flash.rs`: 書き込めるのは「ストア + otadata + 動いていないスロット」だけにした。動いているスロットは、MMU のエントリ 0 から求める（`Platform::kernel_slot`）。
  - `Platform::chip_revision`（eFuse の WAFER_VERSION。Tab5 は 100）、`Platform::reset`（ROM の `software_reset`）。
  - `APP_DESC` の版と secure_version を、ビルド時の環境変数（`T5_KERNEL_VERSION`、`T5_KERNEL_SECURE_VERSION`）で変えられるようにした（試験用のカーネル）。
  - 試験用の feature: `test-bad-panic`（起動直後にパニック）、`test-bad-selftest`（自己試験が必ず 1 件失敗）。
- `make kupdate-test-kernels`: `build/kupdate/` に good（0.1.1-good、secure_version 1）、bad-selftest、bad-panic、tampered（署名の後に 1 バイト変えたもの）を作る。
- `make kupdate PROBE=… [T5K=FILE]`: 動いているカーネルに更新させて再起動する。`make flash-kernel-p4`: ota_0 に書いて otadata を消す（どちらのスロットが選ばれていても、ota_0 から起動する状態に戻す）。
- t5emu: MMU のエントリ 0〜31 をブートローダと同じく ota_0 に向けた。書き込みの許可を「otadata、ota_1、ストア」にした。ROM の `software_reset` で止まる（`Stop::SoftwareReset`）。eFuse のチップの版を v1.0 にした。

### 6.2 確かめたこと
- **t5emu**（`make test-p4emu` の 5 本目）: 改ざんしたパッケージは `bad signature` で拒否され、otadata は変わらない。正しいパッケージは ota_1 に入り（98KB、消去 24 回、書き込み 384 回）、otadata のセクタ 0 に `seq 2, new`（CRC 正）が書かれ、`reboot` で止まる。ota_0 やブートローダへの書き込みがあれば t5emu が止める。
- **実機**: 新しいカーネル（ota_0）の起動時の `kinfo` で、ブートローダが書いたエントリ（`seq 1, valid`）の CRC が、カーネルの計算と一致した（`crc_ok=true`）。自分のスロットを 0 と正しく判定した。
- QEMU virt: スロットが分からないので、更新は受け付けない（自己試験は通る）。

### 6.3 実機の試験の手順（ota_1 と otadata を書き換える）
`PROBE=303a:1001:30:ED:A0:E0:CB:86` として:
1. `make kupdate PROBE=$PROBE T5K=build/kupdate/good.t5k` → 再起動後、`make rtt PROBE=$PROBE CMD=kinfo` で、ota_1 の 0.1.1-good が動き、`kupdate.confirmed` が出て、otadata の seq 2 が `valid` になっていること。
2. `make kupdate PROBE=$PROBE T5K=build/kupdate/bad-selftest.t5k` → 新しいカーネル（ota_0）は自己試験に失敗して `kupdate.rejected` を出し、再起動する。ブートローダは ota_1 の 0.1.1-good を起動し、`kupdate.rolled_back`（seq 3、invalid）が出ること。
3. `make kupdate PROBE=$PROBE T5K=build/kupdate/bad-panic.t5k` → **10 秒ほどプローブをつながない**（つなぐとウォッチドッグが止まり、リセットされない）。パニック → RWDT → ブートローダが ABORTED にして ota_1 に戻る。そのあと `kinfo` で `kupdate.rolled_back`（aborted）が出ること。
4. `make kupdate PROBE=$PROBE`（開発版。secure_version 0）→ `secure_version rollback` で拒否されること。`make kupdate PROBE=$PROBE T5K=build/kupdate/tampered.t5k` → `bad signature`。
5. 元に戻す: `make flash-kernel-p4 PORT=/dev/ttyACM3`（ota_0 に開発版を書き、otadata を消す）。

### 6.4 実機の結果（2026-10-30。[logs/m5a-kupdate-tab5-20261030.txt](logs/m5a-kupdate-tab5-20261030.txt)）
手順 1〜3 を、利用者が 1 回の入力で続けて実行した（t5log は再起動をまたいでつながったままだった）。
- **正しい更新**: 98KB の書き込みに 1.7 秒（消去 25 回、書き込み 385 回）、署名の確認 95ms。otadata のセクタ 1 に `seq 2, new` を書いた。再起動後、ブートローダは `pending_verify` にして ota_1 の 0.1.1-good を起動した（reset_cause 3 = ソフトウェアによるシステムリセット）。PVT と 360MHz、自己試験も通り、最終的に `seq 2` は `valid` になった（確定）。
- **パニックするカーネル**: ota_0 に `seq 5` で入った。起動直後に `kernel.panic` を出して止まり、次の起動でブートローダが `seq 5` を `aborted` にし、ota_1 の 0.1.1-good に戻った。カーネルは `kupdate.rolled_back`（ota_seq 5、aborted、slot 0）を出した。
  - ただし **自動ではリセットされなかった**。プローブがつながったままで、ウォッチドッグが止まっていたため（§6.3 の注意のとおり）。プローブからリセットして戻した。プローブがなければ RWDT でリセットされるはず（推測。M2 の L3 試験で、パニック → RWDT のリセットは確認済み）。
- **自己試験に失敗するカーネル**: そのときのログは残っていない（次の更新で上書きされた）。ただし、パニックするカーネルの seq が 4 ではなく 5 になったことから、その前にセクタ 0 に CRC の正しい `seq 3` があった（ota_0 を指す）と分かり、それが有効なまま選ばれていなかった（invalid）ことと矛盾しない。直接の確認は、もう一度単独で行う必要がある。
- 手順 4（ダウングレードと改ざんの拒否）は、実機では未実施（t5emu では改ざんの拒否を確認）。

### 6.5 2 回目の bad-selftest の試験で止まったこと（2026-10-30、未解決）
- 利用者が `make kupdate … T5K=build/kupdate/bad-selftest.t5k` を実行したところ、カーネルは 387 フレームのどれにも答えず（`kupdate.begin` も出ない）、`--until-done` が待ち続けた。フラッシュには何も書かれていない。
- 直前の起動（boot 28。プローブのリセットで起動し、`kinfo` に答えた）のログは、`kinfo` の応答（起動後 1.53 秒）で終わっていて、その後の約 40 分に何も記録がない。
- そのあと `t5log --reset` と `probe-rs reset` では起動しなかった（RTT の制御ブロックが 0 のまま、フラッシュの窓も対応付けられていない）。`espflash monitor`（USB からのリセット）で起動し直すと、ブートローダが ota_1 の 0.1.1-good を正常に起動した。ROM が出した直前の PC は 0x4FF1_0E4C で、カーネルのコードとデータ（〜0x4FF0_D310）の外。
- 再現を試みた: USB リセットで起動して 36 分放置、プローブのリセットで起動して 40 分放置 → どちらも `ps` に正常に答えた。**再現しなかった**。
- プローブ（probe-rs）をつなぐとウォッチドッグが止まるので、カーネルが止まると誰もリセットしない。原因は分からないが、少なくとも気づけるように、`t5log --until-done` は、答えのない要求があり 30 秒ログが来なければ、エラーで終わるようにした。

### 6.7 自己試験の失敗からのロールバック（2 回目）と、SYSTIMER で止まる問題
- `make kupdate … bad-selftest.t5k`: ota_0 に `seq 7` で入り、再起動した。bad-selftest は自己試験に失敗して `kupdate.rejected`（`seq 7` を invalid に。written=true）を出し、自分でリセットした。ブートローダは ota_1 の 0.1.1-good を起動した。チップのリセットの後の `kinfo` で、`seq 7` が `invalid`、`kupdate.rolled_back`（ota_seq 7、invalid、slot 0）を確認した。**3 通りの戻り方がすべて実機で働いた**。
- ただし、そのロールバック後の起動（boot 32、0.1.1-good）が止まっていた。`t5log status`（新しく作った。コア 0 を止めて pc と CSR を読み、再開する）で 4 回読むと、**毎回 pc = 0x4000_B2BE**（`clear_alarm` の中の `SYSTIMER_CONF` の読み出し）で、`mepc` = 0x4300_01DC（スロット 2 の XIP アプリ = 自己試験 M4b の xipcheck の ecall）。**SYSTIMER のレジスタの読み出しが終わらない（バスが止まっている）**。
  - t5log が再起動をまたいでつながっていたので、ウォッチドッグは止まっていた。プローブからのリセットでも戻らず、USB からのチップのリセット（rst 0x17）でだけ戻る。§6.5 の件も、同じ止まり方だった可能性が高い（推測）。
  - RTT のログが `psram.controller` で切れていたのは、読む人がいない間にバッファが埋まり、新しいフレームが捨てられたため（止まった場所ではない）。
  - 2 回とも、**360MHz（M4c）にしてから**、**プローブ（RTT の読み出し）がつながっている間**に起きている。M4b までの 90MHz では、長時間の RTT の利用で一度も起きていない（推測の材料）。原因の候補: ①360MHz（MEM 180MHz / APB 90MHz）でのペリフェラルへのアクセス、②プローブのメモリ読み出しとコアのアクセスの競合、③PVT による電圧。切り分けは未実施。

### 6.8 止まる問題の再現の試み（360MHz + PVT、2026-10-30）
書き込みなしで切り分けられるように、カーネルに制御コマンド `clock 90|360`、`pvt on|off`、`power` を足した（`10a21b3`）。開発版のカーネルを ota_0 に書き、otadata を消した状態で、既定の 360MHz + PVT のまま次を試した。**どれも止まらなかった。**

| 試験 | 内容 | 結果 |
|---|---|---|
| a | 15 分、負荷をかけ続ける（t5log はずっとつないだまま） | xipcheck 604 回、spin 608 回すべて完了。hp_dbias はほぼ 26 |
| b | アプリを動かしながら、つなぐ・外すを 150 回 | すべて応答 |
| c | t5log をつないだまま `reboot` を 50 回 | 49 回の起動を観測、自己試験はすべて合格 |
| d | フラッシュに書いた直後の `reboot` を 30 回 | 自己試験はすべて合格 |
| e | 2 時間ほぼ放置し、1 分ごとにつないで xipcheck と `power` を実行 | 120 分すべて応答。hp_dbias は 120 回とも 26 |
| f | t5log をつないだまま `reboot` を 300 回（自己試験の M4b で止まった場面の繰り返し） | 299 回の起動を観測、自己試験はすべて合格、`ps` 599 回すべて応答 |

止まったのは 2 回とも、カーネルの更新の試験（別のスロットへの切り替えのあと）の前後だった。どの条件が効いているかは、まだ分からない。

### 6.9 分かったこと・残っていること
- プローブ（probe-rs）がつながっている間でも、確定前のカーネルのパニックはリセットになり、前のカーネルに戻る（§6.10）。
- 開発版（secure_version 0）は、secure_version 1 の試験用カーネルのあとでは `kupdate` では入らないので、`make flash-kernel-p4`（otadata を消して ota_0 に書く）で戻す。今の Tab5 はこの状態（§6.8）。

### 6.10 確定前のカーネルは止まらずにリセットする（2026-10-30）
§6.4 のパニックからのロールバックは、プローブがつながっているとウォッチドッグが止まっているため、プローブからのリセットが要った。これを、プローブの有無によらず戻れるようにした。
- **パニック**: 起動したときの自分の otadata が `pending_verify`（更新されたばかりで、まだ確定していない）なら、パニックの処理は止まらずに `kupdate.panic_reset` を出してチップをリセットする（ROM の software_reset）。ブートローダは `pending_verify` のまま起動されたものを aborted にして、前のカーネルに戻る。確定済みのカーネルのパニックは、これまでどおり止まる（デバッグで調べられるように）。
- **止まったとき**: `pending_verify` で起動したら、カーネルはウォッチドッグ（RWDT、約 4 秒）をもう一度有効にする（`kupdate.pending watchdog=true`）。プローブが起動のあとでつなぎ直して止めた場合までは防げない。また、ウォッチドッグのリセットは HP 系だけのリセット（LP SRAM のログを残すため）で、§6.7 の止まり方はプローブからのリセットでも戻らなかったので、それで戻るかどうかは確かめていない。
- 確定前にブレークポイントで数秒止めると、リセットされる（確定してからデバッグする）。
- 試験: `make test-p4emu` の 6 回目。otadata を「seq 1（ota_0）、pending_verify」にして（`elf2t5a otadata`）、bad-panic のカーネルを動かす。`kupdate.pending`、`kernel.panic`、`kupdate.panic_reset` が出て、ソフトウェアリセットで終わることを確かめる（`t5emu --expect-reset`）。
- 実機（2026-10-30）: ota_0 の開発版（0.1.0）から `make kupdate … T5K=build/kupdate/bad-panic.t5k` を t5log をつないだまま実行した。bad-panic は ota_1 に `seq 2` で入り、パニックして自分でリセットし、ブートローダが `seq 2` を aborted にして ota_0 の開発版に戻った（`kupdate.rolled_back ota_seq=2 state=aborted slot=1`）。**プローブからのリセットなしで戻った**（§6.4 ではプローブからのリセットが要った）。
- `make kupdate PROBE=<Tab5> T5K=build/kupdate/*` は、利用者が Claude Code の許可設定（`.claude/settings.local.json`、コミットしない）に入れたので、Claude が実行できる。それ以外の書き込みは、これまでどおり確認してから。

## 7. M5b の実装の記録（2026-10-30）
§4.1〜4.3 のとおりに実装した。
- `t5_abi::store`: `State::Previous`（4）、`Entry::is_current`、カタログ v2（v1 も読む）。
- カーネルの `store`: `entry`/`image` は今の版だけを引く。`install_end` が §4.2 の表のとおりに `PendingVerify` と `Previous` を作る。`confirm`、`rollback`。secure_version は今の版と `Previous` の大きいほうと比べる。
- 決め方（`main.rs` の `settle_updates`、制御チャネルを読むたびに呼ぶ）: `PendingVerify` の版から起動したプロセスは、起動した時刻と**その版の最初のエクステント**を覚える。普通に終われば `store.confirmed by=exit`、2 秒動き続ければ `by=running`、例外で止められれば `store.rolled_back reason=fault`。そのあいだに別の版が入っていたら（エクステントが違う）何もしない（`store.settle_skipped`）。エクステントで比べるのは、同じ名前の新しい版を、古い版のプロセスの結果で確定しないため。
- 制御コマンド `rollback NAME`（`cmd` 17）。
- 試験用のイメージ: `greeter-0.2.t5a`（hello を greeter 0.2.0 として署名）、`greeter-bad.t5a`（fault を greeter 0.3.0 として署名。`run greeter 1` で load access fault）、`spin-xip-0.2.t5a`。
- t5emu の `--ctl 'wait-ms N'`: それより前のコマンドがすべて答えられ、さらに N ミリ秒たつまで、後のコマンドを送らない（アプリが終わるのを待つため）。
- `make test-p4emu` の 5 回目（M5b）で確かめたこと: greeter 0.1 の上の 0.2 が `PendingVerify` ＋ `Previous` になり（`list` の state 1 と 4）、普通に終わって確定し、`list` に 1 つだけ残る。greeter-bad は例外で止められて 0.2 に戻り、次の `run greeter` は 0.2（`hello.greet`）。spin-xip 0.2（XIP）は 2 秒動いたところで `by=running` で確定。`rollback` コマンドで戻り、`remove` で両方消える。
- 実機（2026-10-30、[logs/m5b-apps-tab5-20261030.txt](logs/m5b-apps-tab5-20261030.txt)）: 同じ流れを、1 手順ずつ t5log をつなぎ直して実行した（利用者が実行。アプリのインストールはフラッシュへの書き込みなので）。すべて t5emu と同じ結果になった: 0.2 が `by=exit` で確定、greeter-bad（0.3）は load access fault で 0.2 に戻り、次の起動は 0.2（hello）、spin-xip 0.2 は起動から約 2.0 秒で `by=running` で確定、`rollback` コマンドで戻り、`remove` で消えた。もとからあった v1 のカタログ（`catalog_seq` 13 まで）は読めて、その上に v2 で書いた。ログのアプリのイベントが `<unknown …>` なのは、t5log にカーネルの ELF しか渡していないため（内容: pid101/103 は hello の文、pid104 は spin.done）。
