# 07. アーキテクチャ案と最初のマイルストーン

01〜06 の調査結果を統合した提案。各項目の根拠は括弧内の章・節にある（例: 「01 §3.4」は `01-cpu-privilege-and-protection.md` の 3.4 節）。設計判断のうち、資料ではなく判断・推測に基づくものは「（判断）」「（推測）」と明記する。

- 版: 初版（2026-10-29）
- 状態: **提案。実装前に方針確認を受ける**

---

## 0. 前提として確定した事実

| 項目 | 内容 | 根拠 |
|---|---|---|
| チップ | 手元の Tab5 は **ESP32-P4 rev v1.0**（ROM `esp32p4-eco2-20240710`） | `~/repos/tab5_claude_client/logs/boot.log:2,15`、03 §0、06 §0.1 |
| 特権モード | HP コアは **M + U のみ**（S なし、satp/MMU なし）。LP コアは M のみで PMP もない | 01 §0、TRM §2.5.2 / §5.3 |
| PMP | v1.x は **16 エントリ**、粒度 128B、TOR/NAPOT（NA4 なし） | 01 §3.2、IDF `cpu_region_protect.c:235-357` |
| 割り込み | CLIC のみ。v1.x は非標準 CLIC（mintthresh CSR がなく、閾値は MMIO 0x2080_0008） | 01 §0 / §4.1、04 §0 |
| 外部メモリ MMU | 64KB ページ。フラッシュ用・PSRAM 用に各 1024 エントリ（各 64MB 窓）。**全コア共通の 1 枚で権限ビットなし** | 02 §3 |
| esp-hal | **P4 は rev v3.0 以上のみ対応**。v1.x は対象外で、v1.x 向けの貢献も受け付けない方針 | 03 §1.1、esp-hal `soc.toml:15`、#3962 |
| 既存ブートローダ | 現在の Tab5 のブートローダは `REGION_PROTECTION_ENABLE=y`。v1.x では PMP 0/1/2/15 を**ロック付き**で設定し、U モードにも RW を与えてしまう | 05 §3.4 注記、IDF `cpu_region_protect.c:21-35,235-256,355-357` |

最後の項目が重要。PMP は番号の小さいエントリから先に照合し、ロック済みエントリは U モードにも同じ権限を与える。そのためエントリ 0（0x2000_0000〜0x3000_0000、CLIC/CLINT やデバッグ領域）と 1（CPU 周辺）を U モードに対して閉じる手段がない。**M/U 分離のためには、ブートローダを `CONFIG_BOOTLOADER_REGION_PROTECTION_ENABLE=n` で作り直して書き込む必要がある。**

## 1. 全体方針（要約）

1. **土台は自作の M モードカーネル。** `riscv` クレートと、自前の起動コード・トラップ入口（asm）で組む。esp-hal、Tock、ESP-IDF の上には作らない（03 §0）。
   - レジスタ定義は ESP-IDF の `soc/esp32p4/register/hw_ver1` ヘッダから、カーネルが触る周辺の分だけ手書きする。
   - ロジックは ESP-IDF の `*_ll.h`、esp-hal、NuttX の Tab5 対応を参照する（03 §1.6、06 §0）。
2. **ブートローダは ESP-IDF 6.0 の 2nd stage bootloader を設定を変えて流用する。** OTA の A/B、otadata、ロールバックをそのまま使う（05 §0）。
3. **アプリは U モード、保護は PMP。** カーネルは PMP エントリを使わない。U モードはどのエントリにも一致しなければ拒否され、M モードは一致しなければ許可されるため（01 §0）。
   - カーネル自身の W^X は PMA で作る。PMA は全モードに効くが、U モードはもともと PMP で拒否されているので問題はない（01 §8-3、判断）。
4. **アプリは静的 PIE（`R_RISCV_RELATIVE` だけ）。** ホストツールで独自コンテナ T5A に変換し、カーネルがコピーと再配置を行ってから実行する（04 §0）。
5. **syscall は `ecall`。a7 = 番号、a0–a5 = 引数、a0 = 状態、a1–a3 = 値**（04 §4.2）。
   - 次の 3 つの設計を借りる。
     - Zephyr: 検証関数と実装関数の分離、生成ツール、事前検査してからのコピー
     - Tock: Permissions TLV、ABI の照合
     - Xous: ハンドルとメッセージ
   - v0 の syscall は割り込み禁止で短く実行する。これで v1.x に mintthresh がない問題を回避する（判断。§4.3）。
6. **ドライバはカーネル内に置く（Tock 型）。** アプリとサービスの間は、ハンドル＋チャネル（Xous 風）でつなぐ（03 §0、判断）。
   - ユーザ空間ドライバは PMP の窓と APM の UM ビットで将来の選択肢として残す（01 §3.6）。
7. **最初はシングルコア。** Core1 は起動しない。LP コアは使わない。
8. **エミュレータを最初から使う（08）。** ESP32-P4 を模すエミュレータは存在しない（Espressif QEMU も P4 非対応）。そこで次の 3 層で検証する。
   - 純ロジック: Linux 上の `cargo test`（Hosted）
   - アーキ層: 上流 QEMU `-M virt`（トラップ、PMP、U モード、ecall、ローダ）
   - SoC 層: 実機。補助として Wokwi の P4 v1.0（beta）

   そのため、カーネルの SoC 依存部（Console / Timer / IrqCtrl / Mpu）を trait で切り替える（08 §3.3）。

## 2. ブート → カーネル → アプリ起動の流れ

```
[ROM]  USB Serial/JTAG にログを出し、0x2000 の 2nd stage bootloader をロード
  │
[ESP-IDF 6.0 bootloader（本プロジェクト用に作り直したもの）]
  │   設定: ESP32P4_SELECTS_REV_LESS_V3=y, BOOTLOADER_APP_ROLLBACK_ENABLE=y,
  │         BOOTLOADER_REGION_PROTECTION_ENABLE=n
  │   - パーティションテーブル(0x8000)と otadata を読み、ota_0/ota_1 を選ぶ
  │   - カーネル（ESP app image）を検証し、L2MEM へセグメントをコピー、
  │     フラッシュ上の text/rodata を MMU で 0x4000_0000〜 にマップ
  │   - RWDT を 9s で有効にしたまま app に渡す（06 §2）
  ▼
[カーネル _start（Core0, M モード）]
  1. sp/gp、.bss クリア、.data、mtvec（CLIC モード、トラップ入口は 64B 境界）
  2. RWDT（LP_WDT）と TIMG0/1 の MWDT を停止            … 06 §2
  3. USB Serial/JTAG コンソール（FIFO 直書き、タイムアウト付き） … 06 §3.1
  4. 自己診断ダンプ: misa/mstatus、pmpcfg0-3/pmpaddr0-15、PMA、eFuse の chip rev
     → ロック済み PMP があれば panic（分離が成立しないため）
  5. PMA でカーネル text を RX、カーネル data を RW（W^X）
  6. SYSTIMER と CLIC（Interrupt Matrix）を初期化
  7. （M4 以降）クロック 360MHz 化、PSRAM 初期化、PSRAM を線形マップ
  8. アプリのロード: 取得 → 署名検証 → コピー → 再配置 → キャッシュ同期
  9. プロセスの PMP 設定 → mepc=entry, MPP=U, sp=ユーザスタック → mret
  ▼
[アプリ（U モード）] ecall → カーネルのトラップ入口 → syscall → mret
```

### トラップ入口（要点）
- `csrrw sp, mscratch, sp` で、U モードのスタックを一切信用せずにカーネル側のトラップフレームへ切り替える。カーネル実行中は mscratch=0 として、M→M のトラップ（割り込み）と区別する（01 §8-5、03 §3.4）。
- **トラップハンドラ内（mret 前）でさらに例外が起きると CPU が lock-up する**（01 §0、TRM §2.13.5）。入口から GPR の退避が終わるまでの間に、フォールトしうるアクセスをしないこと。
- 保存するのは全 GPR、mepc、mstatus。F レジスタは mstatus.FS による遅延保存とする。ただし v1.x には FPU の EXT_ILL のバグがあり、判定に注意が要る（01 §5.3）。
- ESP 独自拡張（HWLP/PIE）はオフにし、U モードからは使わせない（01 §5.3、03 §2.2）。

## 3. メモリマップ（v1.x、L2 キャッシュ 128KB の場合）

| 範囲 | 大きさ | 用途 | U モード |
|---|---|---|---|
| 0x2000_0000–0x3000_0000 | – | CPU サブシステム（CLIC/CLINT、デバッグ） | 拒否 |
| 0x3010_0000– | 8KB | HP SPM（TCM）: カーネルのトラップスタックなど（判断） | 拒否 |
| 0x3FF0_0000– | – | CPU 周辺（キャッシュコントローラなど） | 拒否 |
| 0x4000_0000– | 〜16MB | フラッシュ XIP 窓: カーネルの text/rodata（bootloader がマップ）、将来は読み取り専用アセット | 拒否（将来アセットの窓だけ R） |
| 0x4800_0000–0x49FF_FFFF | 32MB | PSRAM（キャッシュ経由）。起動時に 512 ページを線形マップ。先頭はカーネルのヒープとフレームバッファ、残りは**アプリプール** | 自プロセスの領域だけ |
| 0x4FC0_0000– | 128KB | HP ROM | 拒否 |
| 0x4FF0_0000–0x4FF2_CBD0 | 約 179KB | L2MEM 下側: カーネルの .data/.bss、ホットな text（トラップ、キャッシュ/MMU/フラッシュ操作）、カーネルスタック | 拒否 |
| 0x4FF2_CBD0–0x4FF4_0000 | 約 77KB | bootloader と ROM の予約域（ROM の .bss/.data を含む）。**触らない** | 拒否 |
| 0x4FF4_0000–0x4FFA_0000 | 384KB | L2MEM 上側: カーネルヒープ、DMA バッファ。**M1〜M3 では暫定のアプリ領域** | 自プロセスの領域だけ |
| 0x4FFA_0000–0x4FFB_FFFF | 128KB | L2 キャッシュ | – |
| 0x5000_0000– | – | HP 周辺 MMIO | 拒否 |
| 0x8xxx_xxxx | – | 上記のキャッシュを通らないエイリアス | 拒否（エントリを作らないので自動的に拒否） |

数値の出典は 02 §1 / §1.1（IDF `memory.ld.in:22-33,80-82`）。v3.x では L2MEM の配置が逆なので、リンカスクリプトをリビジョンごとに分ける（02 §0）。

### 02（固定仮想アドレス）と 04（PIE）の折衷（判断）
- PSRAM は起動時に**一度だけ**線形にマップし、それ以降は MMU を張り替えない。
- アプリは PIE で、プール内の任意の位置へ再配置する。

こうすると、複数のアプリが同時に常駐でき、コンテキストスイッチで MMU を張り替える必要もない（04 §0 の指摘）。PSRAM を使うのでフラッシュの OTA 中も止まらず、SD からも読み込める（02 §6 の利点）。MMU の張り替えは、フラッシュ上のアセットを XIP する用途だけに残す。

### プロセスの配置と PMP（v1.x、16 エントリ）
各プロセスのイメージは連続した 1 つの領域 `[base | text+rodata | data+bss | stack]` で、境界は PMP の粒度 128B に揃える（アプリのリンカスクリプト `user/rt/t5app.ld` が RW 部の先頭を 128B 境界にする）。TOR を 3 本つないで表す（M0 で実装。当初案の NAPOT 2 本だと、PIE の text と data の相対位置を保ったまま両方を 2 のべき乗に整列させられないため）。

| PMP # | pmpaddr | 用途 | 権限 |
|---|---|---|---|
| 0 | – | 予備。トラップベクタを U に X で見せる必要が出たとき用（Tock の ESP32-C3 移植で見つかった CPU バグの回避策。P4 での有無は未確認。03 §4.1） | OFF |
| 1 | base | TOR の下端 | OFF |
| 2 | base + text_size | text+rodata | TOR, R-X |
| 3 | end | data+bss+stack | TOR, RW- |
| 4 | – | 共有メモリ（`shm_map`、フレームバッファなど） | NAPOT, RW- |
| 5–6 | – | IPC で貸すバッファ（Xous の Lend/Borrow 相当） | TOR ペア, R または RW |
| 7–15 | – | 予約（ユーザ空間ドライバの MMIO 窓、追加の shm など） | – |

コンテキストスイッチでは 1–5 だけを書き換える（Zephyr と同じく、プロセスごとに事前計算した値を書き込む。04 §4.3.3）。

## 4. システムコール（v0 初版）

### 4.1 呼び出し規約（04 §4.2）

| レジスタ | 入力 | 出力 |
|---|---|---|
| a7 | 番号 | 破壊 |
| a0–a5 | 引数 | a0 = 状態（0 成功 / 負 = -Error）、a1–a3 = 値 |
| s0–s11, sp, gp, tp, F レジスタ | 保存される | |

- `mcause=8` のとき `mepc += 4` する（ecall は常に 4 バイト）。
- ユーザポインタは、オーバーフローを含めて「自プロセスの領域に完全に含まれるか」をソフトウェアで事前に検査し、カーネルのバッファへコピーしてから使う。
- 例外を拾って回復するフィックスアップ方式は使わない。ESP 系ではロードのアクセス例外が不正確とされるため（04 §4.2 / §4.3）。
- syscall 定義は 1 つの表（Rust の `abi` クレート）に集め、U 側のスタブと M 側のディスパッチ表を build.rs か proc-macro で生成する（Zephyr の `gen_syscalls.py` 相当）。

### 4.2 一覧（初版）

| 番号 | 名前 | 引数 → 戻り値 | 導入 |
|---|---|---|---|
| 0x000 | `exit` | code → (戻らない) | M1 |
| 0x001 | `yield` | – → – | M2 |
| 0x002 | `sleep` | ns_lo, ns_hi → – | M2 |
| 0x003 | `sys_info` | buf, len → 書いた長さ（ABI 版、カーネル版、対応 syscall のビットマップ） | M1 |
| 0x004 | `time` | – → ns_lo, ns_hi（単調時刻） | M2 |
| 0x040 | `brk` | addr → 新しい brk | M3 |
| 0x080 | `close` | h → – | M3 |
| 0x082 | `wait` | handles, n, timeout, ev_out → イベント数 | M3 |
| 0x0C0 | `chan_create` | – → h_tx, h_rx | M5 以降 |
| 0x0C1/0x0C2 | `chan_send` / `chan_recv` | h, buf, len → len | M5 以降 |
| 0x100 | `open` | name, len, rights → h（Permissions TLV で許可されたものだけ） | M3 |
| 0x101/0x102 | `read` / `write` | h, buf, len → len | M3 |
| 0x103 | `ctl` | h, op, a, b → 値 | M3 |
| 0x140 | `shm_map` | h → addr, len | M6 |
| 0xF00 | `dbg_write` | buf, len → len（USB Serial/JTAG へ出力） | **M1** |

番号は追加だけにし、振り直しはしない。ABI 版はアプリヘッダの TLV と `sys_info` の両方で照合する（04 §4.2）。

### 4.3 割り込みと syscall の関係（判断）
- v0 では、カーネル（syscall とトラップ処理）は割り込み禁止のまま実行する。トラップに入ると mstatus.MIE が 0 になるので、追加の操作は要らない。
- 長い処理は、プロセスをブロック状態にしてからスケジューラに戻す。
- Zephyr は P4 で、syscall 中に mintthresh で割り込みを止める方式を採っているが、v1.x には mintthresh CSR がない（04 §0）。本案では使わないので、この問題を避けられる。
- 将来カーネルをプリエンプティブにするときは、v1.x の MMIO 閾値（0x2080_0008）を HAL で吸収する（01 §8-7）。

## 5. 更新（05 の要約と本案での決定）

| 対象 | 方式 |
|---|---|
| ブートローダとパーティションテーブル | 最初の 1 回だけシリアルで書き込み、以後は固定する。P4 には recovery bootloader がないため（05 §2.4） |
| カーネル | ESP-IDF の OTA（ota_0/ota_1 + otadata + ロールバック）。`esp_app_desc_t` と otadata の形式は Rust で自前に写す。v1.x では `esp-bootloader-esp-idf` をリンクできないため（03 §1.6、05 §3.1 注記）。カーネルは自分自身に埋め込んだ鍵で Ed25519 の分離マニフェストを検証してから otadata を切り替える |
| アプリ | `appmeta`（A/B）と `apps`（64KB エクステント）のカスタムパーティションに、T5A（Ed25519 署名）を置く。状態は New → PendingVerify → Valid/Invalid。アプリごとに secure_version を持ち、ダウングレードを防ぐ（05 §5） |
| Secure Boot v2 / フラッシュ暗号化 | eFuse への書き込みは不可逆なので後回し（05 §2.2） |

パーティション案（05 §0）: nvs / otadata / ota_0 2MB / ota_1 2MB / appmeta 64KB / apps 8MB / userdata 約 3.9MB。

## 6. リポジトリ構成案（判断）

```
Cargo.toml            workspace
kernel/               M モードカーネル（bin, riscv32imafc-unknown-none-elf）
  src/arch/riscv32/   起動、トラップ入口（asm）、コンテキスト、PMP（virt と P4 で共通）
  src/platform/virt/  QEMU virt 用（NS16550 UART、CLINT、sifive_test で終了）
  src/platform/esp32p4v1/  hw_ver1 から手書きしたレジスタ定義、WDT、USJ、SYSTIMER、CLIC、PMA、キャッシュ/MMU
  src/loader/         T5A の検証、再配置
  memory-v1.x         リンカスクリプト（v3.x 用は別ファイル）
abi/                  syscall 番号と型（no_std。カーネルとアプリで共有）
user/rt/              アプリのランタイム（t5-rt。_start、syscall スタブ、アロケータ、panic）
apps/hello/           最初のアプリ
tools/elf2t5a/        ホストツール（ELF の検査 → T5A 生成と署名）
boot/                 ESP-IDF の bootloader をビルドするための設定（sdkconfig.defaults, partitions.csv）
doc/
```

ツールチェーン:
- カーネルは stable の Rust でビルドする予定。
- アプリは PIE で core を作り直すために `-Zbuild-std` を使うので、nightly が必要（04 §5）。
- 手元には stable 1.99.1 と nightly があり、`riscv32imafc-unknown-none-elf` もインストール済み。espflash は 4.4.0、ESP-IDF 6.0 は `~/esp-idf/6.0` にある。

## 7. マイルストーン

| # | 内容 | 完了条件 | 実機書き込み |
|---|---|---|---|
| **M0** | 準備。workspace、カーネルの雛形とリンカスクリプト、`esp_app_desc_t`、bootloader の設定とビルド、パーティションテーブル。**静的 PIE の実験**（ホストで `readelf -r` を見て再配置が RELATIVE だけになるか確認。04 §6 の未確認事項） | ビルドが通る。PIE の結論が出る | なし |
| **M1-virt** | M1b と同じ内容（U モードのアプリが ecall で `dbg_write` と `exit`、保護テスト 3 種類）を、Rust のカーネルで QEMU virt 上で先に通す。CI で自動実行できるようにする。トラップ入口は最初から P4 と同じ書き方にする（mcause の上位ビットをマスクして判定、64B 整列、mscratch=0 でカーネル実行中を示す）（08 §5）。同じ内容は、調査時に asm の試作で virt 上で通ることを確認済み（08 §4） | QEMU で U モードの hello と 3 種類のフォールトを確認 | なし |
| **M0.5** | 実機のフラッシュ 16MB 全体をバックアップ（`espflash read-flash`）。いまの portable_terminal に戻せるようにする | バックアップを取得し、ハッシュを記録 | 読み出しのみ（要確認） |
| **M1a** | bootloader、パーティションテーブル、カーネルを書き込む。M モードで "hello" を出し、診断（chip rev、PMP/PMA、misa）をダンプ、RWDT を停止 | USB Serial/JTAG にログが出る。ロック済み PMP がない | **あり（要確認）** |
| **M1b** | **U モードのアプリが ecall で出力する。** カーネルに埋め込んだアプリ（PIE を L2MEM 上側へ再配置、M0 の実験が失敗したら固定アドレスでリンク）を、PMP の 2 領域で U モード実行し、`dbg_write` → USB Serial/JTAG、`exit`。あわせて**保護テスト**: アプリが ① 0x500D_2000 を読む、② カーネル領域に書く、③ data 領域を実行する → それぞれカーネルがアクセスフォールトを捕まえてプロセスを終了させ、カーネルは動き続ける | 正常系と 3 種類の異常系がすべて期待どおり | あり |
| M2 | SYSTIMER のティック、2 プロセスのプリエンプション、`yield`/`sleep`/`time`、FPU の遅延保存 | 2 つのアプリが交互に出力する | あり |
| M3 | T5A、`elf2t5a`、`apps` パーティションからのロード、Ed25519 検証、ハンドル、`open`/`read`/`write`/`wait` | 書き込んだアプリを再起動なしで起動・終了できる | あり |
| M4 | クロック 360MHz、PSRAM の初期化（MSPI のタイミング調整を含む）、アプリプールを PSRAM へ移す | PSRAM 上のアプリが動く | あり |
| M5 | カーネル OTA（otadata とロールバック）、アプリストアの A/B 更新、IPC チャネル | 更新と自動ロールバックの実演 | あり |
| M6 | I2C → IO エクスパンダ → MIPI-DSI 表示、`shm_map` でアプリに描画させる。パネルの判別は 06 §5 | アプリが画面に描画する | あり |

その後: タッチ、SD（FAT）、オーディオ、USB、C6 経由の Wi-Fi（SDIO + esp_hosted、重い）、Core1 の起動（06 §6）。

### M1 の具体的な手順
1. `boot/`: ESP-IDF 6.0 で空のプロジェクトを作り、`idf.py bootloader` で bootloader.bin を得る。設定は次のとおり。
   - `ESP32P4_SELECTS_REV_LESS_V3=y`
   - `BOOTLOADER_APP_ROLLBACK_ENABLE=y`
   - `BOOTLOADER_REGION_PROTECTION_ENABLE=n`
   - フラッシュ 16MB、パーティションテーブルは 05 §0 の案
2. `kernel/`: `_start`（asm）→ `kmain`。リンカスクリプトは、text をフラッシュ XIP（0x4000_0000〜）に、data/bss/スタックを L2MEM 下側に置く。`esp_app_desc_t` を `.flash.appdesc` 相当のセクションに置く。
3. `espflash save-image --chip esp32p4` で ESP app image に変換する。v1.x 向けに chip revision の範囲が正しく入るかを確認する（05 §7 の未確認事項）。
4. （確認後）`espflash write-bin` で bootloader @0x2000、パーティションテーブル @0x8000、カーネル @0x10000 を書き込み、`espflash monitor` で確認する。
5. USB Serial/JTAG の出力: `0x500D_2004` の bit1 を待ってから `0x500D_2000` に書き、最後に bit0（WR_DONE）を立てる（06 §3.1）。
6. U モードへ入る手順:
   - pmpaddr1/2 と pmpcfg に NAPOT 領域を設定する。
   - `mstatus.MPP=0`、`MPIE=0`、`mepc=entry` とする。
   - mscratch にカーネルのトラップフレームを入れて `mret` する。
7. トラップ: `mcause` が 8 なら syscall、1/5/7 ならアクセスフォールトとしてプロセスを終了させる。それ以外は panic。

## 8. リスクと未確認事項（実機で早めに潰す順）

0. virt では再現できない差（CLIC、PMP の粒度 128B、ロックされた PMP、ネストした例外での lock-up、PMA、キャッシュ同期）。M1a/M1b の最終判定は実機で行う（08 §3.2）。
1. ブートローダを作り直したとき、ロックされた PMP/PMA が残っていないか（M1a の診断ダンプで確認）。PMA の空きエントリ数。
2. v1.x の U モードへ実際に入れるか。U モード実行中の M 割り込みとアクセスフォールトの mcause/mtval/mepc が正確か（01 §9）。
3. Tock が ESP32-C3 で踏んだような「トラップベクタが U から実行可能でないとフォールトする」バグが P4 にもあるか（PMP#0 を予備にしてある）。
4. ブートローダ直後に USB Serial/JTAG がそのまま使えるか（06 §7）。
5. ~~Rust の静的 PIE で、再配置が RELATIVE だけになるか~~ → **M0 で確認済み**。nightly-2026-09-10 で `-Zbuild-std=core`、`-C relocation-model=pie`、`-pie --no-dynamic-linker -z text` を使うと、ELF は ET_DYN で、動的再配置は `R_RISCV_RELATIVE` だけ（hello 5 件、fault 6 件）。
6. ~~espflash の app image が v1.x を受け付けるか~~ → **M0 で一部確認**。`.flash.appdesc` セクション（フラッシュ窓 0x4000_0020）に `esp_app_desc_t` を置けば、`espflash save-image --min-chip-rev 1.0` で、チップリビジョン v1.0〜v1.99 の有効なイメージになる（`esptool image-info` で検証）。実機のブートローダが受け付けるかは M1a で確認する。
   - 注意: ブートローダは自分自身を 0x4FF2_9ED0 付近に置いて動く（既存ファームの boot.log）ため、カーネルのロードされるセグメントはそれより下に収める。
7. fence.i が L1 I キャッシュに効くか。効かなければキャッシュコントローラで同期する（02 §2.2）。
8. errata APM-560（DMA 側の APM。PSRAM やフラッシュへのアクセスを拒否する使い方は避ける。01 §3.5）。
9. ディスプレイのパネル種別（ILI9881C / ST7123 / ST7121。06 §1.4）。M6 までに確認する。

## 9. 参考（主要なもの）
- 各章の参考文献一覧（01〜06 の末尾）
- ESP32-P4 TRM v0.7: https://www.espressif.com/sites/default/files/documentation/esp32-p4_technical_reference_manual_en.pdf
- ESP-IDF 6.0（ローカル `~/esp-idf/6.0`）: `components/esp_hw_support/port/esp32p4/cpu_region_protect.c`、`components/bootloader/Kconfig.projbuild:274-282`
- Tock TRD104: https://github.com/tock/tock/blob/master/doc/reference/trd104-syscalls.md
- Zephyr RISC-V PMP / userspace: https://github.com/zephyrproject-rtos/zephyr/blob/main/arch/riscv/core/pmp.c
- Xous: https://github.com/betrusted-io/xous-core
- ESP Privilege Separation: https://github.com/espressif/esp-privilege-separation
- NuttX Tab5: https://nuttx.apache.org/docs/latest/platforms/risc-v/esp32p4/boards/esp32p4-tab5/index.html
