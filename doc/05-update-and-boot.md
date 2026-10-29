# 05. 更新（ブート・OTA・署名・ロールバック）

調査日: 2026-10-29
対象: M5Stack Tab5（ESP32-P4, Flash 16MB）上の Rust 製 OS。カーネル（M モード）とアプリ（U モード）をそれぞれ更新する仕組みを検討する。

> 表記: 「（推測）」は出典で裏付けていない設計判断や見込み。ローカルソースは ESP-IDF 6.0（`~/esp-idf/6.0`）。
> クローンしたコミット: esp-hal `5c0820cf9e2b`、embassy `c2413d5994bb`、tock `d4979a4c09b6`（2026-10-27〜29 時点）。

---

## 0. 結論（先に要点）

### カーネルの更新は ESP-IDF の 2nd stage bootloader を流用する（自前ブートローダは作らない）
- **ESP-IDF 6.0 の 2nd stage bootloader**（`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y` でビルドしたもの）、**パーティションテーブル**、**otadata**、**ota_0/ota_1 の A/B 構成**をそのまま使う。Rust カーネルは「ESP-IDF の app image フォーマット＋`esp_app_desc_t`」を満たすイメージとしてビルドする。esp-hal なら `esp_bootloader_esp_idf::esp_app_desc!()` で埋め込み、espflash で変換する。**ただし Tab5（P4 v1.0）では esp-hal を使えず、`esp-bootloader-esp-idf` も `esp32p4` feature で `esp-hal/esp32p4` を要求するためリンクできない（文書 03 §1.6）。`esp_app_desc_t` 構造体と otadata 形式は自前で写す**（統合時注記）。
- OTA の書き込み、otadata の切り替え、`PendingVerify → Valid` の確定は、`esp-bootloader-esp-idf` の `OtaUpdater` / `Ota` で行える（[ota_updater.rs](https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-bootloader-esp-idf/src/ota_updater.rs)）。esp-hal を使わない場合も、otadata は 32 バイトの構造体 2 個（CRC は ota_seq に対する CRC32）だけなので、自前実装は難しくない（§1.3）。
- **注意 1: espflash に同梱されている P4 用ブートローダはロールバックが無効**（IDF 既定の `default n` のまま。§3.2）。ロールバックを使うには、自分でビルドした bootloader.bin を `espflash.toml` の `[idf] bootloader = ...` で指定する。同じ作者の既存 Tab5 プロジェクトは、まさにその構成（ロールバック有効、rev<3 向け、IDF 6.0）でビルドされている（`~/repos/tab5_claude_client/sdkconfig.defaults:11`, `sdkconfig:1085, 2040-2055`）。
- **注意 2（最重要）: esp-hal は P4 rev v3.0 未満を非サポート**（[soc.toml L15](https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-metadata/devices/esp32p4/soc.toml#L15)）。既存 Tab5 プロジェクトは rev 1.00–1.99 向けにビルドされている。実機の revision 確認が全体の前提になる（文書 04 §0 を参照）。
- 自前ブートローダ（MCUboot や embassy-boot 流の swap）は、**ESP32-P4 の公式 MCUboot ポート表に P4 が載っていない**ことと、embassy-boot に ESP 用 HAL クレートがないことから、v1 では採らない（§4）。

### アプリ単体の更新は「カーネル管理のアプリストア＋署名付き T5A＋アプリごとの A/B」
- フラッシュ上に**専用のカスタムデータパーティション**（type 0x40–0xFE）を 2 つ置く。`appmeta`（A/B の 2 セクタにメタデータ。otadata と同じ発想）と `apps`（64KB 境界のエクステントにアプリイメージを置く）。
- アプリイメージ（T5A。文書 04 を参照）は **Ed25519 で署名**し、公開鍵はカーネルに埋め込む（ルート鍵 → 署名鍵の 2 段も可）。検証は純 Rust のソフトウェア実装を第一候補とする。P4 の SHA アクセラレータ（SHA-512 対応）で高速化する余地がある（§5.3）。
- 状態遷移は ESP-IDF の otadata をまねて `New → PendingVerify → Valid / Invalid` とする。新版が `PendingVerify` のまま規定回数異常終了したら、旧版へ自動で戻す。**アプリごとの単調増加の `secure_version`** を appmeta に保存し、ダウングレードを防ぐ。
- 転送経路（Wi-Fi 経由の HTTPS、USB など）は、既存 Tab5 プロジェクトの OTA 設計（GitHub Pages の `latest.json`＋`.bin`、`~/repos/tab5_claude_client/docs/OTA_UPDATE_DESIGN.md` §1.7, §2）をカーネル更新とアプリ更新の両方に流用できる（推測）。

### 推奨パーティション配置（16MB、案）
| 名前 | type/subtype | オフセット | サイズ | 用途 |
|---|---|---|---|---|
| (bootloader) | — | 0x2000 | 〜0x6000 | P4 の 2nd stage は 0x2000 から（§2.1） |
| (partition table) | — | 0x8000 | 0x1000 | `CONFIG_PARTITION_TABLE_OFFSET=0x8000`（既存プロジェクトと同じ） |
| nvs | data/nvs | 0x9000 | 0x4000 | カーネル設定（Rust 側は互換形式でなくてもよい） |
| otadata | data/ota | 0xD000 | 0x2000 | ESP-IDF の bootloader が読む |
| ota_0 | app/ota_0 | 0x10000 | 0x200000 (2MB) | カーネル A |
| ota_1 | app/ota_1 | 0x210000 | 0x200000 (2MB) | カーネル B |
| appmeta | 0x40/0x00 | 0x410000 | 0x10000 | アプリストアのメタデータ（A/B） |
| apps | 0x40/0x01 | 0x420000 | 0x800000 (8MB) | アプリイメージ |
| userdata | data/littlefs など | 0xC20000 | 0x3E0000 | アプリのデータ |

カーネルのサイズは未知なので、2MB で足りるかは要確認（推測）。app パーティションは 64KB 境界に置く必要がある（`~/esp-idf/6.0/docs/en/api-guides/partition-tables.rst:220-221`）。カスタムタイプ 0x40–0xFE の subtype は任意（同ファイル `:99-105, :202`）。

---

## 1. ESP-IDF 2nd stage bootloader の仕組み

### 1.1 起動の流れ
1. ROM ブートローダが flash の `CONFIG_BOOTLOADER_OFFSET_IN_FLASH` から 2nd stage を読み込む。P4 ではこの値が **0x2000**。先頭 2 セクタは鍵マネージャの AES-XTS（フラッシュ暗号化）用に予約されているため（`~/esp-idf/6.0/components/bootloader/Kconfig.projbuild:7-12`）。
2. 2nd stage はパーティションテーブル（`ESP_PARTITION_TABLE_OFFSET`、最大 0xC00 バイト。`bootloader_support/include/esp_flash_partitions.h:56-64`）と otadata を読み、起動するスロットを選ぶ。
3. 選んだ app image を検証し、セグメントを読み込む。IRAM/DRAM 向けはコピーし、IROM/DROM 向け（flash の仮想アドレス窓）は MMU でマップする（`bootloader_support/src/bootloader_utility.c:817-823, 864`）。そのあと entry_addr にジャンプする。

### 1.2 app image フォーマット
- **ヘッダ** `esp_image_header_t`（24 バイト、packed）。magic `0xE9`、segment_count、spi_mode/speed/size、entry_addr、wp_pin、spi_pin_drv[3]、chip_id、min_chip_rev（旧）、**min_chip_rev_full / max_chip_rev_full**（major*100+minor）、reserved[4]、**hash_appended**（`bootloader_support/include/esp_app_format.h:76-111`）。
- **セグメントヘッダ** `{load_addr, data_len}`。セグメントは最大 16 個（`esp_app_format.h:117-122`）。
- **チェックサム**: 初期値 `0xEF` に全セグメントのデータを XOR した 1 バイト（`esp_image_format.c:58, :156`, `process_checksum` `:998`）。
- **SHA-256**: `hash_appended=1` のとき、イメージ全体の SHA-256 をチェックサムの後ろに付ける。これは破損検出用で、Secure Boot の署名はさらにその後ろに付く（`esp_app_format.h:102-106`, `verify_simple_hash` `esp_image_format.c:1137`）。
- **app descriptor** `esp_app_desc_t`（DROM の先頭付近に置く）: magic `0xABCD5432`、**secure_version**、version[32]、project_name[32]、time、date、idf_ver、app_elf_sha256、min/max_efuse_blk_rev_full、**mmu_page_size**（log2）など（`esp_app_format/include/esp_app_desc.h:21-41`）。
- chip revision の範囲がヘッダに入るので、**rev 1.x の Tab5 では、Rust 側が出すイメージの min/max_chip_rev_full が 1.x を含んでいる必要がある**（推測。espflash がどう設定するかは要確認）。

### 1.3 otadata と A/B 選択
- otadata は 2 セクタあり、各セクタの先頭に `esp_ota_select_entry_t { ota_seq:u32, seq_label[20], ota_state:u32, crc:u32 }`（32 バイト。crc は ota_seq だけの CRC32）を置く（`esp_flash_partitions.h:78-84`）。
- 有効なエントリのうち新しいほうの `ota_seq` を使い、`boot_index = (ota_seq - 1) % app_count` で起動スロットを決める（`bootloader_utility.c:440-444`）。
- **状態**: `NEW=0`、`PENDING_VERIFY=1`、`VALID=2`、`INVALID=3`、`ABORTED=4`、`UNDEFINED=0xFFFFFFFF`（`esp_flash_partitions.h:66-75`）。
- **ロールバック**（`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`。**既定は n**、`bootloader/Kconfig.app_rollback:3-5`）:
  - 起動時に `PENDING_VERIFY` のエントリがあれば `ABORTED` に書き換える。前回の起動で確定されなかったことになる（`bootloader_utility.c:395-404`）。
  - 選ばれたエントリが `NEW` なら `PENDING_VERIFY` にしてから起動する（`bootloader_utility.c:445-450`）。
  - アプリ側は起動後に `esp_ota_mark_app_valid_cancel_rollback()`（`app_update/esp_ota_ops.c:1020`）で確定するか、`esp_ota_mark_app_invalid_rollback_and_reboot()`（`:1030`）で戻す。
- **アンチロールバック**（`CONFIG_BOOTLOADER_APP_ANTI_ROLLBACK`）: eFuse の secure_version と app desc の secure_version を比べ、`VALID` になった時点で eFuse を更新する（`bootloader_utility.c:425-437, :452-456`）。eFuse は書き戻せないので、開発中は使わない。

### 1.4 検証のスキップ
- `BOOTLOADER_SKIP_VALIDATE_IN_DEEP_SLEEP` / `_ON_POWER_ON` / `_ALWAYS` があり、起動時間と引き換えに検証を省ける（`bootloader/Kconfig.projbuild:324-369`）。Secure Boot と同時には使えない（`docs/en/api-guides/bootloader.rst:172`）。

---

## 2. ESP32-P4 固有の注意

### 2.1 ブートローダ配置と chip revision
- bootloader は 0x2000（§1.1）。esptool / espflash も P4 では 0x2000 に書く。
- **v3.0 以上と v3.0 未満は互換性がない**: `CONFIG_ESP32P4_SELECTS_REV_LESS_V3` のヘルプに「Revisions higher than 3.0 (included) is not compatible with 0.x and 1.x」とある（`esp_hw_support/port/esp32p4/Kconfig.hw_support:4-11`）。espflash もブートローダを `esp32p4-v0`（`SELECTS_REV_LESS_V3`, `REV_MIN_100`）と `esp32p4-v3`（`REV_MIN_300`）の 2 種類同梱し、接続したチップの revision で選ぶ（[espflash resources/README.md](https://github.com/esp-rs/espflash/blob/main/espflash/resources/README.md)、[manifest.yaml](https://github.com/esp-rs/espflash/blob/main/espflash/resources/bootloaders/manifest.yaml)）。
- **esp-hal は v3.0 未満を非サポート**（§0）。Zephyr も rev 1.3 では PMP を無効にしている（文書 04 §4.3.7）。

### 2.2 Secure Boot v2
- P4 の SoC caps: `SOC_SECURE_BOOT_V2_RSA=1`、`SOC_SECURE_BOOT_V2_ECC=1`、eFuse の公開鍵ダイジェストは 3 本、鍵失効に対応（`soc/esp32p4/include/soc/soc_caps.h:656-660`）。
- 公式ドキュメント（latest, P4）: 方式は RSA-PSS 3072、ECDSA P-256、ECDSA P-384。**「ECDSA 方式の Secure Boot V2 は特定の入力ベクタで正しく動かない」ので RSA の使用を推奨**、ハードウェア ECO で修正予定とある。署名ブロックは 1216 バイトで、4KB 境界の専用セクタに置く。最大 3 鍵、保守的／積極的の 2 種類の失効モードがある（[Secure Boot v2 (ESP32-P4)](https://docs.espressif.com/projects/esp-idf/en/latest/esp32p4/security/secure-boot-v2.html)）。
- ESP-IDF 6.0 のローカル Kconfig には、latest ドキュメントにある ECDSA 強制有効化オプション（`SECURE_BOOT_V2_FORCE_ENABLE_ECDSA`）が見当たらない。版によって扱いが違う（`bootloader/Kconfig.projbuild:470-520`）。
- **Secure Boot を使わない署名付きアプリ**（`SECURE_SIGNED_APPS_NO_SECURE_BOOT`）: P4 では RSA か ECDSA(V2) の方式になり、検証は「更新時」（`SECURE_SIGNED_ON_UPDATE_NO_SECURE_BOOT`、既定 y）に行われる。「起動時に bootloader が検証する」`SECURE_SIGNED_ON_BOOT_NO_SECURE_BOOT` は SBv1 の ECDSA 方式にしか依存できないので、P4 では使えない（`bootloader/Kconfig.projbuild:504-560, :587-602`）。なお「更新時の検証」は IDF の `esp_ota_end` 側の機能なので（推測）、Rust カーネルでは自前で実装する必要がある。
- eFuse への書き込みは**取り消せない**。開発用の Tab5 では最後の段階まで有効にしない（推測による運用判断）。

### 2.3 フラッシュ暗号化
- XTS-AES-128/256 に対応し、鍵マネージャによる鍵配布もできる（`soc_caps.h:663-667, :647-653`）。
- 暗号化した flash を MMU 経由で読むと透過的に復号される。生読みでは復号されない。esp-bootloader-esp-idf には暗号化領域用の `as_encrypted_flash_region` / `EncryptedFlashRegion` がある（[region.rs L35, L245](https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-bootloader-esp-idf/src/partitions/region.rs#L35)）。
- 暗号化すると apps パーティションの書き込みも暗号化書き込みにする必要がある（推測）。v1 では無効にしておく。

### 2.4 ブートローダ自体の更新
- ESP-IDF 6.0 の soc_caps で `SOC_RECOVERY_BOOTLOADER_SUPPORTED` / `SOC_BOOTLOADER_ANTI_ROLLBACK_SUPPORTED` が定義されているのは C5 と C61 だけで、P4 にはない（`soc/esp32c5/include/soc/soc_caps.h:550-552`、`soc/esp32c61/.../soc_caps.h:401-403`）。→ **P4 ではブートローダを電源断に安全な方法で現場更新する手段がない**（推測）。ブートローダは書いたら固定のものとして扱い、変えるのはパーティションテーブル以降に限る。
- パーティションテーブルの書き換えも電源断に弱い（1 セクタ）。最初に余裕のあるレイアウトを決めておく（推測）。

### 2.5 暗号アクセラレータ（ソフトウェア検証の高速化候補）
- P4 の SoC caps: SHA（SHA-1/224/256/384/512/512_t、DMA、レジューム）、RSA（最大 4096 ビット）、ECC（定数時間の点乗算、P-384）、HMAC、DS、AES がある（`soc_caps.h:71-79, :486, :512-533`）。**`SOC_ECDSA_SUPPORTED 0  // TODO: IDF-13523`**（`soc_caps.h:78`）。
- esp-hal（P4）: SHA は partial、RSA は partial、ECC は partial、AES は partial、**ECDSA は not_supported**（[soc.toml L300-301, L540-584](https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-metadata/devices/esp32p4/soc.toml#L300-L584)）。

---

## 3. Rust 側: esp-hal を ESP-IDF bootloader に載せる

### 3.1 esp-bootloader-esp-idf クレート（v0.6.0）
- 役割: app descriptor の埋め込み、パーティションテーブルの読み出し、パーティション単位の flash 読み書き、OTA（[lib.rs L1-80](https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-bootloader-esp-idf/src/lib.rs#L1-L80)）。`esp32p4` feature がある（[Cargo.toml L98](https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-bootloader-esp-idf/Cargo.toml#L98)）。
- `esp_app_desc!()` マクロで、version、project_name、build time/date、idf_ver、**MMU ページサイズ**、eFuse ブロックの revision 範囲、**secure_version** を指定できる（[lib.rs L377-410](https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-bootloader-esp-idf/src/lib.rs#L377-L410)）。
- `OtaUpdater::new(flash, buf)` はパーティションテーブルを読み、otadata と OTA スロット 2 つ以上を見つける。`next_partition()` で書き込み先を得て、`activate_next_partition()` と `set_current_ota_state(New)` で切り替える。起動後に `set_current_ota_state(Valid)` で確定する（[ota_updater.rs L13-161](https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-bootloader-esp-idf/src/ota_updater.rs#L13-L161)、状態 enum は [ota.rs L72-95](https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-bootloader-esp-idf/src/ota.rs#L72-L95)）。
- 公式の OTA サンプル（factory＋ota_0/ota_1）: `Valid` の設定（L96）、`activate_next_partition`＋`New`（L145-146）。runner は `espflash flash --monitor --partition-table=.../partitions.csv`（[examples/ota/update](https://github.com/esp-rs/esp-hal/tree/5c0820cf9e2be0a49719bd71c30e7a45cae48134/examples/ota/update)）。

### 3.2 espflash
- 同梱ブートローダは ESP-IDF の `release/v6.1` を最小構成でビルドしたもの。共通の設定は `# CONFIG_BOOTLOADER_COMPILE_TIME_DATE is not set` だけで、P4 用はほかに revision 選択の 2 項目しか持たない。**ロールバックは有効になっていない**（[manifest.yaml](https://github.com/esp-rs/espflash/blob/main/espflash/resources/bootloaders/manifest.yaml) の defaults と esp32p4-v0/v3 の項目）。
- 「esp-hal は一部チップで MMU ページサイズを 0x10000 としているので、該当するブートローダは flash サイズ 64MB 設定でビルドしている」との注記がある（[resources/README.md](https://github.com/esp-rs/espflash/blob/main/espflash/resources/README.md)）。
- 独自のブートローダやパーティションテーブルは `espflash.toml` の `[idf] bootloader = ...` / `partition_table = ...` で指定する（[espflash README](https://github.com/esp-rs/espflash/blob/main/espflash/README.md)）。`save-image` でアプリイメージだけを書き出せる（同 README）。この出力を OTA 用の配布物にする。

### 3.3 既存の Rust 製 OTA クレート
- `esp-hal-ota`: no_std の OTA ライブラリ。作者は esp32/s3/c3 で動作確認済みとしている。docs.rs では 0.6.0 のビルドに失敗している（[docs.rs](https://docs.rs/esp-hal-ota)）。P4 対応は不明。
- `esp-ota`（faern）: 転送路に依存しない OTA。主に esp-idf-svc（std）向け（[GitHub](https://github.com/faern/esp-ota)）。
- → v3.x なら公式の `esp-bootloader-esp-idf` を使う。**v1.x（Tab5）ではリンクできないので、同クレートの `esp_app_desc` / `ota` / `ota_updater` の形式を参照して自前実装する**（統合時注記。文書 03 §1.6、07 §5）。

### 3.4 移行経路（推測）
- 既存の C++ ファームウェア（tab5_claude_client）は、IDF 6.0 製でロールバックが有効、rev<3 向けのブートローダと、`ota_0/ota_1` が各 3MB のパーティションテーブルで動いている（`~/repos/tab5_claude_client/partitions.csv`）。Rust カーネルを**同じパーティションテーブルのまま ota_1 に OTA で入れれば**、シリアル書き込みなしで移行でき、問題があればロールバックで戻れる見込み。
  - その場合 apps はテーブルにない `0x710000` 以降の未使用領域（約 9MB。`OTA_UPDATE_DESIGN.md` 前提節）にカーネルが直接置く形になり、少し筋が悪い。
  - 本番では §0 の新しいレイアウトにシリアルで書き直すのが素直。
  - **統合時注記: この移行経路は M/U 分離の観点では使えない。** 既存ファームのブートローダは `CONFIG_BOOTLOADER_REGION_PROTECTION_ENABLE=y`（`~/repos/tab5_claude_client/sdkconfig:1162`）でビルドされている。v1.x ではこの設定で PMP エントリ 0（0x2000_0000–0x3000_0000、CLIC/CLINT など CPU サブシステム, RW）、1（CPU 周辺, RW）、2（ROM, RX）、15（0x5000_0000–0x5010_0000, RW）がロック付きで設定され（`~/esp-idf/6.0/components/esp_hw_support/port/esp32p4/cpu_region_protect.c:21-35, 235-256, 355-357`）、ロック済みエントリは U モードにも同じ権限を与える。番号の小さいエントリが優先されるため 0–1 は後から塞げない。したがって **ブートローダ自体をシリアルで書き換える必要がある**（文書 01 §3.4、07 §2）。

---

## 4. 自前ブートローダ案との比較

| 観点 | ESP-IDF 2nd stage を流用（推奨） | MCUboot（Espressif ポート） | embassy-boot（swap 方式） | 完全自作（Rust） |
|---|---|---|---|---|
| P4 対応 | IDF 6.0 で対応。espflash も同梱 | 公式ドキュメントのポート表に P4 なし。NuttX の P4 ドキュメントは MCUboot 構成に言及（[docs.mcuboot.com readme-espressif](https://docs.mcuboot.com/readme-espressif)、[NuttX ESP32-P4](https://nuttx.apache.org/docs/latest/platforms/risc-v/esp32p4/index.html)） | ESP 用の HAL クレートなし（nrf/rp/stm32 のみ）（[README](https://github.com/embassy-rs/embassy/blob/c2413d5994bbd8b7e6d5975fd49e6427cb903a02/embassy-boot/README.md)） | 自分で書く |
| A/B の方式 | 2 スロット＋otadata。コピーしない | swap / overwrite / direct-xip（MCUboot 一般の話、未確認） | ACTIVE と DFU をページ単位で swap。電源断に安全（同 README） | 任意 |
| ロールバック | PENDING_VERIFY → ABORTED | あり（一般論） | `mark_booted` がなければ Revert（[firmware_updater/blocking.rs L212](https://github.com/embassy-rs/embassy/blob/c2413d5994bbd8b7e6d5975fd49e6427cb903a02/embassy-boot/src/firmware_updater/blocking.rs#L212)） | 任意 |
| 署名 | SBv2（RSA-3072 推奨。eFuse 依存） | ECDSA/RSA/ed25519（一般論） | `verify_and_mark_updated`（Digest＋VerifyingKey。ed25519 など）（[blocking.rs L129](https://github.com/embassy-rs/embassy/blob/c2413d5994bbd8b7e6d5975fd49e6427cb903a02/embassy-boot/src/firmware_updater/blocking.rs#L129)、[Cargo.toml L40](https://github.com/embassy-rs/embassy/blob/c2413d5994bbd8b7e6d5975fd49e6427cb903a02/embassy-boot/Cargo.toml#L40)） | 任意 |
| ROM との関係 | ROM はこの形式を直接起動できる | ESP image 形式の 2nd stage として起動 | 2nd stage として ESP image 形式に変換が必要（推測） | 同左＋clock/flash/MMU/PSRAM の初期化を全部自前で |
| 工数とリスク | 最小 | 中（P4 対応の確認が要る） | 大（ESP 移植） | 最大 |

**判断**: v1 は ESP-IDF のブートローダを流用する。カーネル更新の署名検証は、Secure Boot を使わずに**カーネル自身が新イメージを検証してから otadata を切り替える**形にし（§5.4）、Secure Boot（RSA）は量産相当の段階で再検討する。

---

## 5. アプリ単体の更新

### 5.1 ストレージ方式の比較
| 方式 | 長所 | 短所 | 評価 |
|---|---|---|---|
| A. 専用パーティション＋自前の簡易ストア（メタ A/B、64KB エクステント） | 書き込みの原子性を設計できる。64KB 境界なので将来 MMU で XIP もできる | 自前実装が要る | ◎ 推奨 |
| B. littlefs（`littlefs2` クレートなど）にファイルとして置く | 実装が楽。電源断に強い（littlefs の特性。未確認） | C ライブラリへの依存（推測）。断片化した配置なので XIP は不可 | ○（userdata 用） |
| C. FAT | PC と互換性がある | 電源断に弱い | × |
| D. Tock 流の連続配置（TBF を flash に順番に並べる） | 単純（[trd-dynamic-process-loading](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd-dynamic-process-loading.md)） | 削除や更新のたびに詰め直す必要がある | △ |

### 5.2 アプリストア（方式 A）の設計案（推測）
- **appmeta** は 2 セクタ（A/B）。各セクタに `{magic, seq, crc32, entries[N]}` を置き、`seq` の大きいほうを有効とする（otadata と同じ考え方。`esp_flash_partitions.h:78-84`）。
- **entry** は `{app_id(u128 か名前ハッシュ), slot[2]{extent_off, len, version, secure_version, sha256, state}, active_slot, boot_fail_count}`。
- **インストール手順**:
  1. 空きエクステントを消去し、受信しながら書き込む。
  2. 全体の SHA-256 と署名を検証する。
  3. 新しい appmeta を非アクティブ側のセクタに書く（`state = New`、seq+1）。
  4. 書き終えたら完了。電源断の時点に応じて、旧 appmeta か新 appmeta のどちらかが有効になる。
- **起動時**: `New` を起動したら `PendingVerify` にする。アプリが `sys_app_confirm()` を呼ぶか、カーネルの健全性条件（N 秒以上動作し、異常終了していない）を満たせば `Valid` にする。`PendingVerify` のまま異常終了が続いたら `Invalid` にして旧スロットへ戻す（IDF の状態遷移をまねる。`bootloader_utility.c:395-450`）。
- **ダウングレード防止**: アプリごとに「受け入れた secure_version の最大値」を appmeta（または NVS）に持つ。eFuse は使わないので、物理攻撃には耐えない。

### 5.3 署名とハードウェア支援
- **方式の選択**:
  - Ed25519 を第一候補にする（公開鍵 32B、署名 64B、決定的）。Tock の TBF クレデンシャルは RSA-3072/4096、SHA-256/384/512、ECDSA P-256 を定義している（[types.rs L254-262](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/libraries/tock-tbf/src/types.rs#L254-L262)）。embassy-boot は ed25519 で検証している（§4）。
  - ECDSA P-256 は、P4 の ECDSA ペリフェラルが IDF 6.0 / esp-hal とも未対応（§2.5）なので、使うなら ECC ペリフェラル（点乗算）かソフトウェア実装（`p256` クレート）になる（推測）。
- **ハッシュ**: 本体は SHA-256（RFC 8032 の Ed25519 は内部で SHA-512 を使う）。どちらも P4 の SHA アクセラレータ（DMA 対応）で高速化できる余地がある（`soc_caps.h:512-529`）。純 Rust 実装と性能を比べるのは未確認事項。
- **鍵の階層（推測）**: カーネルにルート公開鍵を埋め込む。T5A のフッタには「署名鍵の公開鍵＋ルートによる証明」と「アプリ署名」を入れる。開発用の鍵と本番用の鍵を分け、開発用の鍵は設定で無効化できるようにする。

### 5.4 カーネル更新の署名（ESP-IDF の Secure Boot を使わない場合）
- 配布物は「espflash `save-image` で作った ESP app image」＋「その SHA-256 に対する Ed25519 署名などを含む分離マニフェスト」（推測）。
- 稼働中のカーネルが次の順で処理する。
  1. 非アクティブなスロットに書き込む。
  2. ESP image としての整合性（magic、chip revision の範囲、SHA-256 の付加）と、マニフェストの署名を検証する。
  3. `activate_next_partition()` と `New` を書く。
  4. 再起動する。
  5. 新カーネルが健全性チェックのあとで `Valid` にする。
- 弱点: bootloader 自体は署名を見ないので、**物理的に書き換えられれば無効**（`SECURE_SIGNED_APPS_NO_SECURE_BOOT` のヘルプが述べる弱点と同じ。`bootloader/Kconfig.projbuild:504-513`）。

### 5.5 アプリとカーネルの互換性
- T5A の `AbiVersion` TLV（文書 04）を、インストール時と起動時に照合する。カーネルを更新して ABI の major が上がった場合、互換性のないアプリは起動せず、ストアで「要更新」にする。
- カーネルを更新するときは「新カーネルが必要とするアプリの最低 ABI」をマニフェストに入れる。アプリの更新を先に配って、後からカーネルを更新する順序でもよい（推測）。

---

## 6. 比較表（まとめ）

| 更新対象 | 方式 | 署名 | ロールバック | 備考 |
|---|---|---|---|---|
| bootloader | 更新しない（P4 には recovery bootloader がない） | — | — | 最初に書いたまま固定 |
| partition table | 原則更新しない | — | — | 最初に余裕をもって設計 |
| カーネル | ESP-IDF の A/B（ota_0/ota_1＋otadata） | v1: カーネルが Ed25519 マニフェストを検証。将来: SBv2 RSA-3072 | bootloader の PENDING_VERIFY → ABORTED | 自分でビルドしたロールバック有効のブートローダが必須 |
| アプリ | 自前のアプリストア（appmeta A/B＋apps エクステント） | Ed25519（T5A のクレデンシャルフッタ） | アプリごとの New/Pending/Valid/Invalid と旧スロット保持 | secure_version でダウングレード防止 |
| アプリのデータ | littlefs など | — | — | アプリのアンインストール時に消す |

---

## 7. 未確認事項

1. **Tab5 実機の ESP32-P4 chip revision**（v1.x か v3.x か）。esp-hal の対応可否、使うブートローダ、Zephyr での PMP の可否がすべてこれで決まる。
2. espflash や esp-hal のビルドが出す app image の `min/max_chip_rev_full` が v1.x を受け入れるか。esp-hal が v1.x で起動するかどうか自体も未確認。
3. 既存の tab5_claude_client 用ブートローダ（IDF 6.0、ロールバック有効、rev<3）が、Rust 製のイメージ（MMU ページ 64KB、esp-hal のリンカスクリプト）をそのまま起動できるか。
4. Rust カーネルのサイズ（OTA スロット 2MB で足りるか）。
5. P4 の SHA アクセラレータを使った Ed25519 / SHA-256 検証の速度と、純ソフトウェア実装との差。
6. MCUboot の P4 対応状況（NuttX 経由の実体、上流 mcuboot リポジトリの esp32p4 対応）。
7. フラッシュ暗号化を有効にしたときの apps パーティションの扱い（カーネルによる暗号化書き込み、PSRAM へのロード時の復号）。
8. ESP-IDF の ECDSA Secure Boot の不具合が影響するチップ revision の範囲（ドキュメントは「将来の ECO で修正」としか書いていない）。

---

## 8. 参考文献

- ESP-IDF 6.0（ローカル）:
  - `~/esp-idf/6.0/components/bootloader_support/include/esp_app_format.h`
  - `esp_flash_partitions.h`
  - `bootloader_support/src/bootloader_utility.c`
  - `bootloader_support/src/esp_image_format.c`
  - `esp_app_format/include/esp_app_desc.h`
  - `app_update/esp_ota_ops.c`, `app_update/include/esp_ota_ops.h`
  - `bootloader/Kconfig.projbuild`, `bootloader/Kconfig.app_rollback`
  - `soc/esp32p4/include/soc/soc_caps.h`
  - `esp_hw_support/port/esp32p4/Kconfig.hw_support`
  - `docs/en/api-guides/partition-tables.rst`, `docs/en/api-guides/bootloader.rst`
- Secure Boot v2 (ESP32-P4): https://docs.espressif.com/projects/esp-idf/en/latest/esp32p4/security/secure-boot-v2.html
- esp-bootloader-esp-idf: https://github.com/esp-rs/esp-hal/tree/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-bootloader-esp-idf
- esp-hal OTA example: https://github.com/esp-rs/esp-hal/tree/5c0820cf9e2be0a49719bd71c30e7a45cae48134/examples/ota/update
- esp-hal ESP32-P4 metadata: https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-metadata/devices/esp32p4/soc.toml
- espflash（resources/README, bootloaders/manifest.yaml, README）: https://github.com/esp-rs/espflash
- esp-hal-ota: https://docs.rs/esp-hal-ota ／ esp-ota: https://github.com/faern/esp-ota
- embassy-boot: https://github.com/embassy-rs/embassy/tree/c2413d5994bbd8b7e6d5975fd49e6427cb903a02/embassy-boot
- MCUboot Espressif port: https://docs.mcuboot.com/readme-espressif ／ Espressif blog（2026-01）: https://developer.espressif.com/blog/2026/01/mcuboot-getting-started/
- NuttX ESP32-P4: https://nuttx.apache.org/docs/latest/platforms/risc-v/esp32p4/index.html
- Tock TBF credentials / dynamic process loading: https://github.com/tock/tock/tree/d4979a4c09b6253fc0c44ddb202096ddcb15f445
- 既存 Tab5 プロジェクト: `~/repos/tab5_claude_client/docs/OTA_UPDATE_DESIGN.md`, `partitions.csv`, `sdkconfig`, `sdkconfig.defaults`
