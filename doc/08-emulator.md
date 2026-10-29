# 08. 開発初期から使えるエミュレータ

ESP32-P4 rev v1.0（Tab5）向け自作 OS のうち、実機に書き込む前にエミュレータで確かめられる範囲を調べた。あわせて、その確かめ方も検討した。前提は 07 §0〜§3、§7 にある。事実には出典を付け、推測には「（推測）」、設計判断には「（判断）」と書く。

- 版: 初版（2026-10-29）
- 検証環境: Ubuntu（Linux 6.5）、上流 QEMU 8.2.2（`/usr/bin/qemu-system-riscv32`）、Espressif QEMU `esp_develop_9.2.2_20250817`（`~/.espressif/tools/qemu-riscv32/`）、`riscv32-esp-elf-gcc` 14.2.0_20260121、`riscv32-esp-elf-gdb` 17.1_20260402
- 検証コードとログ: リポジトリの `experiments/qemu-virt-umode/`（`umode.S`、`link.ld.in`、`build.sh`、`run_virt*.log`）。esp32c3 と GDB のログは調査時の一時コピーで、リポジトリには含めない。

---

## 0. 結論

**ESP32-P4 を模すエミュレータは、2026-10-29 時点で Espressif の QEMU フォークにはない。** 自由に使える（OSS でローカル実行できる）ものも見当たらない。このため推奨構成は次の 3 層になる（判断）。

| 層 | 使う環境 | 何を確かめるか |
|---|---|---|
| **ホスト（Hosted）** | `cargo test`（Linux プロセス） | syscall の検証関数、ハンドル表、IPC、T5A パーサ、再配置、署名検証などの純ロジック |
| **アーキ層** | **上流 QEMU `qemu-system-riscv32 -M virt`**（`-cpu rv32,s=off,h=off,mmu=off,d=off,pmp=on`） | トラップ入口、`mscratch` によるスタック切り替え、PMP NAPOT、`mret` で U モードへ入る、ecall（mcause=8）、アクセスフォールト（1/5/7）、コンテキストスイッチ、ローダと再配置、GDB |
| **SoC 層** | **実機（Tab5）**。補助として **Wokwi の ESP32-P4（`chipRevision: "1.0"`、beta）** | ブートローダからの起動、app image の形式、USB Serial/JTAG、SYSTIMER、CLIC、PMA、キャッシュ/MMU、PSRAM。いずれも最終確認は実機で行う |

理由は次のとおり。

1. **Espressif QEMU に P4 のマシンがない。** 最新のリリース（`esp-develop-9.2.2-20260417`）にも、開発ブランチ `esp-develop`（2026-04-29 に ESP32-C6 マシンを追加）にも、`esp32p4` のソースが 1 つもない（§1.1）。ESP-IDF 6.0 の `idf.py qemu` が対応するのも esp32、esp32c3、esp32s3 だけ（`~/esp-idf/6.0/tools/idf_py_actions/qemu_ext.py:54-139`）。Espressif の担当者は 2025-06 の時点で「C6 を優先し、P4 もやるのが妥当だと思う」と書いたが、時期は示していない（espressif/qemu#127）。
2. **上流 `virt` で 07 §7 の M1b に当たるテストがすべて通った。** M モード出力、PMP 2 エントリと `mret` による U モード移行、ecall の mcause=8、禁止領域への load/store/fetch の mcause=5/7/1 を確認した（§4.2）。GDB スタブも動く。CI にそのまま載せられる。
3. **virt と P4 v1.x の違いは、カーネルの SoC 依存部（コンソール、タイマ、割り込みコントローラ）と CLIC 固有の挙動に集中している。** trait で切り替えれば、アーキ層のコードは共通にできる（§3.3）。ただし CLIC、PMP の粒度 128B、ロックされた PMP、ネストした例外での lock-up、PMA、キャッシュ同期は virt では再現できない。そのため M1a/M1b の最終判定は実機で行う（§3.2）。
4. **Wokwi は P4 v1.0 を選べる唯一の候補。** ただしクラウド実行（無料枠は 30 日で 50 分）で、beta、ソースは非公開。PMP、U モード、CLIC がどこまで忠実かは公開資料に書かれていない（§2.3）。「bootloader → カーネル image → USB Serial/JTAG に hello」の起動経路を、実機に書き込む前に確かめる補助として使う価値はある（推測）。**今回はアカウントとトークンがないため、実際には動かしていない。**

---

## 1. Espressif の QEMU フォーク（espressif/qemu）

### 1.1 ESP32-P4 への対応状況: なし

| 確認項目 | 結果 | 出典 |
|---|---|---|
| 最新リリース | `esp-develop-9.2.2-20260417`（2026-04-19）。リリースノートは ESP32-S3 の SDMMC と TIE、ESP32-C3 の割り込みの修正などで、P4 への言及はない | `gh api repos/espressif/qemu/releases`（https://github.com/espressif/qemu/releases/tag/esp-develop-9.2.2-20260417） |
| `esp-develop` ブランチの RISC-V マシン | `hw/riscv/esp32c3*.c` と `hw/riscv/esp32c6*.c` だけ。ツリー全体を `p4` で grep しても、P4 関連のファイルは 0 件 | `gh api repos/espressif/qemu/git/trees/esp-develop?recursive=1`（2026-10-29 取得、HEAD `febae182e1`） |
| C6 マシン | 2026-04-29 に `esp-develop` へ追加（`febae182e1 hw/riscv: implement ESP32-C6 machine`、`178ec5aafb target/riscv: add PMA CSR support ...`）。リリースにはまだ入っていない | `gh api repos/espressif/qemu/commits?sha=esp-develop` |
| ROM イメージ | `pc-bios/` にあるのは `esp32-v3-rom*.bin`、`esp32c3-rom.bin`、`esp32c6-rom.bin`、`esp32s3_rev0_rom.bin`。P4 用はない | 同ツリー |
| P4 対応の要望 | Issue #127「Is QEMU support planned for esp32p4 and esp32c6?」（2025-05-17、open）。igrr（2025-06-30）: "I think it makes sense to do P4 as well, just C6 will probably have priority" | https://github.com/espressif/qemu/issues/127 |
| 公式の対応表 | ESP32、ESP32-S3、ESP32-C3 の 3 列だけ。USB はどのチップも ❌。"Espressif does not provide support for QEMU" | https://github.com/espressif/esp-toolchain-docs/blob/main/qemu/README.md（l.9-45, 51） |
| ESP-IDF 6.0 の `idf.py qemu` | `QEMU_TARGETS` は esp32、esp32c3、esp32s3 だけ。それ以外のターゲットでは `QEMU is not supported for target {target}` と表示して終了する | `~/esp-idf/6.0/tools/idf_py_actions/qemu_ext.py:54-139, 233-237` |
| ESP-IDF の tools.json | `qemu-riscv32` は `esp_develop_9.2.2_20250817`、`supported_targets: ["esp32c3"]`（6.0）。master でも版が `..._20260417` になっただけで、対象は esp32c3 のまま | `~/esp-idf/6.0/tools/tools.json:1011-1012`、`espressif/esp-idf` master の `tools/tools.json` |

したがって、依頼にあった「`-M esp32p4` の対応リビジョン」「P4 の ROM」「CLIC の模擬」「USB Serial/JTAG」「ESP-IDF の bootloader と app image を `idf.py qemu` で起動」は、**いずれも該当なし**となる。

**参考プロジェクトとの食い違い**: `~/repos/tab5_claude_client` の `CLAUDE.md:30-38` と `Makefile:30,68-75` には「ESP-IDF 6.0 ESP32-P4 QEMU support is preliminary」とあり、`make qemu` の手順も書かれている。しかし次の 2 点から、**この `make qemu` は動かないはず**である（推測。ビルドが重いので今回は実行していない）。
- `idf.py qemu` が esp32p4 を受け付けない（上の `qemu_ext.py:233-237`）。
- Makefile が参照する `sdkconfig.qemu` がリポジトリにない（`ls` で確認）。

### 1.2 フォークの CPU モデル（P4 が来たときの参考として）

既存の ESP 用 CPU モデル（`target/riscv/esp_cpu.c`）は、上流 QEMU の汎用 RISC-V コアに CSR とトラップの差分を足したもの。

- `esp_cpu_init` は `set_misa(env, MXL_RV32, RVI | RVM | RVC)` を呼ぶ（`esp_cpu.c:410`、esp-develop）。**ところが実際に -M esp32c3 で misa を読むと `0x401411ad`（I M A F D C S U）だった**（§4.2 のログ）。実機の C3 は IMC で、S/F/D はない。この差は、親クラスの初期化が後から misa を上書きしているためと考えられる（推測）。つまり**このモデルの特権機能は、実チップより上流 QEMU の汎用コアに近い**。
- PMP: 16 エントリ。粒度は G=0（4 バイト）で、`pmpaddr15` に `~0` を書くと `0xffffffff` が読み返される（§4.2）。実チップ（C3 も P4 も 128B 粒度）とは違う。
- 割り込み: C3 と C6 は Interrupt Matrix（C6 は PLIC を内蔵）を `MIP_MEIP` 1 本に集約する方式（`esp_cpu.c:352`、`esp32c6_intmatrix.c`）。**CLIC のモデルはない**（上流 QEMU にもない。§2.1）。
- PMA CSR（0xBC0–0xBDF）は C6 用に、読み書きできるだけのものが入った（`esp_cpu.c:358-367`）。P4 の PMA と同じ CSR 番号なので、P4 対応の下地になりうる（推測）。
- USB Serial/JTAG（C3）は**空のスタブ**。読むと 0 が返り、書き込みは捨てられる（`hw/misc/esp32c3_jtag.c:19-30`）。コンソールに使えるのは UART0 だけ。

---

## 2. 上流 QEMU `-M virt` と、その他の選択肢

### 2.1 上流 QEMU `qemu-system-riscv32 -M virt`

- **メモリマップ（8.2.2 の `info mtree`）**: sifive_test 0x0010_0000、ACLINT SWI 0x0200_0000、ACLINT MTIMER 0x0200_4000、PLIC 0x0C00_0000、NS16550 UART 0x1000_0000、virtio-mmio 0x1000_1000〜、RAM 0x8000_0000〜。P4 の配置（CLINT 0x2000_0000、CLIC 0x2080_0000、L2MEM 0x4FF0_0000、HP 周辺 0x5000_0000）とは全く違う。
- **割り込み**: CLINT/ACLINT + PLIC、オプションで AIA（APLIC/IMSIC。`-M virt,help`）。**上流には CLIC の実装がない**（`hw/intc/` にあるのは `riscv_aclint.c`、`riscv_aplic.c`、`riscv_imsic.c`、`sifive_plic.c`。GitHub の qemu/qemu、2026-10-29 時点）。`mtvec` に MODE=3 を書いても WARL で 0 に戻る（§4.2 で実測）。
- **PMP**: CPU プロパティ `pmp`（既定 on）、`smepmp`、`u`、`s`、`mmu`、`priv_spec`（8.2.2 の QMP `device-list-properties` で確認）。
  - 8.2.2 はエントリ数 16 固定で、粒度を設定するプロパティがない（G=0）。
  - 上流では `num-pmp-regions` が **v10.1.0** で（`target/riscv/cpu.c:1604,2608`@v10.1.0）、`pmp-granularity` が **v10.2.0** で（`cpu.c:1642,2647`@v10.2.0）追加された。**QEMU ≥ 10.2 なら `num-pmp-regions=16,pmp-granularity=128` で P4 v1.x の PMP 形状を再現できる**（推測。プロパティの存在はソースで確認したが、実行はしていない。ローカルは 8.2.2）。
- **P4 に寄せた CPU 設定**: `-cpu rv32,s=off,h=off,mmu=off,d=off,pmp=on` で misa は `0x40101125`（I M A F C U）になり、P4 HP コアの「M+U、S なし、MMU なし、F あり D なし」と同じ構成になる。この設定でも §4.2 のテストはすべて通った（`run_virt_p4like.log`）。`priv_spec=v1.10.0` を指定すると `H extension requires priv spec 1.12.0` で起動しない（h=off と組み合わせれば使えるはずだが（推測）、今回は不要と判断して試していない）。
- **Rust 製 OS の先行例**
  - Tock `boards/qemu_rv32_virt`: カーネルを `-bios` として読み込み、VirtIO（net、rng）を使う。動作確認済みの QEMU は 8.2.7、9.1.3、9.2.3、10.0.2 で、8.1.5 以下は動かない（Tock `boards/qemu_rv32_virt/Makefile:10-12`、README l.1-35）。PMP は `KernelProtectionMMLEPMP<16, 5>`、つまり **ePMP（Smepmp）前提**（`chips/qemu_rv32_virt_chip/src/chip.rs:25-27`）。P4 には Smepmp がないので、Tock の PMP 実装はそのままでは参考にならない（推測。01 §3.2 に P4 の Smepmp についての記述はない）。U モードのプロセス、トラップ、syscall の作りは参考になる。
  - rCore-Tutorial は RV64 で、S モードのカーネル + SBI という構成。M+U の P4 とは構造が違うので、参考の度合いは低い（推測。今回は未確認）。

### 2.2 Renode
- 公式リポジトリの `platforms/cpus/` には RISC-V の `riscv_virt.repl`、LiteX/VexRiscv、OpenTitan、Kendryte などがあるが、**ESP 系は 1 つもない**（`gh api repos/renode/renode/git/trees/master`）。
- `renode-infrastructure` には `CoreLocalInterruptController.cs`（CLIC）がある（GitHub のコード検索）。**CLIC を持つ RISC-V プラットフォームを `.repl` で自作できる**ので、P4 の CLIC を模す道は QEMU より近い（推測）。ただし、v1.x の非標準 CLIC（閾値が MMIO、mintstatus が 0x346）や USB Serial/JTAG のモデルは自分で書くことになる。
- 有志による ESP32-S3 の移植（Sean Cross、`git.xobs.io/xobs/renode-esp32s3`、WIP）がある。Xous は Precursor のエミュレーションに Renode を使っている（xous-core README「Emulation」）。

### 2.3 Wokwi
- **ESP32-P4 に対応（beta）**。ボードは ESP32-P4-Preview-DevKitC-1。**`chipRevision` 属性で `"1.0"` か `"3.1"` を選べる（既定は 3.1）**。"code built for a specific chip revision may not work on other revisions"（https://docs.wokwi.com/guides/esp32）。
- P4 で使える周辺: UART、USB Serial/JTAG（シリアルのみで JTAG はなし）、USB CDC、PSRAM（`psramSize` 2/4/8MB、quad/octal）、フラッシュ（`flashSize` 2〜32MB）、タイマ、WDT、GDMA、I2C（マスタのみ）、SPI、RMT、LEDC、RNG、AES/SHA/RSA/ECC、Wi-Fi（ESP-Hosted 経由）。GDB は「Wokwi for VS Code」から使える。I2S、MCPWM、ULP はない（同ページ）。
- **SYSTIMER、PMP、U モード、CLIC、PMA、キャッシュ/MMU の忠実度は、ページに記述がない**（未確認）。
- ファームウェアは `.bin`、`.elf`、`.uf2` を受け付ける。bootloader、パーティションテーブル、app を `esptool merge_bin` でまとめた 1 つの `.bin` を推奨している。オフセットが判別できないときは app とみなして 0x10000 に置く（同ページ）。
- CI: `wokwi-cli` を使い、クラウドでシミュレーションしてシリアル出力を返す。シミュレーション時間は 30 日で Free 50 分、Hobby 200 分、Hobby+ 500 分、Pro 2000 分。1 セッションの上限は Free/Hobby で 5 分（https://docs.wokwi.com/wokwi-ci/getting-started）。トークンが必要（推測。CLI の文書は読んでいない）。
- Wokwi の作者（urish）は espressif/qemu#127 で「Wokwi supports both ESP32-C6 and ESP32-P4」と書いている（2025-07-14）。

### 2.4 Hosted モード（Xous 風）
- Xous の `kernel/src/arch/` には `hosted/`（`process.rs`、`syscall.rs`、`mem.rs`、`irq.rs`）がある。カーネルとプロセスを Linux のプロセスとして動かすモードである（xous-core のツリー。動作の詳細は Xous Book によるもので、今回は読んでいない）。
- 本プロジェクトに当てはめると、`syscall` のディスパッチより上（引数の検証、ハンドル、チャネル、T5A の検証と再配置計算、Ed25519）を `#[cfg(test)]` の通常のホスト Rust でテストできる。ecall やトラップの境界はテストできないので、そこは virt で確かめる（判断）。

---

## 3. 比較

### 3.1 比較表

| 項目 | Espressif QEMU | 上流 QEMU virt | Wokwi（P4 beta） | Renode | Hosted（Linux） |
|---|---|---|---|---|---|
| **P4 の模擬** | **なし**（C3、C6〔未リリース〕、ESP32、S3 のみ） | なし（汎用 RV32） | **あり**（v1.0 / v3.1 を選べる） | なし（`.repl` を自作すれば可能） | なし |
| CPU 特権機能 | M/U/S が動く（misa は汎用コアと同じ。§4.2） | M/U（`s=off` で S を消せる）。実測 OK | 記述なし（未確認） | 汎用 RISC-V コア（未確認） | なし（ホストのプロセス） |
| PMP | 16 エントリ、G=0（実測） | 16 エントリ。8.2 は G=0、≥10.2 で粒度とエントリ数を設定できる（ソースで確認） | 記述なし | あり（未確認） | なし |
| 割り込み | Interrupt Matrix → MEIP（C3/C6）。CLIC はない | CLINT/ACLINT + PLIC（AIA）。CLIC はない。mtvec MODE=3 は無視（実測） | 記述なし（P4 の CLIC を模しているかは不明） | **CLIC のモデルあり**（標準 CLIC。v1.x の差は自作） | なし |
| コンソール | UART0（`-serial file:` で出力を実測）。**USB Serial/JTAG はスタブ** | NS16550 @0x1000_0000（実測） | UART、**USB Serial/JTAG（シリアル）** | UART 各種 | stdout |
| フラッシュ/MMU | C3: SPI flash（`if=mtd`）、MMU、暗号化 | pflash/virtio（ESP の MMU はない） | `flashSize` 指定可。MMU の忠実度は不明 | 自作 | ファイル |
| PSRAM | C3 にはない（S3 にはある） | なし（RAM を増やすだけ） | あり（`psramSize`） | 自作 | なし |
| GDB | `-gdb tcp::N`（実測 OK） | `-gdb tcp::N`（実測 OK） | VS Code 拡張経由 | あり | 通常のデバッガ |
| CI 適性 | ◯（ローカル、OSS）。ただし P4 は不可 | **◎**（ローカル、OSS、数秒で終わる、sifive_test で終了コードを返せる） | △（クラウド、トークン、無料は 50 分/月、5 分/セッション） | ◯（ローカル、OSS、.NET） | **◎** |
| v1.x との差 | チップが違う。ROM、周辺のアドレス、CLIC、PMA、USB-SJ が違う | CLIC、PMP 粒度、メモリマップ、PMA、キャッシュ、ロックされた PMP、lock-up がない | 公称は v1.0。CPU 内部の忠実度は不明 | すべて自作 | 特権境界がない |

### 3.2 virt で確かめられないこと（実機か Wokwi で確認）
1. CLIC: `mtvec.MODE=3` 固定、mcause の上位フィールド（MINHV/MPP/MPIE/MPIL。01 §2.2）、閾値が MMIO 0x2080_0008、mintstatus が 0x346、mscratchcsw。**トラップ入口は `mcause` の例外コードをマスクしてから分岐する必要がある**が、virt ではこれらのビットが常に 0 なので、マスクを忘れていても見つからない（判断）。
2. PMP の粒度 128B と NA4 がないこと（8.2.2 の virt では G=0。QEMU ≥10.2 の `pmp-granularity=128` で部分的に再現できる見込み）。
3. ブートローダがロックした PMP/PMA（07 §0）。virt はすべてロックなしで起動する。
4. トラップ中の例外による CPU lock-up（01 §0、TRM §2.13.5）。QEMU は普通にネストしたトラップとして処理する。
5. PMA、L1/L2 キャッシュ、`fence.i` の効き方、外部メモリ MMU、PSRAM、XIP。
6. USB Serial/JTAG、SYSTIMER、TIMG/RWDT、eFuse のチップリビジョン、Interrupt Matrix。

### 3.3 SoC 依存部を trait で切り替える構成（判断）
```
kernel/
  arch/riscv32/   … 共通: トラップ入口(asm)、TrapFrame、PMP NAPOT/TOR エンコード、
                     mret による U モード移行、syscall のディスパッチ、コンテキストスイッチ
  platform/virt/  … NS16550 Console、CLINT mtimer Timer、PLIC Irq、sifive_test で終了
  platform/esp32p4v1/ … USB Serial/JTAG Console、SYSTIMER Timer、CLIC + Interrupt Matrix Irq、
                     PMA、RWDT 停止、L2MEM のリンカスクリプト
```
- trait の例: `Console`、`Timer`（`now()`、`set_deadline()`）、`IrqCtrl`（`enable`、`claim`/`complete`、`threshold`）、`Platform`（`init()`、`mem_regions()`、`exit()`）、`Mpu`（エントリ数と粒度を定数で持つ）。
- **利点**
  - M1b（U モード + ecall + 保護テスト）、M2（プリエンプション）のスケジューラ部分、M3（ローダ、再配置、ハンドル）を、実機なしで CI 回帰テストにできる。
  - 粒度とエントリ数を `const` にしておけば、virt と P4 の差（G=0 と 128B）に依存するバグが表に出やすい。
- **限界**
  - 割り込みのモデルが根本的に違う（PLIC の claim/complete と CLIC のレベル/閾値）。「syscall は割り込み禁止で短く」（07 §4.3）のような CLIC 前提の設計は、virt では検証にならない。
  - トラップベクタの配置（CLIC では 64B 整列で例外は BASE へ）は virt の direct モードでも同じように書けるが、割り込みベクタ（mtvt）の経路は実機でしか試せない。
  - アドレス固定の部分（リンカスクリプト、PMP で塞ぐ範囲）は platform ごとに別物になる。

---

## 4. 実物での検証

### 4.1 テストの内容
`umode.S`（1 ファイル、RV32IMAC、`.option norvc`）。`-DPLAT_VIRT` と `-DPLAT_ESP32C3` で UART のアドレスとリンク先だけを切り替える。

1. M モードで UART に hello を出し、misa、mtvec、mstatus を表示する。mtvec に MODE=3 を書いて読み返す。
2. `pmpaddr15 ← ~0` を読み返して粒度を推定する。
3. PMP0 = user text 4KiB NAPOT R-X（cfg 0x1d）、PMP1 = user data 4KiB NAPOT RW-（cfg 0x1b）。カーネル領域には PMP エントリを作らない。
4. `mstatus.MPP=0, MPIE=0, mepc=uentry` として `mret` する。
5. U モードで順に次を行う。ecall → カーネル data の load → store → カーネル text へ jalr → 自分の data に書く（許可）→ UART MMIO に store → 自分の text に store → 自分の data へ jalr → `ecall(a7=99)` で終了。
6. トラップハンドラは `csrrw sp, mscratch, sp` で M のスタックに切り替え、mcause、mepc、mtval、mstatus.MPP を表示する。5/7 と ecall は mepc+4、1 は ra に戻す。

ビルド:
```
riscv32-esp-elf-gcc -march=rv32imac_zicsr -mabi=ilp32 -nostdlib -nostartfiles \
  -DPLAT_VIRT -T link_VIRT.ld -o umode_VIRT.elf umode.S        # BASE=0x8000_0000
  -DPLAT_ESP32C3 -T link_ESP32C3.ld -o umode_ESP32C3.elf        # BASE=0x4038_0000 (C3 IRAM)
```

### 4.2 結果

**(1) 上流 QEMU 8.2.2 virt — 成功**
```
$ qemu-system-riscv32 -M virt -cpu rv32 -bios none -nographic -kernel umode_VIRT.elf
[M] hello from M-mode
[M] misa=401411ad
[M] mtvec=80000200
[M] mtvec after writing MODE=3: 80000200
[M] pmpaddr15 after writing ~0 = ffffffff
[M] pmpcfg0=00001b1d
[M] pmpaddr0=200005ff
[M] mret -> U
[M] trap mcause=00000008 mepc=80001004 mtval=00000000 MPP=00000000   ← (c) U の ecall
[M] trap mcause=00000005 mepc=80001010 mtval=80004000 MPP=00000000   ← (d) カーネル data の load
[M] trap mcause=00000007 mepc=80001014 mtval=80004000 MPP=00000000   ← (d) カーネル data への store
[M] trap mcause=00000001 mepc=800003a0 mtval=800003a0 MPP=00000000   ← (d) カーネル text の実行
[M] trap mcause=00000007 mepc=80001040 mtval=10000000 MPP=00000000   ← U から UART MMIO
[M] trap mcause=00000007 mepc=8000104c mtval=80001000 MPP=00000000   ← R-X 領域への store
[M] trap mcause=00000001 mepc=80003000 mtval=80003000 MPP=00000000   ← RW- 領域の実行
[M] trap mcause=00000008 mepc=80001060 mtval=00000000 MPP=00000000
[M] U exit(ecall a7=99): DONE                                         （終了コード 0）
```
（`run_virt.log`）

- (a)〜(d) はすべて期待どおり。MPP=0 なので、トラップは U モードから来ている。
- mtvec の MODE=3 は無視され、粒度は G=0。
- `-cpu rv32,s=off,h=off,mmu=off,d=off,pmp=on` でも、misa=`40101125` で同じ 8 トラップが出て DONE になった（`run_virt_p4like.log`）。

**(2) Espressif QEMU 9.2.2（esp_develop_20250817）`-M esp32c3`（P4 がないので代わりに使用）— 成功**
```
$ qemu-system-riscv32 -M esp32c3 -display none -serial file:c3_uart0.txt -monitor none -kernel umode_ESP32C3.elf
Not initializing SPI Flash
Loading kernel at address 0x40380000
（c3_uart0.txt）misa=401411ad, pmpaddr0=100e05ff, mcause 8,5,7,1,7(mtval=60000000),7,1,8 → DONE
```
- (a)〜(d) は virt と同じ結果。`-d int` のログにも `desc=user_ecall`、`fault_load`、`fault_store`、`fault_fetch` が出た（`qlog.txt`）。
- 出力が見えたのは `-serial file:` のときだけ。`-nographic` や `-serial stdio` をパイプにつないだ場合は、UART の出力が見えなかった（原因は未調査）。
- `-kernel` に ELF を渡すと ROM を通らずにそのアドレスから実行する（`hw/riscv/esp32c3.c:267-302`）。このため bootloader と app image の経路は試していない。
- **misa に S、F、D が立っていて、実機の C3（IMC）と合わない。** このモデルの特権機能は実チップを模していない、と読むべきである。

**(3) GDB スタブ — 両方で成功**
```
$ qemu-system-riscv32 ... -kernel umode_VIRT.elf -gdb tcp::12345 -S &
$ riscv32-esp-elf-gdb -batch -ex 'target remote :12345' -ex 'break trap' -ex continue \
    -ex 'p/x $mcause' -ex 'p/x $mepc' umode_VIRT.elf
Breakpoint 1, 0x80000200 in trap ()   $2 = 0x8   $3 = 0x80001004
```
（`gdb_virt.log`。esp32c3 でも mcause=0x8、mepc=0x40381004 になった: `gdb_c3.log`）

**(4) 実行していないもの**
- ESP32-P4 の app image と bootloader を QEMU で起動すること。起動できるマシンが存在しないため、`idf.py bootloader`、`espflash save-image`、`merge_bin` は作っていない。
- Wokwi（トークンがない）、Renode（未インストール）、QEMU ≥10.2 の `pmp-granularity`（ローカルは 8.2.2 のみ）。
- 実機には一切アクセスしていない。

---

## 5. マイルストーン（07 §7）への組み込み案（判断）

| # | 追加・変更 |
|---|---|
| **M0** | `platform/virt` を作る。`cargo xtask qemu` で `qemu-system-riscv32 -M virt -cpu rv32,s=off,h=off,mmu=off,d=off,pmp=on -bios none -nographic -kernel <elf>` を起動し、sifive_test（0x10_0000 に 0x5555 で成功、0x3333 で失敗）の終了コードで CI の合否を返す。Hosted の `cargo test` も CI に入れる |
| **M1-virt（新設。M1a の前）** | 07 M1b の内容（U モードのアプリが ecall で `dbg_write` と `exit`、保護テスト 3 種類）を virt で先に通す。今回の `umode.S` が雛形になる。トラップ入口は最初から「mcause をマスクして判定」「64B 整列」「mscratch=0 でカーネル実行中を示す」で書き、P4 と同じコードにしておく |
| M1a/M1b（実機） | virt で通ったカーネルを `platform/esp32p4v1` に差し替えて実機に書き込む。**実機でしか確かめられない項目**（ロック済み PMP/PMA、CLIC の mcause、粒度 128B、lock-up、USB Serial/JTAG）を診断ダンプの対象にする。（任意）書き込み前に Wokwi（`chipRevision: "1.0"`）で、bootloader + app image の起動と USB Serial/JTAG の hello を試す。これで 05 §7 の「espflash の app image が v1.x を受け付けるか」を先に確かめられる可能性がある |
| M2 | スケジューラとコンテキストスイッチを virt の CLINT mtimer で回帰テストにする。SYSTIMER と CLIC の部分は実機で確かめる |
| M3 | T5A、`elf2t5a`、再配置、Ed25519、ハンドルを Hosted と virt で確かめる（アプリは `-device loader,file=app.t5a,addr=...` で RAM に置く）。`apps` パーティションからの読み出しは実機で |
| M4 以降 | PSRAM、MSPI、MMU、OTA、MIPI-DSI は実機が主。OTA の状態遷移ロジックは Hosted でテストできる |
| 継続 | QEMU を 10.2 以上にして `num-pmp-regions=16,pmp-granularity=128` を CI の既定にする。espressif/qemu#127 と、`esp-develop` に `esp32p4` が入るかを定期的に確認する |

---

## 6. 未確認事項
1. Wokwi の ESP32-P4（`chipRevision: "1.0"`）が、U モード、PMP（16 エントリ、128B）、CLIC v1.x（MMIO の閾値）、PMA、SYSTIMER をどこまで模しているか。ROM は本物の `esp32p4-eco2-20240710` 相当か。bare-metal の ELF を直接起動できるか。
2. Wokwi の CI のトークンと料金体系の詳細（CLI の文書は読んでいない）。
3. QEMU ≥10.2 で `pmp-granularity=128` と `num-pmp-regions=16` が `rv32` CPU で期待どおりに動くか（ソースにプロパティがあることだけ確認）。
4. Espressif QEMU の esp32c3 で、`-serial stdio` に UART の出力が出ない原因（`-serial file:` では出る）。
5. Espressif の QEMU で misa が `0x401411ad` になる理由（`esp_cpu.c:410` の `set_misa` との食い違い）。
6. Espressif QEMU の P4 対応の時期（#127 では示されていない）。C6 の PMA CSR などの変更が P4 の下地になるか。
7. Renode の `CoreLocalInterruptController` で、v1.x の非標準 CLIC をどの程度再現できるか。
8. `tab5_claude_client` の `make qemu` が実際にどう失敗するか（ソースからの推測のみ）。

## 7. 参考文献
- espressif/qemu: https://github.com/espressif/qemu（`esp-develop` HEAD `febae182e1`、2026-10-29 取得）。`target/riscv/esp_cpu.c:352-367,410,429-438`、`hw/riscv/esp32c3.c:124-137,267-321,399-483`、`hw/misc/esp32c3_jtag.c:19-30`、`hw/char/esp32_uart.c:134-139,213-232`
- espressif/qemu のリリース: https://github.com/espressif/qemu/releases/tag/esp-develop-9.2.2-20260417 、`esp-develop-9.2.2-20250817`（ローカルにインストールしたもの）
- espressif/qemu#127: https://github.com/espressif/qemu/issues/127
- Espressif QEMU の対応機能表: https://github.com/espressif/esp-toolchain-docs/blob/main/qemu/README.md
- ESP-IDF 6.0: `~/esp-idf/6.0/tools/idf_py_actions/qemu_ext.py:30-34,54-139,233-237,287-305`、`~/esp-idf/6.0/tools/tools.json:1011-1012`（`v6.0-beta1-624-g6534932340`）
- 参考プロジェクト: `~/repos/tab5_claude_client/CLAUDE.md:30-38`、`Makefile:30-33,68-82`
- 上流 QEMU: https://github.com/qemu/qemu 、`target/riscv/cpu.c`（v10.1.0 l.1604,2608、v10.2.0 l.1642,2647、master l.1546-1548,3053-3055）、`hw/intc/`
- Tock: https://github.com/tock/tock/tree/master/boards/qemu_rv32_virt （`Makefile:10-12`）、`chips/qemu_rv32_virt_chip/src/chip.rs:25-27`
- Renode: https://github.com/renode/renode （`platforms/cpus/`）、https://github.com/renode/renode-infrastructure （`src/Emulator/Cores/RiscV/CoreLocalInterruptController.cs`）、https://git.xobs.io/xobs/renode-esp32s3
- Wokwi: https://docs.wokwi.com/guides/esp32 、https://docs.wokwi.com/wokwi-ci/getting-started 、ESP-IDF の Wokwi ガイド https://docs.espressif.com/projects/esp-idf/en/v6.1/esp32p4/third-party-tools/wokwi.html
- Xous: https://github.com/betrusted-io/xous-core （README「Emulation」、`kernel/src/arch/hosted/`）
- 実行ログ: scratchpad の `emu/run_virt.log`、`run_virt_p4like.log`、`c3_uart0.txt`、`qlog.txt`、`gdb_virt.log`、`gdb_c3.log`
