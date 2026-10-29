# 04. アプリの動的ロード方式とアプリ／システムコール ABI

調査日: 2026-10-29
対象: M5Stack Tab5（ESP32-P4、RV32IMAFC、Flash 16MB、PSRAM 32MB）。カーネルは M モード、アプリは U モードで PMP により分離する。

> 表記: 「（推測）」は出典で裏付けていない設計判断や予想。出典のない事実主張はしない方針で書いている。
> クローンしたソースのコミット（いずれも `git clone --depth 1`、2026-10-29 取得）:
> tock `d4979a4c09b6`, libtock-rs `b7f17591e436`, tockloader `914a47dbcaf3`, esp-hal `5c0820cf9e2b`, embassy `c2413d5994bb`, esp-iot-solution `e016cb9e5221`, zephyr `5d2ed837a8c9`。
> GitHub への参照は、このコミットに固定したパーマリンクで書いている。

---

## 0. 結論（先に要点）

### 推奨ロード方式
**「静的 PIE（R_RISCV_RELATIVE だけに絞る）＋ホスト側で変換した独自コンテナ」をカーネルが PSRAM（またはホット用に L2MEM）へまるごとコピーし、ロード時に RELATIVE 再配置を当てる方式**を第一候補とする。

- 実行時のカーネルは ELF をパースしない。ホストツール（`elf2tab` に相当する `elf2t5a`。名前は仮）が ELF を検査・変換する。このとき「動的再配置が `R_RISCV_RELATIVE` 以外を含まない」「`.dynsym` や `PT_INTERP` がない」「GP 緩和が使われていない」を検証し、平坦イメージ＋再配置テーブルを出力する。
- 固定アドレスリンクと MMU リマップの組み合わせ（方式 3）は採らない。ESP32-P4 の外部メモリ MMU はグローバル（プロセスごとのアドレス空間がない）でページが 64KB と粗い。プロセスごとに同じ仮想アドレスを使うと、コンテキストスイッチのたびに MMU の張り替えとキャッシュ同期が要る。アプリごとに別の仮想アドレス窓を割り当てる場合も、結局インストール時に配置先を決めることになり、それは再配置と変わらない（推測）。この方式は、大きな読み取り専用アセットを flash から XIP する用途に限って将来検討する。
- Tock 流の「固定アドレスに複数バリアントをリンクする」方式（libtock-rs の現状）は避ける。理由は、PSRAM が 32MB あってコピーロードのコストが小さいこと、そしてバリアント管理が面倒なこと。

### 推奨アプリフォーマット
**TBF（Tock Binary Format）を手本にした独自コンテナ「T5A」（仮称）**。

- 固定長ベースヘッダ（magic, header_version, header_size, total_size, flags, checksum）を置き、その後に TLV を並べる。TLV の内訳は Program（entry オフセット、binary_end、最小 RAM、スタック/ヒープ要求）、アプリ ID と名前、バージョン、要求する ABI バージョン、パーミッション（使ってよい syscall／ドライバ）、再配置テーブルの位置。
- 本体はセグメント（text/rodata/data）を連結した平坦イメージ。
- フッタはクレデンシャル（Ed25519 署名。必要なら ECDSA P-256 も）。TBF の Program ヘッダとクレデンシャルフッタの構成をほぼそのまま借りる。
- 開発中のデバッグ用には生 ELF を直接ロードするパスを残してもよい。no_std の `object`（read_core+elf）や `xmas-elf` で PT_LOAD を読む。

### syscall ABI 案の要点
- 呼び出しは `ecall`。**a7 = syscall 番号、a0–a5 = 引数、戻り値は a0（状態）と a1–a3（値）**。Linux RISC-V 系のレジスタ配置を採り、戻り値の形式は Tock の「Success/Failure バリアント」を簡略化したもの（a0 = 0 なら成功、負なら `-Error`）。
- カーネルのオブジェクトは**プロセスごとのハンドル表**（ケーパビリティ）で指す。
- ユーザポインタは「プロセス所有領域に完全に含まれるか」をオーバーフロー込みで**ソフトウェアで事前検査してから**カーネルバッファにコピーする（Zephyr の `K_SYSCALL_MEMORY_*` と `k_usermode_from_copy` に相当）。「とりあえず読んで、例外をフィックスアップで拾う」方式は使わない。Zephyr によれば一部の ESP RISC-V SoC ではロードのアクセス例外が不正確（mepc が実際の命令からずれる）で、フィックスアップが効かないため（§4.3）。
- **Zephyr との比較**: Zephyr のレジスタ割当は t0 = 番号、a0–a5 = 引数、戻り値は a0 のみ（§4.3）。本案（a7 = 番号）とは番号レジスタが違うが、引数 6 本は同じ。設計面では、Zephyr の「検証関数（`z_vrfy_*`）と実装関数（`z_impl_*`）を分け、ビルドツールでスタブ・ディスパッチ表を生成する」構成をそのまま借りる。Rust では proc-macro か build.rs で、syscall 定義表から U 側スタブと M 側ディスパッチを生成する。
- 非同期処理は**アップコールを使わず、イベントキューを待つ方式**（`wait(handles, timeout)` がイベント構造体を返す）を基本にする。ユーザスタックへカーネルがフレームを積む Tock 式アップコールは採らない。
- ABI バージョンはアプリヘッダの TLV と `sys_info()` の両方で照合する。syscall 番号は追加のみで、番号の振り直しはしない。

### 重大な注意（両文書に共通）
- **esp-hal の ESP32-P4 サポートは「Revisions below v3.0 are not supported.」と明記されている**（[esp-hal soc.toml L15](https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-metadata/devices/esp32p4/soc.toml#L15)）。一方、同じ作者の既存 Tab5 プロジェクトは `CONFIG_ESP32P4_SELECTS_REV_LESS_V3=y`、`CONFIG_ESP32P4_REV_MIN_FULL=100`、`REV_MAX_FULL=199` でビルドされている（`~/repos/tab5_claude_client/sdkconfig:2040-2055`）。つまり**手元の Tab5 は P4 v1.x である可能性が高い**。ESP-IDF の Kconfig ヘルプにも「v3.0 以上と v3.0 未満はハードウェア差が大きく互換性がない」とある（`~/esp-idf/6.0/components/esp_hw_support/port/esp32p4/Kconfig.hw_support:4-11`）。この点は OS 全体の前提を左右するので、実機の chip revision を最優先で確認すること（実機確認は本調査の範囲外）。
- **Zephyr も P4 rev 1.3 では PMP（したがって userspace）を無効にしている**。`select RISCV_PMP if SOC_ESP32P4_HPCORE && !MCUBOOT && !SOC_ESP32P4_REV_1_3`（[zephyr soc/espressif/esp32p4/Kconfig L7](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/soc/espressif/esp32p4/Kconfig#L7)）。コミット [737eda3f5cae](https://github.com/zephyrproject-rtos/zephyr/commit/737eda3f5cae)（2026-10-02）の説明は「rev 1.3 は mintthresh CSR を持たない」。ESP-IDF も「P4 は REV2 以降で標準 CLIC（mintthresh）」としている（`~/esp-idf/6.0/components/riscv/include/riscv/csr_clic.h:38-42`）。Tab5 が v1.x なら、**Zephyr が P4 の U モードで使っている回避策（syscall 処理中は mintthresh で割り込みを止める）がそのままでは使えない**。詳しくは §4.3。

---

## 1. 前提となる ESP32-P4 のメモリ事情

| 項目 | 値 | 出典 |
|---|---|---|
| 外部メモリ MMU のページサイズ | 0x10000（64KB） | `~/esp-idf/6.0/components/soc/esp32p4/include/soc/ext_mem_defs.h:19`、[esp-hal soc.toml L17-19](https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-metadata/devices/esp32p4/soc.toml#L17-L19)（entry_num=1024） |
| Flash の仮想アドレス窓 | 0x4000_0000–0x4400_0000（I/D 共用） | `ext_mem_defs.h:28-38` |
| PSRAM の仮想アドレス窓 | 0x4800_0000–0x4C00_0000 | `ext_mem_defs.h` の `SOC_IRAM_PSRAM_ADDRESS_LOW/HIGH`（ファイル内 26-30 行目付近） |
| MMU の構成 | 外部メモリの種類（flash, psram）ごとに MMU がある | `soc_caps.h:417` `SOC_MMU_PER_EXT_MEM_TARGET` |
| PMP の粒度 | 128 バイト | `soc_caps.h:208` `SOC_CPU_PMP_REGION_GRANULARITY 128` |
| PMA の有無 | あり | `soc_caps.h:206` |
| FPU と既知の不具合 | FPU あり。ただし「EXT_ILL CSR doesn't support FLW/FSW」 | `soc_caps.h:193-194` |
| 独自拡張 | HWLOOP、PIE（SIMD）がある。HWLOOP の状態遷移にも不具合がある | `soc_caps.h:195-198` |

ロード方式への影響（推測）:
- 128B の PMP 粒度は、セグメント境界を 128B に揃えれば足りる。MMU の 64KB 粒度より細かく分離できるので、**アプリ間の分離は PMP だけで行う**のが自然。
- FPU 状態の遅延退避を `mstatus.FS` で行う場合、P4 には上記の FPU 例外不具合があるので注意が要る。U モードのアプリが FPU を使う前提で、F レジスタは常に退避するのが安全。HWLOOP と PIE はコンパイラが自動生成しないので（推測）、ABI では「アプリ使用禁止」とし、カーネルは状態を保存しない。

---

## 2. ロード方式の比較

### 2.1 比較表

| 方式 | 仕組み | ロード時の処理 | RISC-V + Rust での実情 | 長所 | 短所 | 評価 |
|---|---|---|---|---|---|---|
| (1) PIC/PIE（GOT＋PC 相対） | `-C relocation-model=pie` でコンパイルし、lld で `-pie` リンク。コードは `auipc` による PC 相対 | `.rela.dyn` の `R_RISCV_RELATIVE` を当てる（base を加算） | 組み込みターゲットの既定は `RelocModel::Static`（[riscv32imafc target spec](https://doc.rust-lang.org/nightly/nightly-rustc/src/rustc_target/spec/targets/riscv32imafc_unknown_none_elf.rs.html)）。事前ビルド済みの core/alloc は PIE ではないので `-Zbuild-std` で再ビルドが要る（推測。§2.2） | どこにでも置ける。再配置は 1 種類だけでよい | 静的 PIE の情報が少なく、ツールで検証する仕組みが必要 | ◎ 推奨 |
| (2) ロード時の汎用再配置 | `--emit-relocs` や `-r`（再配置可能オブジェクト）を出し、`R_RISCV_HI20/LO12/PCREL_*/CALL` などを全部当てる | HI20/LO12 の対応付けなど複雑。シンボル解決も要る | Zephyr LLEXT と NuttX が実装済みで、どちらも「HI20/LO12 が隣接しない」問題で苦労している（§3.2, §3.5） | 非 PIC のコードもロードできる | ローダが複雑で攻撃面が広い | △ |
| (3) 固定仮想アドレス＋MMU | アプリを固定 VA にリンクし、flash/PSRAM のページを MMU でその VA に張る | MMU エントリの設定だけ（再配置なし） | P4 の MMU はグローバルで 64KB ページ（§1）。プロセスごとの空間は作れない | XIP できる。再配置が不要 | 全アプリが同じ VA を使うならスイッチのたびに MMU 張り替えとキャッシュ同期が要る。別 VA にするなら配置をインストール時に決めることになる（推測） | △（大きなアセットの XIP 用途に限る） |
| (参考) Zephyr userspace＋LLEXT | カーネルと U スレッドは 1 つのイメージに静的リンクし、PMP でメモリドメインを分ける。後からロードするコードは LLEXT（ELF の .o/部分リンク/.so）を汎用再配置し、`llext_add_domain` で U 用ドメインに加える | 汎用再配置（RISC-V では 30 種超の `R_RISCV_*` を処理）＋エクスポート表による名前解決 | RISC-V 対応済み。P4 rev 3.x は userspace 可、rev 1.3 は PMP 無効（§4.3） | U モードと PMP の組み合わせを実機で運用している最も近い先行例 | 「ロードしたコードはカーネルオブジェクトや syscall を定義できない」などの制約がある。脅威モデルはビルド全体を信頼する前提（§4.3） | 設計を借りる（検証関数、メモリドメイン、syscall 生成）。ローダ方式は借りない |
| (4) Tock 方式 | TBF に入れる。PIC が使えないアーキテクチャでは複数アドレスでリンクしたバリアントを `.tab` に同梱し、tockloader が合うものを選ぶ | 固定アドレス版はロード処理なし。PIC 版は libtock-c（ARM）の独自再配置 | libtock-rs は「apps must be compiled for the flash and RAM address they are executed at」（[README L10-14](https://github.com/tock/libtock-rs/blob/b7f17591e43607474958ed7d117ba831e54e1017/README.md?plain=1#L10-L14)） | 実績がある。フォーマット（TBF）は流用できる | バリアントが増える。配置の自由度が低い | フォーマットだけ採用 |

### 2.2 静的 PIE を Rust で作る際の論点

- **ターゲットの既定**: `riscv32imafc-unknown-none-elf` は Tier 2 で、`linker = rust-lld`、`relocation_model = Static`、`llvm_abiname = ilp32f`、`features = +m,+a,+c,+f`（[rustc target spec](https://doc.rust-lang.org/nightly/nightly-rustc/src/rustc_target/spec/targets/riscv32imafc_unknown_none_elf.rs.html)）。P4 の HP コアはこのターゲットを使う（[esp-hal soc.toml L11](https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-metadata/devices/esp32p4/soc.toml#L11)）。
- **非 PIC のオブジェクトが混ざると失敗する**: `-pie` のリンクに絶対アドレス参照の `R_RISCV_HI20` が混ざると、lld は「relocation R_RISCV_HI20 cannot be used against symbol …; recompile with -fPIC」で失敗する（Linux カーネルでの事例: [LKML 2306.1/02149](https://lkml.rescloud.iu.edu/2306.1/02149.html)）。したがってアプリ側は `-Zbuild-std=core,alloc` と `-C relocation-model=pie` で、core/alloc/compiler_builtins を含めて全部ビルドし直す必要がある（推測。実証は未確認）。
- **生成される動的再配置の種類**: lld は PIC 出力で、プリエンプトされないシンボルへの `R_RISCV_32` を `R_RISCV_RELATIVE` に変換する（[lld test riscv32-reloc-32-pic.s](https://llvm.googlesource.com/lld/+/refs/heads/master/test/ELF/riscv32-reloc-32-pic.s)）。OpenSBI の議論では「static-pie に対して Linux/musl/glibc が処理するのは R_RISCV_RELATIVE（と ifunc 用の IRELATIVE）だけ」とされ、`.dynsym` を出さないために `--exclude-libs ALL` を使っている。同じ議論で、RV32 では `.rela.dyn` の ALIGN 指定が原因で約半数のバイナリが起動しない不具合も報告されている（[OpenSBI ML 2023-10](https://lists.infradead.org/pipermail/opensbi/2023-October/005654.html)）。→ **リンカスクリプトの `.rela.dyn` 周りに注意し、ホストツールで RELATIVE 以外を拒否する**。
- **GP 緩和**: lld の GP 緩和（`.sdata/.sbss` を gp 相対に書き換える）は**既定で無効**で、`--relax-gp` を付けたときだけ有効になる（[LLVM commit 85444794](https://llvm.googlesource.com/llvm-project/+/85444794cdde8211b916767cc51a37547dbd17e9)）。GNU ld は `-no-pie` のとき GP 緩和を行い、binutils 2.41 で `--no-relax-gp` が加わった（[MaskRay: The dark side of RISC-V linker relaxation](https://maskray.me/blog/the-dark-side-of-riscv-linker-relaxation)）。→ アプリは rust-lld（既定）でリンクし、`--relax-gp` は使わない。そうすれば gp は ABI 上「未使用（カーネル・アプリとも任意）」として扱える。カーネルは trap のたびに gp/tp を含む全 GPR を保存・復元する（推測）。
- **text と data の相対位置**: PIE の text は data/GOT を PC 相対で参照するので、ロード後も**text と data の相対オフセットはリンク時と同じでなければならない**。text だけを flash に XIP し、data を RAM に置くと、この関係が崩れる。ARM には r9 を使った RWPI があるが、LLVM の RISC-V に対応する仕組みはない（推測）。→ **イメージ全体を連続した領域（PSRAM）へコピーする**。
  - 応用（推測・未検証）: flash の窓（0x40..）と PSRAM の窓（0x48..）の差は固定の 0x0800_0000 なので、「data を text + 0x0800_0000 に置く」ようリンクすれば、text は flash から XIP、data は PSRAM に置くことも原理的にはできる。MMU の張り方に制約が付くので v1 では採らない。
- **キャッシュ同期**: PSRAM にコード書き込んだあとは、D キャッシュの書き戻しと I キャッシュの無効化が要る（推測。ESP-IDF の `elf_loader` も PSRAM から実行する構成をとっている。§3.3）。

### 2.3 ロード手順（提案）

1. アプリストア（文書 05 を参照）から T5A ヘッダを読み、署名を検証し、ABI バージョンとパーミッションを照合する。
2. PSRAM から `text+rodata+data+bss+heap+stack`（ヘッダで要求されたサイズ）を 128B（PMP の粒度）以上の境界で確保する。実装を簡単にするなら 4KB 境界（推測）。
3. 本体を確保領域の先頭にコピーし、bss をゼロで埋める。
4. 再配置テーブル（RELATIVE のオフセット列）を走査して `*(base+off) += base` を行う。オフセットが data/GOT の範囲内にあることを必ず検査する。
5. D キャッシュを書き戻し、I キャッシュを無効化する。
6. PMP を設定する。text は R+X、rodata は R、data/bss/heap/stack は R+W とし、W^X を守る。TOR エントリを 4〜5 個使う。Tock の RISC-V PMP 実装（TOR ベースの `TORUserPMP`、[pmp.rs L880](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/arch/riscv/src/pmp.rs#L880)）が参考になる。
7. `mepc = base + entry`、`sp = stack_top`、a0 = プロセス情報ブロックのアドレス（またはブートストラップ用ハンドル）、`mstatus.MPP = U` として `mret` する。

### 2.4 no_std の ELF パーサ（開発用・ホストツール用）

| クレート | no_std | alloc | 備考 | 出典 |
|---|---|---|---|---|
| `object` 0.40 | ○（`default-features=false, features=["read_core","elf"]`） | 不要（`alloc` は任意機能） | ELF32 の PT_LOAD、再配置、動的セクションを読める | [docs.rs features](https://docs.rs/crate/object/latest/features) |
| `goblin` 0.10.7 | 制限付き | 既定機能の `std`/`endian_fd` は `alloc` に依存する | `Elf::parse` を no_std・alloc なしで使えるかは要確認 | [docs.rs features](https://docs.rs/crate/goblin/latest/features) |
| `xmas-elf` | ○（とされる） | 不要（とされる） | 軽量。どこまで保守されているかは未確認 | 未確認 |

推奨（推測）: **ホストツール（std）は `object` で書く**。カーネル内に ELF を直接ロードする開発用パスを入れるなら、同じ `object` を `read_core` で使う。本番のカーネルは T5A だけを読む。

---

## 3. 既存のフォーマット／ローダ

### 3.1 Tock Binary Format（TBF）

構造（`libraries/tock-tbf/src/types.rs`）:
- **ベースヘッダ** `TbfHeaderV2Base { version:u16, header_size:u16, total_size:u32, flags:u32, checksum:u32 }`（[types.rs L114-120](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/libraries/tock-tbf/src/types.rs#L114-L120)）。checksum はヘッダの 4 バイト語ごとの XOR で、checksum フィールド自身は除く（[parse.rs L79-101](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/libraries/tock-tbf/src/parse.rs#L79-L101)）。
- **TLV 型**: Main=1、WriteableFlashRegions=2、PackageName=3、FixedAddresses=5、Permissions=6、StoragePermissions=7、KernelVersion=8、Program=9、ShortId=10、FooterCredentials=128。知らない型は長さを見て読み飛ばす（[types.rs L124-140](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/libraries/tock-tbf/src/types.rs#L124-L140)）。
- **Main** は `init_fn_offset, protected_trailer_size, minimum_ram_size`。**Program** はそれに `binary_end_offset, version` を加えたもの。Program ヘッダがあるときだけ、binary_end から TBF 末尾までをクレデンシャルフッタとして使える（[types.rs L149-179](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/libraries/tock-tbf/src/types.rs#L149-L179)）。
- **FixedAddresses**: `start_process_ram`、`start_process_flash`。このヘッダがなければ PIC として扱う（[types.rs L181-205](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/libraries/tock-tbf/src/types.rs#L181-L205)）。
- **Permissions**: ドライバ番号ごとに、許可するコマンドのビットマップ（u64）とオフセットを持つ（同ファイル `TbfHeaderDriverPermission`）。
- **クレデンシャル型**: Rsa3072Key=1、Rsa4096Key=2、SHA256=3、SHA384=4、SHA512=5、EcdsaNistP256=6（[types.rs L254-262](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/libraries/tock-tbf/src/types.rs#L254-L262)）。
- tockloader は、全アプリが固定アドレスかどうかで配置戦略を切り替える（[tockloader.py L1376-1378](https://github.com/tock/tockloader/blob/914a47dbcaf3690394d3fc338fdfea376cf43ed2/tockloader/tockloader.py#L1376-L1378)、[app_installed.py L88-120](https://github.com/tock/tockloader/blob/914a47dbcaf3690394d3fc338fdfea376cf43ed2/tockloader/app_installed.py#L88-L120)）。
- **動的プロセスロード**: Tock には実行時にアプリを追加する仕組み（`setup/write/load/abort`）を定めた TRD がある（[trd-dynamic-process-loading.md L26-62, L108-135](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd-dynamic-process-loading.md)）。非 XIP 環境（RAM へロードして実行）についての Draft TRD もある（[trd-non-xip.md](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd-non-xip.md)）。

**T5A への取り込み方（推測）**: ベースヘッダ、TLV、Program ヘッダ、クレデンシャルフッタの考え方はほぼそのまま使う。追加するのは次の 3 つの TLV。
- `Relocations`（RELATIVE 再配置のオフセット列の位置と個数）
- `MemoryRequest`（スタック、ヒープ、最大ヒープ）
- `AbiVersion`（major/minor。Tock の KernelVersion に当たる）

checksum は XOR ではなく CRC32 か SHA-256 にする。真正性は署名で担保する。

### 3.2 Zephyr LLEXT
- RISC-V 用の再配置は psABI に沿って実装されている。「HI20/LO12 が隣接しない」ケースに対応するため `arch_elf_relocate` に引数が追加された。最初の実装時点では、ユーザスレッドから llext を呼ぶことは RISC-V では未サポートだった（[Zephyr commit 5275d444](https://pigweed.googlesource.com/third_party/github/zephyrproject-rtos/zephyr/+/5275d4440953265ff793b0030c03f04fa21a76ab)）。
- 位置付け: カーネル拡張（同じ特権レベルで動く）に近く、U モードアプリのローダとしての参考度は中程度（推測）。

### 3.3 ESP-IDF `elf_loader`（espressif/elf_loader, esp-iot-solution）
- 2.0.0 で **ESP32-P4 をサポート**しており、PSRAM からの実行もできる。PMP による IDRAM 分割があるターゲットで内部 SRAM にロードするには `CONFIG_ESP_SYSTEM_PMP_IDRAM_SPLIT=n` が要るため、`ELF_LOADER_LOAD_PSRAM` が推奨されている（[Component Registry](https://components.espressif.com/components/espressif/elf_loader)）。
- アプリは `-nostdlib -fPIC -shared` でビルドする（[elf_loader.cmake L21-23](https://github.com/espressif/esp-iot-solution/blob/e016cb9e5221f8016ff6371d3babdda15f4934a7/components/elf_loader/elf_loader.cmake#L21-L23)）。RISC-V で処理する再配置は `R_RISCV_NONE/32/RELATIVE/JUMP_SLOT` の 4 種類だけ（[esp_elf_riscv.c L106-114](https://github.com/espressif/esp-iot-solution/blob/e016cb9e5221f8016ff6371d3babdda15f4934a7/components/elf_loader/src/arch/esp_elf_riscv.c#L106-L114)）。
- 示唆: **「shared/PIE にすれば RISC-V でも少数の動的再配置で足りる」ことの実例**になる。ただしシンボルを解決するため（`JUMP_SLOT` や `R_RISCV_32` でカーネル側の関数を直接呼ぶ）、特権分離は考えていない。本 OS では、アプリが外部シンボルを持たないこと（未定義シンボル 0）をホストツールで強制する（推測）。

### 3.4 MicroPython native .mpy
- アーキテクチャとして `rv32imc` がある。ネイティブコードは「position-independent code with a global offset table」でビルドし、`mpy_ld.py` でリンクする（[MicroPython natmod docs](https://docs.micropython.org/en/v1.24.0/_sources/develop/natmod.rst.txt)、[emlearn native modules](https://emlearn-micropython.readthedocs.io/en/latest/native_modules.html)）。GOT 方式で RISC-V の PIC を運用している例として参考になる。

### 3.5 NuttX ELF / プロテクトビルド
- RISC-V の ELF ローダでは、HI20/LO12 の対を「隣接している」と仮定していたことによるバグを、HI20 のキャッシュを持たせて修正している（[NuttX commit 996625e](https://apache.googlesource.com/nuttx/+/996625ec5822b01a9a4197f0ab73c161f437bc6a)）。汎用再配置ローダ（方式 2）の落とし穴を示す例。
- プロテクトビルドは、MPU で特権カーネルと非特権アプリを分け、システムコールでつなぐ（[NuttX Protected Build](https://nuttx.apache.org/docs/latest/guides/protected_build.html)）。本 OS と構成が近い。
- NuttX は ESP32-P4 で MCUboot を使う構成を持つ（[NuttX ESP32-P4](https://nuttx.apache.org/docs/latest/platforms/risc-v/esp32p4/index.html)）。詳細は文書 05 を参照。

### 3.6 その他
- Hubris（Oxide）はビルド時に全タスクを固定アドレスでリンクし、動的ロードはしない設計とされる（未確認。参考程度）。
- Apache Mynewt の split image なども参考候補に挙がったが、今回は未調査。

---

## 4. syscall ABI 案

### 4.1 参照: Tock の syscall（TRD104）

| 項目 | 内容 | 出典 |
|---|---|---|
| レジスタ（RISC-V） | 引数 a0–a3、戻り値 a0–a3、**クラス ID は a4**、アップコールの引数 a0–a3 | [TRD104 L93-111](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md#L93-L111) |
| 戻り値のバリアント | a0 = 0..3 が Failure 系（a1 にエラーコード）、128..133 が Success 系（u32、2×u32、u64、3×u32、u32+u64）。1 つの syscall が返す成功・失敗のバリアントはそれぞれ 1 種類だけ | [L207-270](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md#L207-L270) |
| エラーコード | 1 FAIL、2 BUSY、3 ALREADY、4 OFF、5 RESERVE、6 INVALID、7 SIZE、8 CANCEL、9 NOMEM、10 NOSUPPORT、11 NODEVICE、12 UNINSTALLED、13 NOACK、1024 BADRVAL | [L272-306](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md#L272-L306) |
| クラス | 0 Yield、1 Subscribe、2 Command、3 RW Allow、4 RO Allow、5 Memop、6 Exit。ブロックするのは Yield と Exit だけ | [L353-404](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md#L353-L404) |
| Yield | 0 no-wait（a1 が指す 1 バイトに、アップコールが実行されたかを書く）、1 wait、2 wait-for（driver/subscribe を指定。コールバックは呼ばず、a0–a2 で引数を返す）。アップコールが起きるのは Yield の中だけ | [L406-539](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md#L406-L539) |
| Subscribe | (driver, subscribe 番号, upcall ポインタ, app data)。成功すると以前の upcall/data を返す。保留中のアップコールは破棄する。Null Upcall は 0 | [L541-647](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md#L541-L647) |
| Command | (driver, command 番号, arg0, arg1)。command 0 はドライバの存在確認 | [L649-688](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md#L649-L688) |
| Allow RW/RO | (driver, allow 番号, ptr, len)。カーネルはバッファがプロセスの書き込み可能領域に完全に含まれることを検査し、外れていれば INVALID。再度 allow すると前のバッファを返す。「バッファは途中で変わりうる」前提で実装すること | [L689-895](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md#L689-L895) |
| Memop | 0 brk、1 sbrk、2–9 は各種アドレスの取得、10/11 はスタックとヒープ開始位置の通知 | [L896-931](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md#L896-L931) |
| Exit | 0 terminate、1 restart。a1 に完了コード | [L933-985](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md#L933-L985) |

カーネル側でのバッファ検査の実装例は `ProcessStandard::build_readwrite_process_buffer` と `in_app_owned_memory`（[process_standard.rs L1070-1120](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/kernel/src/process_standard.rs#L1070-L1120)）。libtock-rs の RISC-V 用 syscall スタブは `ecall` でクラスを a4 に置き、t0–t6、a0–a7、ra を clobber として宣言する。**F レジスタは clobber に入れていない**（[syscalls_impl_riscv.rs L1-35](https://github.com/tock/libtock-rs/blob/b7f17591e43607474958ed7d117ba831e54e1017/runtime/src/syscalls_impl_riscv.rs#L1-L35)）。したがってカーネルは F レジスタを保存しなければならない。

### 4.2 本 OS 向け ABI 案（v0 ドラフト、推測を含む設計案）

**呼び出し規約**

| レジスタ | 入力 | 出力 |
|---|---|---|
| a7 | syscall 番号（下位 16 ビット = 関数番号、上位 = 予約 0） | 破壊 |
| a0–a5 | 引数 0–5 | a0 = 状態（`0` は成功、`<0` は `-Error`）、a1–a3 = 戻り値 |
| その他 | 呼び出し側保存（ra, t*, a*）はすべて破壊されうるものとする。s*、sp、gp、tp、F レジスタは保存される | |

- Linux RISC-V に合わせて a7 を番号に使う理由（推測）: 引数を 6 本取れること、既存の `syscalls` 系クレートやツール（strace 風のトレーサ）の慣習に合うこと。Tock 式（a4 にクラス、引数 4 本）は呼び出しが単純な代わりに、大きな構造体を渡すには毎回 allow が要る。
- 戻り値は Tock と違ってバリアント番号を持たない。「a0 < 0 ならエラー」の 1 本に統一し、Rust 側で `Result<[usize;3], Error>` に包む。エラーコードの値は Tock の表（1–13）を流用すると互換性の説明がしやすい。

**syscall の分類（初版の候補）**

| 番号帯 | 系統 | 例 |
|---|---|---|
| 0x000– | プロセス | `exit(code)`、`yield()`、`sleep(ns)`、`sys_info(buf)`（ABI バージョン、カーネル版、時刻源） |
| 0x040– | メモリ | `brk(addr)`、`mem_info()`（Tock の memop 相当） |
| 0x080– | ハンドル | `close(h)`、`dup(h, rights)`、`wait(handles_ptr, n, timeout, ev_out)` |
| 0x0C0– | IPC | `chan_create()`、`chan_send(h, buf, len)`、`chan_recv(h, buf, len)` |
| 0x100– | デバイス | `open(name_ptr, len, rights) -> h`、`ctl(h, op, a, b)`、`read(h, buf, len)`、`write(h, buf, len)` |
| 0x140– | 共有メモリ | `shm_map(h) -> addr`（フレームバッファなどの大きなバッファを PMP 領域としてアプリへ割り当てる） |
| 0xF00– | デバッグ | `dbg_write(buf, len)` |

**ユーザポインタの検査**
- 必須: `ptr.checked_add(len)` でオーバーフローを確認する。範囲 `[ptr, ptr+len)` がプロセスのある領域（text 読み出し専用、data/heap/stack 読み書き）に**完全に含まれる**こと。アラインメント。len 0 は特別扱い（Tock と同じく任意アドレスを許す）。
- 二重フェッチを防ぐ: 検査したあとカーネルのバッファへコピーしてから使う（copy-in/copy-out）。Tock の「Buffers Can Change」（[TRD104 L767-832](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md#L767-L832)）と同じ問題で、マルチコアの P4 では、もう片方のコアで動くアプリスレッドが同時に書き換えうる（推測）。
- MPRV について: M モードで `mstatus.MPRV=1, MPP=U` にしてからユーザメモリにアクセスすると、ロード／ストアに U モードの PMP 権限が適用される（RISC-V Privileged ISA の MPRV の仕様）。ただし Zephyr の記述では、ESP 系の一部 SoC でロードのアクセス例外が不正確（mepc がずれる）なので、「アクセスして例外をフィックスアップで拾う」設計は成り立たない（`RISCV_USER_STRING_NLEN_VALIDATE`。§4.3）。**ソフトウェアによる事前検査を必須とし、MPRV は使うとしても多層防御の一つにとどめる**（推測）。

**ハンドル／ケーパビリティ**
- プロセスごとに固定長のハンドル表（例: 64 エントリ）を持つ。各エントリは `{kind, object_ref, rights}`。アプリは整数ハンドルしか知らない。
- `open` を許可するデバイス名は、アプリヘッダの Permissions TLV に書かれた範囲に限る。Tock の TBF Permissions（ドライバ番号と許可コマンドのビットマップ）を名前ベースに置き換えたもの。

**非同期イベント**
- Tock 式アップコール（カーネルがユーザスタックに関数フレームを積む）は、Rust のアプリ側で再入と `Send` の扱いが難しい。Tock でも配送は Yield の中だけで、Yield-WaitFor（コールバックなしでレジスタに値を返す）が追加されている（[TRD104 L507-539](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md#L507-L539)）。
- 本案では **`wait(handles[], timeout) -> Event{handle, kind, value0, value1}`** だけで配送する。各ハンドルは内部にイベントキューを持ち、溢れたら合体させてフラグを立てる。アプリのランタイムでは、これを Embassy 風の async executor のウェイクソースにする（推測）。
- 大量のデータ（入力イベント、ネットワーク）はチャネル（カーネルが持つリングバッファ）へ copy して渡す。

**バージョニング**
- `AbiVersion{major, minor}` をアプリヘッダの TLV に入れる。カーネルは major が一致し、かつ minor ≤ カーネルの minor のときだけ起動する。
- syscall 番号は追加だけにする。意味を変えたいときは新しい番号を割り当て、古い番号は互換層で残すか、major を上げる。
- `sys_info` で、カーネル版と対応している syscall 集合（ビットマップ）を返す。Tock の「command 0 による存在確認」（[TRD104 L678-688](https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md#L678-L688)）を一般化したもの。

**trap の処理（カーネル側の要点、推測）**
- `mcause = 8`（Environment call from U-mode）なら `mepc += 4` して振り分ける。`ecall` は圧縮命令にならないので常に 4 バイト。
- 全 GPR（gp、tp を含む）と F レジスタ、fcsr を保存する。P4 の FPU 例外の不具合（§1）があるので、遅延退避は慎重に検討する。

### 4.3 Zephyr の特権分離（CONFIG_USERSPACE）とシステムコール — 最も近い先行例

Zephyr は C で書かれているが、**PMP だけを持つ RISC-V MCU で M/U 分離を実運用しており、ESP32-P4 の HP コアも対象にしている**。本 OS にとって最も近い先行例なので、仕組みを細かく見る。ソースは zephyr `5d2ed837a8c9`（2026-10-29 の main）。

#### 4.3.1 脅威モデルとモデルの違い
- Zephyr の userspace は**スレッド単位**の権限モデル。カーネル、ビルド、ツールチェーン、`zephyr.elf` を作るすべての C ファイルを信頼し、ロードしたコードは「検証済みでないなら最初から U モードで入れるべき」とする（[overview.rst L90-125](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/kernel/usermode/overview.rst#L90-L125)）。
- 既定ではプログラムテキストと読み出し専用データが**全スレッドから読める**（[overview.rst L59-60](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/kernel/usermode/overview.rst#L59-L60)）。PMP の初期化でも `__rom_region` を全モードに R+X で開けている（[pmp.c L768-822](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/core/pmp.c#L768-L822)）。
- 制約: U から使うカーネルオブジェクト、スタック、ドライバはビルド時に定義しなければならない。後からロードしたコードは、カーネルオブジェクトも、割り込みハンドラも、syscall も定義できない（[overview.rst L171-195](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/kernel/usermode/overview.rst#L171-L195)）。
- 本 OS との違い（推測）: 本 OS は「信頼できないアプリをプロセス単位で動的に入れる」前提。したがって text の機密性（他アプリから読めないこと）、プロセス単位のリソース回収、実行時のオブジェクト生成（ハンドル表）が要る。Zephyr のビルド時静的テーブル（gperf）方式はそのままでは使えない。

#### 4.3.2 RISC-V での user mode 実装
- **U モードへの入り方**: `arch_user_mode_enter()` が特権スタックを用意し、`mstatus.MPP = U` を設定して `mret` で U に入る（[thread.c L147-232](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/core/thread.c#L147-L232)）。スレッドごとに特権スタック（`CONFIG_PRIVILEGED_STACK_SIZE`）を持つ。
- **trap の入口**: `mstatus.MPP` を見て U から来たと分かれば、per-CPU に保存しておいた特権スタックに切り替える。続いて tp を現スレッドの TLS に、gp を正しい値に戻す。gp は `.option norelax` 下で再ロードする（[isr.S L193-215](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/core/isr.S#L193-L215)）。→ 本 OS も、U から入ったら **sp、tp、gp をカーネルの値に必ず差し替える**。
- **syscall のディスパッチ**: `is_user_syscall` で PMP をカーネルモード用に切り替え（`z_riscv_pmp_kernelmode_enable`）、**割り込みを再び許可してから** syscall 本体を実行する。mepc を +4 し、ESF から a0–a5 と t0 を取り出す。`t0 >= K_SYSCALL_LIMIT` なら `K_SYSCALL_BAD` に差し替え、`_k_syscall_table[t0]` を呼んで結果を ESF の a0 に書き戻す（[isr.S L621-722](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/core/isr.S#L621-L722)）。つまり **syscall 本体はプリエンプト可能**で、特権スタック上で走る。
- **レジスタ割当（RISC-V）**: `arch_syscall_invoke6` は a0–a5 = 引数 1–6、**t0 = call_id**、戻り値は a0 だけ（[syscall.h L36-58](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/include/zephyr/arch/riscv/syscall.h#L36-L58)）。7 個以上の引数は構造体にまとめて渡し、32 ビット系の 64 ビット引数は 2 レジスタに分ける（[syscalls.rst L200-230](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/kernel/usermode/syscalls.rst#L200-L230)）。M モード内部用の ecall（例外報告、IRQ offload、スケジュール）は別番号で区別している（[syscall.h L20-24](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/include/zephyr/arch/riscv/syscall.h#L20-L24)）。

#### 4.3.3 PMP の割り当て（`arch/riscv/core/pmp.c`）
- **シャドウコピー**: PMP の設定はスレッドごとに M モード用と U モード用を RAM 上に事前計算しておき、コンテキストスイッチではレジスタへ一括で書くだけにする。下位のスロットは変更しないグローバルエントリ（[pmp.c L1-27](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/core/pmp.c#L1-L27)）。
- **予約スロット**: `CONFIG_PMP_DATA_EXECUTION_PREVENTION` を有効にすると、最後のスロットを「全空間 RW のみ（X なし）」に、USERSPACE ではさらに最後から 2 番目を「U ブロッカー」に使う（[pmp.c L62-76](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/core/pmp.c#L62-L76)、[L984-1020](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/core/pmp.c#L984-L1020)）。
- **スタックガード**: `CONFIG_PMP_STACK_GUARD` は IRQ スタック、メインスタック、各スレッドスタックの底に PMP ガードを置く（[pmp.c L824-845](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/core/pmp.c#L824-L845)）。有効にすると `PMP_KERNEL_MODE_DYNAMIC` が選ばれ、M モード側の PMP もスイッチごとに張り替える（[Kconfig L583-620](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/Kconfig#L583-L620)）。ガードが利くように M モード側は MPRV＋catch-all エントリを使う（[pmp.c L469-480](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/core/pmp.c#L469-L480)）。
- **U モード**: ユーザスタックに 1 エントリ（NAPOT）か 2 エントリ（TOR）。そのあとにメモリドメインの各パーティションを 1〜2 エントリずつ書く（[pmp.c L1151-1166](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/core/pmp.c#L1151-L1166)）。
- **エントリが足りないとき**: `arch_mem_domain_max_partitions_get()` は楽観的な見積もり（使えるスロット − グローバル − スタック用）を返す。実際に足りなくなったら、`resync_pmp_domain()` は assert で止めずに、**入る分だけ設定し、残りのパーティションに触れたスレッドだけがフォールトする**（[pmp.c L1230-1262, L1311-1338](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/core/pmp.c#L1230-L1338)）。
- **ユーザバッファの検証** `arch_buffer_validate()` は「自スタック内か」「読み出しならグローバル RO（rom_region と rodata）内か」「自ドメインのパーティションに完全に含まれ、権限が合うか」を判定する（[pmp.c L1385-1450](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/core/pmp.c#L1385-L1450)）。
- **関連 Kconfig**: `PMP_SLOTS` は既定 8（[Kconfig L468-473](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/Kconfig#L468-L473)）、`PMP_GRANULARITY` は既定 4（[L545-552](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/Kconfig#L545-L552)）。`RISCV_PMP` は `ARCH_HAS_USERSPACE` を選ぶ（[L447-455](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/Kconfig#L447-L455)）。

#### 4.3.4 syscall の定義・生成・検証
- 構成要素は 4 つ（[syscalls.rst L24-47](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/kernel/usermode/syscalls.rst#L24-L47)）。
  1. `__syscall` 付きの C プロトタイプ（実体は生成される）
  2. 実装関数 `z_impl_*`（引数は検証済みと仮定してよい）
  3. 検証関数 `z_vrfy_*`（全引数を検証して実装関数を呼ぶ）
  4. 自動生成されるアンマーシャル関数（`*_mrsh.c`）
- `parse_syscalls.py` と `gen_syscalls.py`（`scripts/build/`）が生成するもの（[syscalls.rst L141-170](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/kernel/usermode/syscalls.rst#L141-L170)）。
  - ID の列挙（`syscall_list.h` の `K_SYSCALL_*`）
  - ディスパッチ表 `_k_syscall_table`
  - 未実装用の weak 検証関数（無効化されたサブシステムの syscall が来たときの受け皿）
- 検証マクロ（[syscall_handler.h](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/include/zephyr/internal/syscall_handler.h)）: `K_OOPS`（L378）、`K_SYSCALL_VERIFY_MSG`（L399）、`K_SYSCALL_MEMORY_READ/WRITE`（L480, L499）、`K_SYSCALL_MEMORY_ARRAY_READ`（L543。乗算のオーバーフローも検査）、`K_SYSCALL_DRIVER_OP`（L603）、`K_SYSCALL_OBJ` / `K_SYSCALL_OBJ_INIT`（L653, L669）。`k_usermode_from_copy`（L309）。
- **TOCTOU 対策**: 参照渡しの引数は検証関数内でコピーし、検査も実装関数への受け渡しもコピーに対して行う。文字列は `k_usermode_string_copy` などで安全に取り込む（[syscalls.rst L369-392](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/kernel/usermode/syscalls.rst#L369-L392)）。
- **コールバックを U に公開しない**: 「supervisor で走るコールバックを登録させる API を syscall にしてはならない」（[syscalls.rst L17-20](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/kernel/usermode/syscalls.rst#L17-L20)）。

#### 4.3.5 カーネルオブジェクトの権限とメモリドメイン
- U スレッドはカーネルオブジェクトの**アドレス**を syscall に渡すが、自分では参照外しできない。`gen_kobject_list.py` がビルド後の ELF（DWARF）からオブジェクトを列挙し、gperf の完全ハッシュ表で定数時間に検証する。実行時に動的に作るオブジェクトは赤黒木で並行管理する（[kernelobjects.rst L100-135](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/kernel/usermode/kernelobjects.rst#L100-L135)）。
- 各オブジェクトはスレッド番号で引く**権限ビットフィールド**（`CONFIG_MAX_THREAD_BYTES`）を持つ。権限を渡す方法は `K_INHERIT_PERMS`、`k_object_access_grant`、`k_object_access_all_grant`（public 化）（[kernelobjects.rst L137-175](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/kernel/usermode/kernelobjects.rst#L137-L175)）。権限は参照としての意味も持ち、全参照が消えるとクリーンアップか解放が行われる。
- メモリドメインはパーティション（開始、サイズ、属性）の集合で、スレッドはどれか 1 つのドメインに属する。ドメインが変わるとスレッドの PMP を遅延で同期する（`pmp_update_nr` を比べる。[pmp.c L1270-1290](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/core/pmp.c#L1270-L1290)）。

#### 4.3.6 LLEXT とユーザモード
- 対応アーキテクチャは RISC-V、ARM、ARM64、ARC、x86、Xtensa（[llext/index.rst L21-25](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/services/llext/index.rst#L21-L25)）。形式は ELF オブジェクト、部分リンク、共有ライブラリ（`LLEXT_TYPE_ELF_SHAREDLIB`。ARM では未対応）から選ぶ（[config.rst L275-300](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/services/llext/config.rst#L275-L300)）。
- RISC-V のローダ（`arch/riscv/core/elf.c`）は `case R_RISCV_*` を 34 個持つ（RELATIVE、JUMP_SLOT、HI20、RELAX など。[elf.c L286-525](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/core/elf.c#L286-L525)）。方式 (2) の汎用再配置で、ローダはかなり大きい。
- シンボル解決: カーネル側が `EXPORT_SYMBOL` で公開したシンボル（グループ: UNASSIGNED、SYSCALL、LIBC、DEVICE）に名前で、または SLID（ハッシュ ID）でリンクする（[config.rst L350-385](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/services/llext/config.rst#L350-L385)）。
- ユーザモード: ロードした拡張はどのユーザドメインにも入らない。U から使うには `llext_add_domain` で拡張のメモリ領域をパーティションとしてドメインに加える（[load.rst L35-38](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/services/llext/load.rst#L35-L38)、[llext_mem.c L394-415](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/subsys/llext/llext_mem.c#L394-L415)）。U 専用の拡張向けには、syscall 経由に限るヘッダを出す `CONFIG_LLEXT_EDK_USERSPACE_ONLY` がある（[build.rst L244-253](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/services/llext/build.rst#L244-L253)）。user mode では、拡張のセクションをアーキテクチャのアライメントで確保できるだけのヒープが要る（[config.rst L58-62](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/doc/services/llext/config.rst#L58-L62)）。
- 評価（推測）: **LLEXT を U スレッドで動かすことはできる**が、再配置とリンクは M モードのカーネル内で行われる。また拡張が直接呼べるのはエクスポート表のシンボルだけで、U ではそれが syscall スタブであることが前提になる。本 OS の「再配置を RELATIVE に限り、外部シンボルを 0 にする」方針のほうがローダの攻撃面は小さい。

#### 4.3.7 ESP32 系での USERSPACE の可否
| SoC | `RISCV_PMP` の選択条件 | `PMP_SLOTS` | 出典 |
|---|---|---|---|
| ESP32-C3 | 選択されない（userspace 不可） | — | `soc/espressif/esp32c3/Kconfig` に記述なし |
| ESP32-C6 / C5 / C61 / H2 | `!MCUBOOT`（C6/C5 は HP コアのみ） | 16 | 各 `soc/espressif/<soc>/Kconfig`、`Kconfig.defconfig` |
| **ESP32-P4** | `SOC_ESP32P4_HPCORE && !MCUBOOT && !SOC_ESP32P4_REV_1_3` | **16**、粒度 **128** | [esp32p4/Kconfig L7](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/soc/espressif/esp32p4/Kconfig#L7)、[Kconfig.defconfig L22-44](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/soc/espressif/esp32p4/Kconfig.defconfig#L22-L44) |

- ボード: `esp32p4_function_ev_board` は `SOC_ESP32P4_REV_1_3`（PMP 無効）、`esp32p4x_function_ev_board` は `SOC_ESP32P4_REV_3_1`（`boards/espressif/*/Kconfig.*`）。M5Stack Tab5 用のボード定義はない（`boards/m5stack/` に P4 ボードなし。2026-10-29 に GitHub API で確認）。
- **P4 固有の回避策**（コミット [737eda3f5cae](https://github.com/zephyrproject-rtos/zephyr/commit/737eda3f5cae)「soc: esp32p4: fix user mode under riscv pmp」、2026-10-02）:
  - PMP 粒度を 128B にし、null ガード、IRAM text、flash rodata をそれに揃える。
  - U に開けていたペリフェラル領域をやめる。
  - **syscall の ECALL を処理する間は mintthresh で全割り込みを止める**（`RISCV_SOC_HAS_SYSCALL_INTMASK`、[soc_syscall.h](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/soc/espressif/esp32p4/soc_syscall.h)）。理由は「ECALL 例外が開いている間に割り込みや不正確なアクセス例外が来ると安全に配送できない」こと。代償として syscall 本体はプリエンプトできなくなる（[arch/riscv/Kconfig L215-252](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/Kconfig#L215-L252)）。
  - **syscall 本体の前に ECALL を閉じる**（M モードのまま trap から戻る）。開いた ECALL の中で syscall 本体がスタックガードに当たると、例外が非同期に報告されてコアが固まり、チップがリセットされるため（`RISCV_SOC_SYSCALL_CLOSE_ECALL`、[Kconfig L254-275](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/Kconfig#L254-L275)）。
  - **ユーザ文字列を読む前に事前検証する**。ロードのアクセス例外が不正確で、mepc に基づくフィックスアップが効かないため（`RISCV_USER_STRING_NLEN_VALIDATE`、[Kconfig L650-667](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/Kconfig#L650-L667)）。
  - コンテキストスイッチは常に ECALL を通す（`RISCV_ALWAYS_SWITCH_THROUGH_ECALL`。「mret は ecall と対にしなければならない実装向け」、[Kconfig L60-68](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/arch/riscv/Kconfig#L60-L68)）。P4 の HP コアでは既定で有効（[esp32p4/Kconfig.defconfig L22-23](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/soc/espressif/esp32p4/Kconfig.defconfig#L22-L23)）。
  - U/M 共通で開けるグローバル PMP 領域: SoC ROM（0x4fc00000、libc 等が入っている）、IRAM text、flash rodata（[pmp_regions.c](https://github.com/zephyrproject-rtos/zephyr/blob/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc/soc/espressif/esp32p4/pmp_regions.c)）。
- **P4 の PMP 16 エントリで足りるか（推測による見積もり）**: Zephyr の構成では、DEP 用の予約 2、null ガード 1、rom_region 1〜2、SoC 領域 3〜6、ユーザスタック 1〜2 を引くと、パーティションに使えるのは 4〜6 程度。本 OS は「カーネルは M モードで PMP 不一致時に全許可（ロックなし）」「U にはアプリ領域だけを開ける」構成にできるので、**TOR で text/rodata/data（stack+heap を連続配置）を 4 エントリにまとめ、共有メモリ用に 8〜10 エントリを残せる**。ROM の libc を U から呼ばせないなら ROM 用エントリも不要。

#### 4.3.8 本 OS（Rust、M カーネル＋U アプリ、動的ロード）への含意

**借りるもの**
1. **検証関数と実装関数の分離**（`z_vrfy_*` / `z_impl_*`）と、**定義表からスタブ・ディスパッチ表・ID 列挙を生成するツール**。Rust では `syscalls.toml` か proc-macro から、U 側の `asm!` スタブ（t5-rt）と M 側の `match` ディスパッチを生成する（推測）。未実装の番号は共通の `ENOSYS` に落とす。
2. **ユーザメモリの検証プリミティブ**: `MEMORY_READ/WRITE/ARRAY_READ`（乗算オーバーフロー込み）、`from_copy/to_copy`、`string_copy`。いずれも**事前検証してからコピー**し、フィックスアップには頼らない（P4 の不正確な例外のため）。
3. **PMP のシャドウコピーと一括書き込み**、エントリ不足時に「該当スレッドだけをフォールトさせる」劣化動作。
4. **P4 の trap 処理で踏まれた罠への対策**: U から入ったら sp/tp/gp を差し替える。syscall 本体の前に ECALL を閉じるか、本体を割り込み禁止で走らせる。mret と ecall を対にする。ペリフェラル領域は U に開けない。
5. 権限を「参照」として扱い、最後の参照が消えたら解放する考え方（本 OS ではハンドル表の refcount として実装する）。

**借りないもの**
1. **ビルド時の静的なカーネルオブジェクト表（gperf）とアドレスで渡す方式**: 動的にロードするアプリには使えないうえ、カーネルのアドレスを U に晒す。整数ハンドル＋プロセス単位の表にする。
2. **テキストと RO データを全 U スレッドに公開する既定**: アプリ間で機密性がなくなる。本 OS では U に見えるのは自分のイメージだけにする。
3. **LLEXT 式の汎用再配置＋エクスポート表による名前リンク**: ローダの攻撃面が大きく、アプリがカーネルのシンボル（ABI）に直接依存する。RELATIVE だけの PIE と、番号による syscall にする。
4. **「syscall 本体はプリエンプト可能」を前提にした設計**: P4 の Zephyr では結局 mintthresh でマスクしている。本 OS の v1 は**syscall 本体を割り込み禁止で短く走らせ、長い処理は非同期にしてイベントで完了を通知する**（推測）。こうすれば mintthresh がない v1.x でも成り立つ見込みがある（未検証）。

---

## 5. アプリ側ランタイム crate（`t5-rt`、仮称）の構成

libtock-rs の構成を手本にする:
- libtock-rs の `runtime/src/startup/asm_riscv32.s` は、リンカスクリプトが置いた `rt_header`（start、初期 brk、stack top、data のサイズと flash/ram 位置、bss のサイズと位置）を読む。そのうえで PC と start を比べてロード位置を検査し、memop で brk を設定し、sp を設定し、`.data` をコピーし、`.bss` をゼロ埋めして `rust_start` へ飛ぶ（[asm_riscv32.s L1-86](https://github.com/tock/libtock-rs/blob/b7f17591e43607474958ed7d117ba831e54e1017/runtime/src/startup/asm_riscv32.s#L1-L86)）。
- libtock-rs のリンカスクリプトは「position-independent な再配置の解決策がないので静的リンク」と明記している（[libtock_layout.ld L1-4](https://github.com/tock/libtock-rs/blob/b7f17591e43607474958ed7d117ba831e54e1017/build_scripts/libtock_layout.ld#L1-L4)）。RISC-V の配置アドレス例は ESP32-C3 用の `F=0x403b0000 R=0x3fca2000` など（[Targets.mk L55-61](https://github.com/tock/libtock-rs/blob/b7f17591e43607474958ed7d117ba831e54e1017/Targets.mk#L55-L61)）。
- アプリはビルド時に `LIBTOCK_PLATFORM` か `LIBTOCK_LINKER_FLASH/RAM` で配置先を指定する（[build_scripts/README.md](https://github.com/tock/libtock-rs/blob/b7f17591e43607474958ed7d117ba831e54e1017/build_scripts/README.md)）。

本 OS の案（推測）:
- **カーネルが bss のゼロ埋めと再配置まで済ませてから** `_start` に入る。ランタイム側はコピーや再配置をせず、`sp` もカーネルが設定する。これで `_start` は「`gp` を使わないことを確認し、`a0`（プロセス情報ブロックへのポインタ）を保存して `main` を呼ぶ」だけになる。
- **ヒープ**: プロセス情報ブロックに書かれたヒープ範囲を `embedded-alloc` などの `#[global_allocator]` に渡す。ヒープを伸ばすときは `brk` を使う。
- **パニック**: `#[panic_handler]` で `dbg_write` にメッセージを書き、`exit(PANIC)` を呼ぶ。`panic = "abort"` はターゲットの既定（[target spec](https://doc.rust-lang.org/nightly/nightly-rustc/src/rustc_target/spec/targets/riscv32imafc_unknown_none_elf.rs.html)）。
- **syscall のラッパ**: `core::arch::asm!("ecall", inlateout("a0") …, in("a7") nr, lateout(...) _ …)`。libtock-rs のように clobber を明示する。
- **ビルド**: `.cargo/config.toml` で `-Zbuild-std=core,alloc`、`-C relocation-model=pie`、`-C link-arg=-pie`、`-C link-arg=--no-dynamic-linker`、`-C link-arg=-T t5app.ld` を指定し、ホストツール `elf2t5a` で後処理する（フラグの組み合わせは推測。実証が要る）。
- **エントリ用マクロ**: `t5_rt::entry!(main)`。アプリの名前、バージョン、パーミッションは `Cargo.toml` の `[package.metadata.t5a]` から `elf2t5a` が TLV を生成する。

---

## 6. 未確認事項

1. **実機 Tab5 の P4 chip revision**（v1.x か v3.x か）。esp-hal は v3.0 未満を非サポートとしているので、カーネルの開発基盤（esp-hal を使うか自前 PAC か）にも直結する。
2. Rust の `riscv32imafc-unknown-none-elf` で `-Zbuild-std` と `relocation-model=pie`、lld の `-pie --no-dynamic-linker` を組み合わせて、**動的再配置が RELATIVE だけになるか**。core のフォーマット処理の vtable などが `.data.rel.ro` に入り、`-z text` 違反にならないかも含めて実証が要る。
3. P4 v1.x で U モード＋PMP が実用になるか。Zephyr は rev 1.3 で PMP を無効にしており、理由は mintthresh CSR がないこと。v1.x の非標準 CLIC しきい値で代替できるか、syscall 中は MIE=0 で十分か。ECALL 処理中に非同期のアクセス例外が来るとコアが固まる問題（`RISCV_SOC_SYSCALL_CLOSE_ECALL` が対処している現象）が v1.x にもあるか。MPRV の実装状況。
4. PSRAM 上のコードを実行したときの性能（キャッシュミスの影響）。L2MEM（約 768KB）にホットなアプリを置く価値があるか。
5. HWLOOP/PIE（独自拡張）を U モードで実行したときに例外になるか（禁止を強制できるか）。
6. `xmas-elf` と `goblin` が no_std・alloc なしでどこまで使えるか（今回は docs.rs の feature 一覧しか確認していない）。
7. マルチコア（P4 は 2 コア。[soc.toml L12](https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-metadata/devices/esp32p4/soc.toml#L12)）でアプリを両コアに載せる場合の PMP の扱い。PMP はコアごとなので、スイッチのたびの再設定で済むはず（推測）。

---

## 7. 参考文献

- Tock TRD104 Syscalls: https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd104-syscalls.md
- Tock TBF types/parse: https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/libraries/tock-tbf/src/types.rs ／ .../parse.rs
- Tock dynamic process loading TRD: https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd-dynamic-process-loading.md
- Tock non-XIP TRD (Draft): https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/doc/reference/trd-non-xip.md
- Tock RISC-V PMP: https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/arch/riscv/src/pmp.rs
- Tock process buffer check: https://github.com/tock/tock/blob/d4979a4c09b6253fc0c44ddb202096ddcb15f445/kernel/src/process_standard.rs
- tockloader: https://github.com/tock/tockloader/blob/914a47dbcaf3690394d3fc338fdfea376cf43ed2/tockloader/tockloader.py
- libtock-rs README / runtime / layout / Targets.mk: https://github.com/tock/libtock-rs/tree/b7f17591e43607474958ed7d117ba831e54e1017
- esp-hal ESP32-P4 metadata: https://github.com/esp-rs/esp-hal/blob/5c0820cf9e2be0a49719bd71c30e7a45cae48134/esp-metadata/devices/esp32p4/soc.toml
- ESP-IDF 6.0（ローカル）: `~/esp-idf/6.0/components/soc/esp32p4/include/soc/soc_caps.h`, `ext_mem_defs.h`, `esp_hw_support/port/esp32p4/Kconfig.hw_support`
- 既存 Tab5 プロジェクトの sdkconfig: `~/repos/tab5_claude_client/sdkconfig`（2040-2055 行）
- Espressif elf_loader: https://components.espressif.com/components/espressif/elf_loader ／ https://github.com/espressif/esp-iot-solution/tree/e016cb9e5221f8016ff6371d3babdda15f4934a7/components/elf_loader
- rustc riscv32imafc target spec: https://doc.rust-lang.org/nightly/nightly-rustc/src/rustc_target/spec/targets/riscv32imafc_unknown_none_elf.rs.html
- lld riscv32 PIC reloc test: https://llvm.googlesource.com/lld/+/refs/heads/master/test/ELF/riscv32-reloc-32-pic.s
- LLVM lld GP relaxation commit: https://llvm.googlesource.com/llvm-project/+/85444794cdde8211b916767cc51a37547dbd17e9
- MaskRay, The dark side of RISC-V linker relaxation: https://maskray.me/blog/the-dark-side-of-riscv-linker-relaxation
- OpenSBI relocation fixes (static-pie): https://lists.infradead.org/pipermail/opensbi/2023-October/005654.html
- LKML R_RISCV_HI20 / -fPIC error: https://lkml.rescloud.iu.edu/2306.1/02149.html
- Zephyr LLEXT RISC-V relocations: https://pigweed.googlesource.com/third_party/github/zephyrproject-rtos/zephyr/+/5275d4440953265ff793b0030c03f04fa21a76ab
- NuttX RISC-V ELF HI20/LO12 fix: https://apache.googlesource.com/nuttx/+/996625ec5822b01a9a4197f0ab73c161f437bc6a
- NuttX Protected Build: https://nuttx.apache.org/docs/latest/guides/protected_build.html
- MicroPython natmod: https://docs.micropython.org/en/v1.24.0/_sources/develop/natmod.rst.txt
- object crate features: https://docs.rs/crate/object/latest/features ／ goblin: https://docs.rs/crate/goblin/latest/features
- Zephyr（`5d2ed837a8c9`）: arch/riscv/core/pmp.c, isr.S, thread.c, elf.c; include/zephyr/arch/riscv/syscall.h; include/zephyr/internal/syscall_handler.h; arch/riscv/Kconfig; soc/espressif/esp32p4/{Kconfig,Kconfig.defconfig,soc_syscall.h,pmp_regions.c}; subsys/llext/llext_mem.c — https://github.com/zephyrproject-rtos/zephyr/tree/5d2ed837a8c996b60a7d2b76cab0e3f9e98efbdc
- Zephyr docs: User Mode（overview / syscalls / kernelobjects / memory_domain）https://docs.zephyrproject.org/latest/kernel/usermode/index.html ／ LLEXT https://docs.zephyrproject.org/latest/services/llext/index.html
- Zephyr commit「soc: esp32p4: fix user mode under riscv pmp」: https://github.com/zephyrproject-rtos/zephyr/commit/737eda3f5cae
- ESP-IDF 6.0 CLIC CSR（P4 は REV2 以降で標準 mintthresh）: `~/esp-idf/6.0/components/riscv/include/riscv/csr_clic.h:36-42`
