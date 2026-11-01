# 01. ESP32-P4 CPU の特権モード・メモリ保護・割り込み・トラップ

調査日: 2026-10-29
対象: M5Stack Tab5 (ESP32-P4) 上の自作 Rust OS（カーネル = M モード、アプリ = U モード、動的ロード、ecall システムコール）

> 表記ルール
> - 出典の `TRM` は *ESP32-P4 Technical Reference Manual, Pre-release v0.7 (2026-08-20)* [R1] を指します。「p.N」は PDF の印刷ページ番号（PDF のページ番号と一致）です。
> - `IDF6` は `~/esp-idf/6.0`（v6.0-beta1-624-g6534932340, 2025-11-25）、`IDFm` は `~/esp-idf/master`（d0b65797c30, 2026-07-10）です。
> - 「（推測）」が付いた記述は、資料から直接は確認できていない推論です。
> - **注意: Tab5 に載っているのは ESP32-P4 rev v1.0 とされています [R5]。一方、TRM v0.7 の CPU 章は主に v3.x の内容です（根拠は §1.2）。v1.x と v3.x の違いが大きいので、各節でリビジョンを区別して書いています。**

---

## 0. 結論（OS 設計に効くことを先にまとめる）

- **HP コア（2 基）は M モードと U モードを持ち、S モードはない。** misa は U=1、S=0、N=0 です（TRM §2.5.2 Reg 2.6 misa, p.79）。mstatus.MPP は 0x0 (U) と 0x3 (M) だけを取ります（Reg 2.5, p.77）。仮想メモリ (MMU/satp) はありません。アプリはすべて**物理アドレス上で動かし、PMP で隔離する**設計になります。動的ロードするアプリは PIC/PIE でビルドするか、ロード時に再配置する必要があります（推測ではなく、MMU がないことからの帰結）。
- **LP コアは M モードだけで、U モードも PMP もない**（TRM §5.3 Reg 5.3 misa: U=0, p.637。CSR 一覧に pmp* がない, p.634–635）。LP コアは OS の隔離境界の外にあり、カーネルが完全に管理する「特権コプロセッサ」として扱う必要があります。
- **PMP**: v3.x は **32 エントリ**（TRM §2.10.1.2, p.142）、**v1.x（Tab5）は 16 エントリ**です（v3.x user guide [R3] の「PMP entries increased to 32」と、IDF の rev<3 用コードが 0–15 しか使っていないこと IDF6 `components/esp_hw_support/port/esp32p4/cpu_region_protect.c:235-357` による）。最小粒度は 128 B（G=5）。モードは OFF/TOR/NAPOT で、**NA4 は使えません**（TRM p.142, Reg 2.116 p.146; IDF6 `components/soc/esp32p4/include/soc/soc_caps.h:208`）。U モードは「どの PMP エントリにも一致しなければ拒否」、M モードは「ロックされていないエントリは無視」という標準の動作です（TRM §2.10.1.3, p.142）。
- **PMA（Espressif 独自、16 エントリ）** は M と U の区別なく効く「無効領域ブラックリスト + キャッシュ属性」です（TRM §2.10.2, p.147–150）。特権分離には使えませんが、カーネル自身も含めて全モードで無効アドレスを塞ぐのに使えます。
- **MMIO の保護は 2 段構え**: (1) PMP で U モードから周辺レジスタ範囲（0x5000_0000– など）を塞ぐ、(2) **APM（TRM 20 章 "Permission Control (PMS)"）は U/M を区別した許可ビットを周辺ごとに持つ**（`PMS_COREn_UM_*_ALLOW`, TRM §20.4.1 p.1591）。P4 には C6/C5 のような「TEE/REE の World コントローラ」はなく、APM の判定に使われるのは CPU の M/U モードそのものです（TRM §20.3.1 p.1588）。**基本は PMP で全面的に塞ぎ、ドライバをユーザ空間に出すときだけ PMP の窓と APM の UM ビットを組み合わせる**のが実用的です。
- **割り込みは CLIC のみ**（mtvec.MODE=3 固定。mie/mip/mideleg は 0 固定, TRM §2.9.2.1 p.130, Reg 2.7–2.9 p.80）。外部割り込みは 32 本（CLIC ID 16–47）で、それ以外は CLINT のソフト割り込み (ID 3) とタイマ割り込み (ID 7) だけです。128 本の周辺割り込み源は Interrupt Matrix を通して各コアの 32 本に割り当てます（TRM 13 章）。**全割り込みを M モード割り込みとして扱えば、U モード実行中の割り込みは M モードへトラップします**（RISC-V Privileged spec の規則による。§4.3 参照）。
- **ecall**: U モードからの ecall は mcause.CODE=8、M モードからは 11 です（TRM Reg 2.13 mcause, p.82）。CLIC モードでは例外はすべて mtvec.BASE（64 B 境界）へ非ベクタで飛びます（TRM Reg 2.9 p.80; §2.9.2.2 p.130）。mepc は ecall 命令自身を指すので、復帰時にソフトで +4 します（ecall に圧縮形はありません）。
- **v1.x 固有の落とし穴（Tab5 で直撃）**:
  - CLIC が非標準。閾値は CSR mintthresh (0x347) ではなく**メモリマップドレジスタ (0x2080_0008)** で、mintstatus は 0xFB1 ではなく 0x346 にある（IDF6 `components/soc/esp32p4/include/soc/interrupt_reg.h:28-40`, `components/riscv/include/riscv/csr_clic.h:39`）。
  - U モード割り込みへの委譲がない（v3.x で追加 [R3]）。
  - FPU の EXT_ILL ビットのバグ、HWLP の状態遷移バグがある（`soc_caps.h:194,196`）。
  - Zcmp/Zcmt はビルド対象から外されている（`components/soc/project_include.cmake:44-57`）。
- **ESP-IDF 2nd stage bootloader 経由で起動すると、PMP/PMA の一部エントリがロック済みで渡される**（§3.4 参照）。ロック済みエントリは U モードにも同じ権限を与えるので、**CLIC/CLINT 領域（0x2000_0000–）と CPU 周辺 (0x3FF0_0000–) に U モードから RW できる状態が固定される**おそれがあります。`CONFIG_BOOTLOADER_REGION_PROTECTION_ENABLE=n` にするか、自前のブートローダを使う必要があります（要実機確認）。
- **ネストした例外は CPU を lock-up させる**: トラップハンドラの中（MRET 前）でさらに例外が起きると CPU が停止します。例外は ecall/ebreak だけです（TRM §2.13.5 p.178; mexstatus.EXPT_VLD/LOCKUP, Reg 2.67 p.106）。カーネルのトラップ入口では、スタック切り替えなどで絶対に fault しないことが必須です。
- **実機で見つけた注意: デバッガがハートを止めて再開すると、`mret` の後もプロセスが M モードのまま動くことがある**（Tab5、v1.x、2026-11-01）。カーネルが U へ `mret` するあたりで止まったときに起きた。`mret` の直前の `mstatus.MPP` は U だった。デバッガをつないでいなければ起きない。仕組みは未確認で、TRM にも書かれていない。そのため、デバッガで止めた後のふるまいでは隔離を信用しない。カーネルは `mret` の前に MPP を確かめ、プロセスからのトラップでは MPP を見て M なら止める（doc/36）。
- **ESP32-C3 の「特権分離」は ESP Privilege Separation（2022, Beta、C3/S3 対象）でした。学会論文や arXiv は見つかっていません**（§7）。この実装は **RISC-V の U モードではなく、C3 独自の World Controller（World0/World1）+ PMS** で分離しています。user_app も CPU の M モードで動き、syscall は M モードからの ecall（mcause=11）です。C3 の PMP は 16 領域で優先順位がなく、DMA も守れないことを、C3 の TRM 自身が PMS を必要とする理由として挙げています。**P4 には World Controller がない**ので、P4 では本物の U モード + PMP（+ APM の UM ビット）で組むことになります。syscall テーブルの自動生成、タスクごとのカーネルスタック、user ISR のディスパッチャタスクでの遅延実行、VFS 風のドライバ公開、esp_image 形式の user イメージを単体で OTA/再起動する、といった設計要素は流用できます。v1.x の 16 エントリでも「カーネル + 1 アプリ」の構成は成り立つ見込みです（推測）。
- **参考実装**: ESP-TEE（`components/esp_tee`）は「TEE = M モード、REE = U モード」を ecall で切り替える実在の実装です。ただし対象は **C6/H2/C5/C61 だけで、P4 には対応していません**（IDF6/IDFm `components/esp_tee/Kconfig.projbuild:2`、`subproject/main/soc/` に esp32p4 がない）。CLIC 版のベクタ (`esp_tee_vectors_clic.S`) は C5 用ですが、P4 v3.x の設計とほぼ同じ構造なので参考になります。

---

## 1. CPU コア構成と ISA

### 1.1 HP コア（Core0/Core1）

| 項目 | 内容 | 出典 |
|---|---|---|
| コア数 | 2（デュアル RISC-V、5 段インオーダ）。各コアが CLIC を持ち、CLINT タイマは 2 コアで共有 | TRM §2.1 p.68 |
| 最大クロック | TRM は 360 MHz。v3.x は 400 MHz | TRM §2.2 p.69, [R3] |
| ISA | RV32IMAFC + Zc (Zcb/Zcmp/Zcmt) + Zb + 独自 X（AI 向け PIE、ハードウェアループ HWLP） | TRM §2.2 p.69; §2.6.4.1 p.113 |
| 準拠仕様 | Unprivileged v2.2、**Privileged v1.10**、CLIC (2023-10-10)、External Debug v0.13.2 | TRM §2.2 p.69 |
| 特権モード | M + U（S なし）。「User (U) privilege mode execution」「Interrupt delegation to user mode」 | TRM §2.2 p.69; Reg 2.6 misa p.79 |
| PMP/PMA | PMP 最大 32 領域、PMA 16 領域 | TRM §2.2 p.69 |
| トリガ | HW ブレーク/ウォッチポイント 3 本 | TRM §2.2 p.70; IDF6 `soc_caps.h:202-203` |
| アトミック | A 拡張（LR/SC/AMO）。HP と LP の間でアトミックが効くのは L2 SRAM だけ | TRM §2.6.6 p.117; §5.8.1 p.654 |

**misa の注意**: TRM Reg 2.6 (p.79) のリセット値の表では **F ビットが 0** になっていますが、F 拡張は実装されています（§2.6.3 p.112）。misa を見て F の有無を判定するコードは書かない方が安全です（TRM の誤記か実装の癖かは未確認。要実機確認）。

**ツールチェーン上の ISA 指定**（IDF6 `components/soc/project_include.cmake:41-75`, `tools/cmake/toolchain-clang-esp32p4.cmake:14`）:
- 基本: `rv32imafc_zicsr_zifencei` / ABI `ilp32f`
- rev ≥ 3 のときだけ `_zcb_zcmp_zcmt` を付ける。その場合も `-mno-cm-push-reverse`、`-mno-cm-popret` を付ける（`SOC_CPU_ZCMP_PUSH_REVERSED`/`SOC_CPU_ZCMP_POPRET_ISSUE`, `soc_caps.h:214-215`）。
- HWLP は `_xesploop`。PIE は `_xespv`（rev<3 では `_xespv2p1`）。
- **Rust（推測）**: Tab5 (v1.0) なら `riscv32imafc-unknown-none-elf`（ilp32f）が素直です。Zcmp は無効にしておくのが安全です。PIE/HWLP は Rust からは使わない前提にすれば、カーネルはそれらを OFF のままにしておけます（§5.3）。

### 1.2 TRM がどのリビジョンを記述しているか

- TRM v0.7 の HP CPU 章には次のような v3.x 以降の機能が載っています: PMP 32 エントリ、mintthresh CSR (0x347)、mintstatus 0xFB1、Interrupt Matrix のリマップレジスタ (`COREx_INTR_SIG_IDX_ASSERT_IN_SEC_REG`)。IDF でもリマップレジスタは `register/hw_ver3` にしかなく（`components/soc/esp32p4/register/hw_ver3/soc/interrupt_core0_reg.h:3494-3503`）、`hw_ver1` にはありません。また IDF のコメントには「P4 (since REV2) use the standard CLIC specification」とあります（`components/riscv/include/riscv/csr_clic.h:39`）。以上から、**TRM の CPU 章は v3.x 基準**と判断しました。
- ESP-IDF 6.0 でも、v1.x 対応 (`CONFIG_ESP32P4_SELECTS_REV_LESS_V3`) と v3.x 対応は**相互排他**で、同じバイナリでは両方をサポートできません（IDF6 `components/esp_hw_support/port/esp32p4/Kconfig.hw_support:1-11`）。
- v3.x で CPU まわりに入った変更（[R3] "ESP32-P4 Chip Revision v3.x User Guide" v1.1 §1.1/§1.9/§1.11 の要約）:
  - PMP エントリを 32 に増加。PMP ロックビットの挙動を改善。
  - Zb を追加。
  - **U モードへの割り込み委譲を追加**。
  - HWLP カウンタを 24 bit 化し、ロジックのバグを修正。
  - FPU の EXT_ILL bit0 のバグを修正。
  - CLIC の準拠性とレイテンシを改善。
  - Interrupt Matrix に U→M リマップと割り込みレジスタの権限制御を追加。
  - PERI APM の可変アドレス範囲を 2 から 8 に増加。
- **Tab5 は rev v1.0**（NuttX の Tab5 ボード文書に、出荷チップが v1.0 で `CONFIG_ESP32P4_SELECTS_REV_LESS_V3=y` を使うと記載 [R5]）。ただし、出荷ロットで変わる可能性はあります（要実機確認: eFuse のチップリビジョンを読む）。

### 1.3 LP コア

| 項目 | 内容 | 出典 |
|---|---|---|
| ISA | RV32IMAC（F なし、X なし）、2 段パイプライン、最大 40 MHz | TRM §5.1 p.633; §5.2 p.634; Reg 5.3 misa p.637 |
| 特権 | **M のみ**（misa.U=0、mstatus.MPP は 0x3 のみ有効） | Reg 5.2 p.636; Reg 5.3 p.637; §20.4.2 p.1591 |
| 保護 | **PMP なし**（CSR 一覧に pmp* がない）。LP コアからのアクセスは APM の `PMS_LP_MM_*` で制御 | TRM §5.3.1 p.634–635; §20.4.2 p.1591 |
| 割り込み | 標準の mie/mip、mtvec はベクタモード（256 B 境界）、18 本 | TRM §5.4 p.644–645; Reg 5.5 |
| アクセス範囲 | LP ROM/LP SRAM/L2 SRAM と、全周辺アドレス空間 | TRM §5.2, §5.8 p.654 |

→ LP コアに任意のコードを載せると、HP 側の PMP とは無関係に L2 SRAM（カーネル領域を含む）を読み書きできます。APM の LP2HP 設定で LP コアの L2MEM アクセスを止めることはできますが（TRM 表 20.1-1 p.1586）、OS としては「LP コアのファームウェアはカーネルの一部」と見なすのが妥当です（推測）。

---

## 2. 特権モード関連の CSR（HP コア）

TRM §2.5.1 CSR 一覧 (p.71–75) と §2.5.2 の各レジスタ説明より。

### 2.1 mstatus (0x300)（Reg 2.5, p.77–78）
実装されているフィールド: SD、TW、MPRV、XS（RO）、FS、MPP、MPIE、UPIE、MIE、UIE。
- **MPP**: 0x0=U、0x3=M。書き込めるのは下位ビットだけで、上位ビットは下位ビットに連動します。
- **MPRV**: 実装あり。ロード/ストアを MPP の権限で実行します（命令フェッチには効かない）。カーネルがユーザポインタを PMP 検査付きでコピーするのに使えます。
- **TW**: 実装あり。1 にすると U モードの WFI が不正命令例外になります。
- **TSR/TVM/MXR/SUM はない**（S モードがないため）。
- **FS**: 標準どおり Off/Initial/Clean/Dirty。リセット時は 0 (Off) で、FPU 命令は不正命令例外になります（§2.6.3.3 p.112）。
- **XS**: HWLP と PIE の状態をまとめた読み出し専用フィールド。それぞれの状態の変更は `mext_hwlp_status` と `mext_pie_status` で行います（Reg 2.5 p.77）。

### 2.2 トラップ関連
- **mtvec (0x305)**: BASE[31:6]（64 B 境界）。**MODE は 3 (CLIC) 固定で RO**（Reg 2.9 p.80）。
- **mtvt (0x307)**: CLIC のベクタテーブルの基底（64 B 境界）（Reg 2.10 p.80）。
- **mepc**（Reg 2.12 p.81）、**mtval**: 命令アクセスフォールトのアドレス、不正命令のオペコード、ロード/ストアのアドレス、PIE 不正命令のオペコードが入ります（Reg 2.14 p.83）。
- **mcause (0x342)**（Reg 2.13 p.82）: INT[31]、MINHV[30]、MPP[29:28]、MPIE[27]、MPIL[23:16]、CODE。
  - 例外コード: 1 命令アクセスフォールト、2 不正命令、3 ブレークポイント/EBREAK、5 ロードアクセスフォールト（PMP）、6 ストア/AMO ミスアライン、7 ストア/AMO アクセスフォールト、**8 ECALL from U**、**11 ECALL from M**、**0x1F PIE 不正命令**。
  - 0（命令ミスアライン）は発生しません。命令フェッチ時にアドレスの bit0 がマスクされるためです。
  - **4（ロードミスアライン）が一覧にありません**。ミスアラインアクセスはハードウェアが処理すると推測されますが、要実機確認です。
  - CLIC の仕様どおり、mcause.MPP と MPIE は mstatus のミラーです。
- **mscratch**、**mscratchcsw (0x348)**、**mscratchcswl (0x349)**（Reg 2.19–2.20 p.85–86）: mscratchcsw は「特権モードが変わったときだけ mscratch と rs1 を入れ替える」CLIC 標準の CSR です。U から M へ入ったときのカーネルスタックへの切り替えを 1 命令で書けます（v1.x に実装があるかは要実機確認）。
- **mideleg/mie/mip は 0 固定**（CLIC モード専用のため, Reg 2.7, 2.8, 2.15 p.80, 83）。**medeleg（例外委譲）は CSR 一覧にありません** → 例外は常に M モードで受けることになります。

### 2.3 U モード用 CSR（N 拡張風、CLIC 用）
ustatus (0x000)、utvec (0x005)、utvt (0x007)、uscratch、uepc、ucause、unxti、uintthresh (0x047)、uclicbase、uintstatus (0xCB1) が並んでいます（p.72; Reg 2.22–2.31 p.87–92）。ただし次の矛盾があります。
- misa.N=0（"User-level interrupts supported = 0", p.79）
- mintstatus.UIL は「user mode interrupts are not supported」として 0 固定（Reg 2.17 p.84）
- mcliccfg.NMBITS は 0 固定で「all interrupts are in machine mode only」（Reg 2.94 p.135）

一方で、clicintattr.MODE に 0x0 (User) を書けるという記述（Reg 2.98 p.136）や、U モードに委譲された割り込みのリマップの記述（§13.4.4 p.1233–1234）もあります。**TRM 内で矛盾しています。** IDF6 の CLIC ヘッダでは MODE フィールドが「RO, fixed to 2'b11」とされています（`components/soc/esp32p4/include/soc/clic_reg.h:78-83,147-148`。v1.x と v3.x 共通のヘッダ）。

→ **自作 OS では U モード割り込み (N 拡張) は使わず、すべての割り込みを M モードで受け、必要ならカーネルがソフトウェアでシグナルやアップコールとしてアプリに届ける**設計を推奨します。ustatus/uscratch などを U モードのアプリが書き換えても、カーネルに影響はないと推測されます。

### 2.4 カウンタ
- mcounteren (0x306) で CY/TM/IR/HPM8/9/13 を U モードに公開するかを制御します（Reg 2.32 p.93）。
- U モードからは cycle/time/instret（0xC00–0xC02 と上位の h）が読めます（p.73–74, Reg 2.51–2.62）。time の源は CLINT の mtime と推測されます（要確認）。
- **タイミングサイドチャネルを気にするなら、mcounteren で U モードからの読み出しを制限**します。

---

## 3. メモリ保護

### 3.1 アドレスマップ（HP CPU から見た, TRM 表 2.4-1 p.71、表 20.1-1 注記 p.1587）

| 範囲 | 内容 |
|---|---|
| 0x2000_0000–0x2FFF_FFFF | CPU サブシステム（CLINT 0x2000_0000、CLIC 0x2080_0000、相手コアの CLINT +0x1_0000 / CLIC +0x1_0000。表 2.8-2 p.129） |
| 0x3010_0000–0x3010_1FFF | HP TCM（SPM, 8 KB）（IDF6 `soc.h:160-161`） |
| 0x3FF0_0000–0x3FF1_FFFF | HP CPU PERI（CPU 周辺） |
| 0x4000_0000–0x43FF_FFFF / 0x4800_0000–0x4BFF_FFFF | 外部フラッシュ / PSRAM（キャッシュ経由） |
| 0x4FC0_0000–0x4FC1_FFFF | HP ROM |
| 0x4FF0_0000–0x4FFB_FFFF | HP L2MEM（768 KB） |
| 0x5000_0000–0x500F_FFFF | HP 周辺 (HP PERI) |
| 0x5010_0000–0x5010_FFFF | LP ROM / LP SRAM |
| 0x5011_0000–0x5012_FFFF | LP 周辺 (LP PERI) |
| 0x8xxx_xxxx | 上記メモリの「Direct access」（キャッシュを通らない）エイリアス（0x8FF0_0000 = L2MEM など） |

**キャッシュ有り/無しのエイリアスが両方存在するので、PMP で塞ぐときは両方を塞ぐ必要があります。** IDF の v3 用コードも、キャッシュ有りとキャッシュ無しの 2 エントリをペアで設定しています（`cpu_region_protect.c:88-94` `PMP_ENTRY_SET_CACHED_AND_UNCACHED`）。

### 3.2 PMP（標準）（TRM §2.10.1 p.142–146）
- エントリ数: v3.x は 32（pmpcfg0–7、pmpaddr0–31, p.143）。**v1.x は 16**（[R3]、IDF の rev<3 コード）。TRM 本文にも「enabled 16 pmpcfgX」「implements all pmpcfg0-3 and pmpaddr0-15」という v1 時代の記述が残っています（p.142, p.143）。
- 粒度は 128 B。TOR/NAPOT に対応し、NA4 は非対応（A=2 は "Not Supported"）（Reg 2.116 p.146）。
- 優先順位は標準どおり、**番号の小さいエントリが優先**（`cpu_region_protect.c:364-365` のコメント）。
- **L ビット**: 一度立てるとコアリセットまで解除できず、M モードにも適用されます。
- **XL ビット（bit5, 独自）**: 「custom-lock」。立てると、そのエントリの pmpaddr と pmpcfg を書き換えられなくなりますが、**M モードへの適用はありません**（L と違い、ロックのみ）（Reg 2.116 p.146）。カーネル自身の保護エントリを、M モードを縛らずに固定するのに使えます。v1.x に実装があるかは要確認。
- フォールト時: 命令フェッチは mcause=1、ロードは 5、ストアは 7 で、mtval に違反アドレスが入ります（p.142）。mexstatus.PMP_ERR で PMP/PMA 由来かどうかを判別できます（Reg 2.67 p.107）。
- **PMP の CSR は M モードからしかアクセスできません**（p.142–143）。
- ESP-IDF の errata メモ: **v3 では PMP エントリ 24 と 28 を TOR の上端として使えない**（"DIG-752", `cpu_region_protect.c:183,217`）。

### 3.3 PMA（Espressif 独自）（TRM §2.10.2 p.147–150）
- 16 エントリ。`pma_cfgN` は 0xBC0+N、`pma_addrN` は 0xBD0+N。粒度 128 B、TOR/NAPOT。
- **特権モードに関係なく適用されます**（"doesn't differentiate between machine and user mode"）。
- TYPE=0（invalid）ならアクセスフォールト。TYPE=1 なら RWX をチェックします。
- ATTRIBUTE[27:24] はキャッシュ属性（non-cacheable、write-through、allocate）をキャッシュコントローラへ渡すものです。
- LOCK ビット (bit29) はコアリセットまで解除不可。ビット位置は IDF6 `components/riscv/include/riscv/csr.h:58-66`（PMA_EN=bit0, R=bit4, W=bit3, X=bit2, L=bit29, TOR=0x4000_0000）。
- **ROM が PMA エントリの一部を使います**。外部メモリなどのキャッシュ可属性はエントリ 15 で付けられます（`cpu_region_protect.c:48, 79-81`）。PMA を書き換えると、**キャッシュ属性が変わってキャッシュ可能領域が壊れる**リスクがあります。

### 3.4 ESP-IDF の実際の PMP/PMA 設定（`cpu_region_protect.c`）
- PMA は無効領域（隙間）をすべて `PMA_L | PMA_EN`（invalid、ロック）で塞ぎ、エントリ 13 で外部メモリ/ROM/L2MEM を RWX かつキャッシュ可にしてロックします（`:41-86`）。
- PMP（rev<3, `:235-357`）:
  - 0: CPU サブシステム（0x2000_0000–0x3000_0000）NAPOT **RW+L**
  - 1: CPU 周辺 NAPOT **RW+L**
  - 2: ROM NAPOT **RX+L**
  - 3–5: IRAM/DRAM（MEMPROT 有効時は text を RX、data を RW に分割）
  - 6–8: フラッシュの I/D キャッシュ
  - 11–14: LP メモリ
  - 15: HP 周辺 NAPOT **RW+L**
- PMP（rev≥3, `:96-233`）: 0 CPU サブシステム、1 CPU 周辺、2/3 ROM（キャッシュ有/無）、4–9 SRAM（キャッシュ有/無ペア）、10–19 PSRAM、20–26 フラッシュ、27 周辺、28–31 LP メモリ。
- **重要: ブートローダもこの関数を呼びます**（`components/bootloader_support/src/bootloader_mem.c:40-43`、`CONFIG_BOOTLOADER_REGION_PROTECTION_ENABLE` の既定値は y: `components/bootloader/Kconfig.projbuild:274-276`）。
  - ブートローダビルドでは、SRAM のエントリだけ L なし（`CONDITIONAL_*`, `:21-35`）になります。**CPU サブシステム、CPU 周辺、ROM、HP 周辺のエントリと全 PMA エントリは、ブートローダの時点で L 付きになります**（rev<3: エントリ 0, 1, 2, 15。rev≥3: 0, 1, 2, 3, 27）。
  - L 付きエントリは U モードにもそのまま効きます。そのため、例えば rev<3 で番号最小のエントリ 0 が「0x2000_0000–0x2FFF_FFFF を RW」でロックされていると、**U モードから CLIC/CLINT（割り込み許可、レベル、mtimecmp、MSIP）を書けてしまい、カーネル側から塞ぐ手段がありません**（エントリ 0 より優先されるエントリが存在しないため）。エントリ 1（CPU 周辺）も同じ状況です。HP 周辺（エントリ 15）なら、3–14 にロックなしの「権限なし」エントリを置けば U モードだけを塞げます。
  - → **自作 OS は `CONFIG_BOOTLOADER_REGION_PROTECTION_ENABLE=n` でビルドしたブートローダを使うか、自前のブートローダを使い、PMP/PMA がロックされていない状態でカーネルに渡すべきです。** ROM が PMA をロックしているかは要実機確認です（csrr で pma_cfg を読めば確認できます）。
- アプリビルドでは、MEMPROT の有無にかかわらず SRAM エントリも L 付きです（M モードにも W^X を適用, `:389-409`）。自作 OS では、カーネルの W^X は「L なし + XL」で固定するか、あえてロックしないかを選ぶことになります。

### 3.5 APM / PMS（TRM 20 章 "Permission Control (PMS)", p.1586–1702）
- 構成: **PMP（CPU 内部）と APM（バスポート側）** の 2 段（§20.1 p.1586）。
- 表 20.1-1 (p.1586) によると、HP CPU から次へのアクセスは「PMP + HP APM」または「PMP + LP APM」で二重にチェックされます。
  - HP CPU PERI、HP PERI、LP PERI、LP SRAM
  - Direct Access（キャッシュを通らない）ROM/L2MEM/外部メモリ

  キャッシュ経由のメモリと HP SPM (TCM) は **PMP だけ**でチェックされます。PMP で弾かれたアクセスは APM まで届きません（p.1587）。
- **HP APM / LP APM は CPU0/CPU1 の M モードと U モードで別々の許可レジスタを持ちます**: `PMS_COREn_MM_HP_PERI_PMS_REG0..3` と `PMS_COREn_UM_HP_PERI_PMS_REG0..3`（§20.6.2 p.1599–1600）。
  - 周辺ごとに 1 ビットです（例: `PMS_COREn_UM_HP_GPIO_ALLOW`、`_HP_SYSTIMER_ALLOW`、`_HP_SYS_REG_ALLOW`、`_HP_CLKRST_ALLOW`, Reg 20.68 p.1655）。
  - リセット値は 1（許可）（IDF6 `components/soc/esp32p4/register/hw_ver1/soc/hp_peri_pms_reg.h:748`）。
  - LP 側は `PMS_HP_COREn_{MM,UM}_*_ALLOW`（HP2LP_PERI_PMS）。
- 周辺レジスタ用の可変アドレス範囲は 8 個（v3.x。v1.x は 2 個 [R3]）。可変範囲のチェックが固定範囲より先に行われます（§20.3.2 p.1589–1590）。
- DMA APM: DMA マスタごとに 4 KB 粒度の 32 領域で R/W を許可します（§20.3.2.2 p.1590）。**DMA は CPU の M/U モードと無関係に動きます**（p.1591）。
- 違反時: エラー応答と割り込みが出ます。HP/DMA APM は ICM_SYS_REG_INT、LP APM は LP_SYSREG_INTR。違反情報は SYSREG に記録されます（§20.5 p.1592）。errata **APM-560** があります [R4][R11]。複数の AHB DMA マスタ（USB OTG、GMAC、SDMMC、AHB PDMA など）が PSRAM/Flash に同時にアクセスしている状況で、DMA APM が権限のないアクセスを遮断すると、権限のある後続アクセスまで止まることがあります。回復手段はシステムリセットだけです。影響は v0.0/v1.0/v1.3/v3.0 で、v3.1 で修正されています（ESP32-P4 Series SoC Errata v1.4 §3.11 p.12）。CPU の APM ではなく DMA APM の問題なので、**Tab5 (v1.0) では「DMA APM で PSRAM/Flash へのアクセスを拒否する」という使い方は避ける**必要があります。
- PMS レジスタそのものにロック機構は見当たりません（レジスタ一覧 p.1593–1602）。**APM レジスタは PMP で U モードから塞ぐ必要があります**。HP 周辺 0x5000_0000– の中にあると推測されます。
- **P4 には C6/C5 の "TEE controller / security mode (TEE/REE0–2)"（World）がありません。** IDF でも `SOC_APM_SUPPORT_TEE_PERI_ACCESS_CTRL` / `SOC_APM_SUPPORT_LP_TEE_CTRL` は C5/C6 だけで定義されています（IDF6 `components/soc/esp32c5/include/soc/soc_caps.h:558-560`, `esp32c6/.../soc_caps.h:475`。P4 の `soc_caps.h:85` は `SOC_APM_SUPPORTED` だけ）。P4 の APM は M/U モードそのものを見ます。

### 3.6 U モードから MMIO を禁止/許可する方法（まとめ）
1. **既定（推奨）**: U モード用の PMP エントリを、そのアプリのコード（RX）、データ/スタック（RW）、共有バッファなど必要最小限だけ作ります。それ以外は「どのエントリにも一致しない → U モードは拒否」で自動的に塞がります。
2. **周辺を U モードに直接見せたい場合**（ユーザ空間ドライバ）: その周辺の 4 KB ブロック程度を PMP の NAPOT で RW 許可し、さらに APM で **他の周辺の UM_ALLOW を 0** にします（多層防御）。
   - PMP エントリ数が少ないので（v1.x では 16）、周辺ごとの細かい窓はすぐに枯渇します。
   - 「周辺領域全体を PMP で許可し、どの周辺を許すかは APM の UM ビットで決める」という使い分けもできます。この場合、PMP 1 エントリで多数の周辺を個別に制御できます。
3. **DMA を持つ周辺を U モードに渡すと、PMP を迂回して任意のメモリに書けます。** DMA APM で範囲を制限するか、DMA の設定はカーネルの syscall 経由にします。
4. CLIC/CLINT (0x2000_0000–) は APM の管轄外です（表 20.1-1 にない）。**PMP でしか守れません**（§3.4 のロック問題に注意）。
5. **Dedicated IO の CSR（cpu_gpio_oen/in/out, 0x803–0x805）** と **fxcr (0x800)** は、CSR アドレスの規約上は「ユーザ R/W カスタム」領域（0x8xx）です。U モードから PMP と APM を経ずに GPIO 操作ができる可能性があります（推測。TRM §2.13.2 p.174–175 に特権の記述はなし）。GPIO マトリクスで Dedicated IO をパッドに割り当てなければ実害はないと考えられます。要実機確認。

---

## 4. 割り込みコントローラとタイマ

### 4.1 CLIC（TRM §2.9.2, p.130–136）
- 各コアに CLIC が 1 つ。**CLIC モードのみ**で、mtvec.MODE=3 は RO（p.130）。
- ID の割り当て（表 2.9-1 p.131）:
  - 3 = CLINT の M ソフト割り込み
  - 7 = CLINT の M タイマ割り込み
  - 16–47 = 外部割り込み 0–31（Interrupt Matrix の出力）
  - clicinfo の NUM_INTERRUPTS=48（Reg 2.95 p.135）
- レベル/優先度: CLICINTCTLBITS=3（8 段階）。`mcliccfg.MNLBITS` でレベルと優先度のビット配分を決めます（表 2.9-2 p.132）。
- 有効な閾値は max(mintstatus.MIL, mintthresh.TH) です。ネストさせるにはハンドラ内で mstatus.MIE を 1 にします（p.132）。
- `clicintattr[i]`: SHV（ハードウェアベクタ）、TRIG（レベル/エッジ、極性）、MODE（Reg 2.98 p.136）。
  - SHV=1 のとき `mtvt + 4*i` に格納されたアドレスへジャンプ（テーブル参照型）。SHV=0 のときは mtvec.BASE へ（p.130, 136）。
  - レベル割り込みの IP ビットは RO で、割り込み源をクリアしないと消えません（Reg 2.96 p.136）。
- MMIO の配置: `mcliccfg` 0x2080_0000、`clicinfo` +4、`mintthresh` +8、`clicint*[i]` 0x2080_1000+4i（p.133–134）。ワード単位でアクセスすると 4 バイトがまとめて atomic に更新されます（p.133）。
- 自コアの CLIC は 0x2080_xxxx、相手コアの CLIC は 0x2081_xxxx。「CLIC user」(0x20Bx_xxxx) というサブモジュールもあります（表 2.8-1/2.8-2 p.129）。
- `mnxti` で割り込みテールチェイン（Reg 2.16 p.84）。
- **v1.x の差分**（IDF6 `components/soc/esp32p4/include/soc/interrupt_reg.h:28-40`）:
  - 閾値は CSR ではなく**メモリマップドの `CLIC_INT_THRESH_REG` (0x2080_0008)**（`clic_reg.h:61`）。
  - `mintstatus` CSR は **0x346**。
  - v3.x 以降は標準 CSR（mintthresh=0x347、mintstatus=0xFB1）。
  - Rust の割り込み制御（クリティカルセクション）はリビジョンごとに実装を分ける必要があります。

### 4.2 Interrupt Matrix（TRM 13 章, p.1224–1266）
- 128 の周辺割り込み源を、各コアの 32 本の CPU 割り込み（CLIC ID 16–47）に割り当てます。`COREx_<SRC>_INT_MAP_REG` に 16–47 を書くと割り当て、0–15 を書くと無効化です（§13.4.2 p.1233; Reg 13.x p.1251）。共有も可能です。状態は `COREx_INTR_STATUS_REG_n` で確認できます。
- **v3.x の「Interrupt Remapping」**（§13.4.4 p.1233–1234）: U モード委譲された割り込みを、CPU が M モードで動いている間だけ特定の M 割り込みに振り替える仕組みです（ESP-TEE の C5 実装と同じもの）。
  - `COREx_<SRC>_SRC_IN_SEC_FLAG` を立てた源のマップレジスタは M モードからしか書けません（Reg p.1251）。
  - **v1.x にはありません**（`hw_ver1` にレジスタ定義なし）。
- 自作 OS で全割り込みを M モードとして扱うなら、remapping は不要です。

### 4.3 U モード実行中の割り込み → M モードへのトラップ
- RISC-V の規則では、**現在の特権モードより高い特権の割り込みは、その xIE ビットに関係なく有効**です（Privileged spec v1.10 §3.1.6.1 [R6]）。CLIC 仕様でも同じで、mintthresh は M モード実行中の割り込みの選別に使われます（TRM p.85, p.132 の「preempt the execution of an ongoing machine mode interrupt handler」）。
  - したがって、**U モード実行中は CLIC で許可された M 割り込みが常に入ります**（mstatus.MIE の値に関係なく）。プリエンプティブなスケジューリングが可能です（要実機確認: 特に v1.x）。
- 割り込みを受けたときのハードウェア動作（§2.9.2.2 p.130、mcause Reg 2.13）:
  - mepc に割り込まれた PC、mcause に INT=1 と ID が入ります。MPP=0 (U)、MPIE←MIE、MIE←0、MPIL←前のレベル、mintstatus.MIL←新しいレベル。
  - そのうえで SHV に応じて mtvt テーブルか mtvec へ飛びます。
- **カーネルのトラップ入口は U モード由来か M モード由来かを判別し、スタックを切り替える**必要があります。`mscratchcsw` を使うのが定石で、ESP-TEE は代わりにメモリ上の変数 `_ns_sp`/`_s_sp` に退避しています（§6）。
- タイマ割り込みはハードウェアベクタ (SHV=1) にできます。ただし、U モードから入った場合もベクタ先で同じスタック切り替えが必要です。
- **割り込みハンドラ (MIE=0) の中では例外を起こさないこと**（§5.2 の lock-up）。

### 4.4 タイマ
- **CLINT（コアローカル）**（TRM §2.9.3 p.137–141）:
  - 64 bit の mtime は 2 コア共有で、カウンタを有効化できるのは Core0 だけです。
  - `mtimecmplo/hi` (+0x4000/+0x4004) はコアごと。`mtimectl`（MTIME_EN、オーバーフロー、サンプリングモード）、`msip` (+0x0)、`mtime` (+0xBFF8)。
  - 割り込みは ID 7（タイマ）と ID 3（ソフト、コア間 IPI に使える）。
  - クロック源と周波数は TRM に明記されていません（要確認）。
- **SYSTIMER**（TRM 16 章 p.1507–）: 52 bit カウンタ 2 本、比較器 3 本、16 MHz（XTAL 40 MHz ÷ 2.5）。Interrupt Matrix の源 53–55（`SYSTIMER_TARGET0..2`）として届きます（表 13.4-1 p.1229）。ESP-IDF の FreeRTOS ティックは SYSTIMER を使います（IDF6 `components/freertos/Kconfig:461-498`）。
- **推奨（推測）**: スケジューラのティックやワンショットタイマには、ESP-IDF と同じく SYSTIMER を使うのが実績面で安全です。CLINT タイマはコア内のイベントや IPI (MSIP) に使えます。どちらのレジスタも PMP で U モードから塞ぎます。

---

## 5. トラップと ecall

### 5.1 ecall の流れ（HP コア）
1. U モードで `ecall` を実行する。
2. 例外として M モードへ入る。mepc = ecall 命令のアドレス、mcause.CODE = 8、mcause.INT = 0、MPP = 0 (U)（Reg 2.13 p.82）。
3. 例外は CLIC モードでは常に **mtvec.BASE**（64 B 境界）へ非ベクタで入る（p.130; ESP-TEE のベクタコメント `components/esp_tee/subproject/main/arch/riscv/esp_tee_vector_table_clic.S:22-35`）。
4. ハンドラは mcause で割り込みと例外を振り分け、レジスタを退避し、syscall を実行し、`mepc += 4` として `mret` で U モードへ戻る（MPP=0 のまま）。

ESP-TEE の実装もこの流れで、ハンドラ先頭で mcause を 8 (U ecall) と 11 (M ecall) に振り分けています（`esp_tee_vectors_clic.S:24-26, 190-201`）。

### 5.2 状態の保存とネスト
- ハードウェアは汎用レジスタも FPU レジスタも自動では保存しません（ハードウェアスタックはありません）。CLIC の MPIL、MPP、MPIE は mcause に入ります。
- 独自 CSR `mexstatus` (0x7E1)（Reg 2.67 p.106–107）:
  - EXPT_VLD: トラップに入ると 1 になる。
  - LOCKUP: 二重例外で 1 になる。
  - BUS_ERR、PBEXE/PPBEXE（トラップ時にプッシュ/ポップされる）、PMP_ERR、FETCH_ERR（MRET でクリア）。
  - NMFT（既定 1）: トラップ時にメモリフェンスを省略する。
  - CLIC_INHV。
- **ロード/ストアのバスエラーは既定で非同期例外**です。mepc は原因の命令を指しません（§2.13.1 p.172–173）。`mhint.SBE=1` にすると同期例外になります（ロード/ストアのレイテンシは増える）。**ユーザアプリの fault を正確に特定してアプリだけを kill するには、SBE=1 を検討すべきです**（推測）。PMP 違反自体は同期例外です（p.142）。
- **Core lock-up**（§2.13.5 p.178）: 例外処理中（EXPT_VLD=1、MRET 前）に新しい例外が起きると、コアは停止し、デバッガかリセットでしか復帰できません。ecall と ebreak は例外です。`SOC_CPU_HAS_LOCKUP_RESET`（`soc_caps.h:210`）なので、lock-up でリセットをかけることはできそうです（要確認）。
  - **カーネル設計上の帰結**: トラップ入口で U モードの sp を使って push してはいけません（不正な sp で fault → lock-up）。**mscratch/mscratchcsw でカーネルスタックに切り替えてから**退避します。
  - カーネル内で fault しうる処理（ユーザポインタの参照）は、トラップハンドラを抜けて「EXPT_VLD がクリアされた文脈」で行うか、MPRV なしの事前検証（アドレス範囲チェック）で済ませます（推測）。
- 割り込みのネスト: CLIC レベルで制御します。ESP-IDF は `SOC_INT_HW_NESTED_SUPPORTED`（`soc_caps.h:190`）。ESP-TEE は CLIC 版でもネスト未対応（`esp_tee_vectors_clic.S:588-590`）。

### 5.3 FPU・拡張コンテキスト（コンテキストスイッチで保存が必要なもの）

| 状態 | 有効化 / 状態ビット | U から使えるか | 保存の要否 | 出典 |
|---|---|---|---|---|
| F（f0–f31, fcsr） | mstatus.FS（Off→不正命令） | 可（FS≠0 のとき） | FS=Dirty のとき | TRM §2.6.3.3 p.112 |
| fxcr (0x800) | — | 可（推測。ユーザ CSR 番号） | DQNaN などの設定を持つので保存（推測） | Reg 2.50 p.100 |
| jvt (0x017, Zcmt) | — | 可 | **保存が必要**と明記 | §2.6.4.4 p.115 |
| HWLP（uhwloop0/1_start/end/count, 0x8C0–0x8C6） | `mext_hwlp_status` (0x7F1)。STATE と **URW**（1 で U モードに開放） | URW=1 のとき | STATE=Dirty のとき | Reg 2.64 p.104; §2.7.1.3 p.119 |
| PIE（128 bit ×8 ベクタレジスタ、アキュムレータなど） | `mext_pie_status` (0x7F2) の STATE | STATE≠0 のとき（U 専用の許可ビットは見当たらない） | Dirty のとき | Reg 2.65 p.105; §2.7.2.3 p.127–128 |
| 不正命令の原因 | `mext_ill` (0x7F0): FP_ILL / HWLP_ILL / PIE_ILL | — | 遅延保存の判定に使う | Reg 2.63 p.104 |

- **遅延 (lazy) 保存**: ESP-IDF の FreeRTOS ポートは、FS/STATE を Off にしておき、不正命令トラップで `mext_ill` を見て、コプロセッサの所有者を切り替えます（IDF6 `components/riscv/vectors.S:285-325`。`SOC_CPU_COPROC_NUM=3` は FPU/HWLP/PIE）。
  - **v1.x のバグ**: EXT_ILL が FPU の load/store（FLW/FSW）や FPU CSR 命令に対して立ちません。そのため mtval のオペコードをデコードして判定しています（`vectors.S:298-323`, `soc_caps.h:194`。v3.0 で修正 [R3]）。
  - **v1.x の HWLP の状態バグ**: ループ最終命令の実行後に Dirty にならないので、カウンタ値を見て判定しています（`components/freertos/FreeRTOS-Kernel/portable/riscv/portasm.S:216-226`, `soc_caps.h:196`）。
- **推奨（推測）**: 初期段階では、**U モードには F だけを許可し、HWLP と PIE は OFF（不正命令）のまま**にします。そうすればコンテキストに含めるのは x1–x31、mepc、f0–f31、fcsr（と、Zcmt を使うなら jvt）だけで済みます。Rust の標準ターゲットは PIE/HWLP 命令を生成しないので、この方針で問題ありません。

### 5.4 デバッグとトリガ
- トリガは 3 本（HP）。type=2 (mcontrol) のみで、`mcontrol.u` ビットで U モードでも発火させられます（Reg 2.132 p.167）。トリガ CSR は M モードからしかアクセスできません（§2.11.3.5 p.162）。
- action=0 ならブレークポイント例外（mcause=3）。`tcontrol.mte/mpte` で M モードのトラップ中の再発火を抑止します（§2.11.3.3–2.11.3.4 p.161–162; Reg 2.130 p.165）。デバッガでユーザアプリをデバッグするなら、コンテキストスイッチ時にトリガ設定を切り替える必要があります（推測）。
- dcsr/dpc/dscratch は Debug Mode からしかアクセスできません（p.73）。
- JTAG: HP 用と LP 用の DTM がデイジーチェーンでつながっています。HP コアと LP コアを同時にはデバッグできません（§2.11.1.1 p.152）。デバッガのシステムバスアクセス (SBA) は Core0 のデータバス経由です（p.70）。PMP をロックするとデバッガのメモリ書き込みも制限されます（IDF が OCD 接続時に SRAM を RWX にするのはそのため, `cpu_region_protect.c:127-133`）。
- **デバッガの停止と `mret`（実機、doc/36）**: probe-rs がハートを止めて再開すると、カーネルがプロセスへ `mret` した後もプロセスが M モードで動くことがあった。`mcause`=0x3800000B（M モードからの `ecall`、MPP=3）で見つかる。PMP はロックしていないので、その間 M モードのプロセスには効かない。
  - 止めていたのは次の 3 つ（いずれも probe-rs）:
    - `Core` を作るとき（`misa` と `dcsr`）
    - 8/16 ビットのメモリアクセス（v1.x ではプログラムバッファを使う）
    - `Session` を捨てるとき
  - デバッガをつながずに 60 回の echo を流しても起きなかった。
  - 推測: 例外の処理中（EXPT_VLD、§5.2）に停止要求を受けたときの `dcsr.prv`/`dpc` の扱いが関係している。
  - 対策: RTT はハートを止めずに読む（`t5log rtt`）。止める使い方（`t5log status`、probe-rs の CLI、GDB）をした後は、再起動するまで隔離を信用しない。

---

## 6. ESP-TEE（M/U 分離の参考実装）

- **対応チップ**: `depends on IDF_TARGET_ESP32C6 || ESP32H2 || ESP32C5 || ESP32C61`（IDF6 `components/esp_tee/Kconfig.projbuild:2`）。SoC ごとのファイルは `subproject/main/soc/{esp32c5,c6,c61,h2}` だけで、master (2026-07-10) でも同じです。**P4 は未対応です。**
- **構造**: TEE = M モード、REE（FreeRTOS + アプリ）= U モード。
  - REE から TEE への呼び出しは `_u2m_switch: ecall; fence; ret`（`components/esp_tee/src/esp_tee_u2m_switch.S:10-13`）。
  - TEE 側の U-ecall ハンドラの処理（`esp_tee_vectors_clic.S:325-377`）:
    1. REE のレジスタを REE スタックに退避する。
    2. `_ns_sp` に REE の sp を保存する。
    3. TEE スタックに切り替える。
    4. mepc を `_tee_s_entry` にする。
    5. 割り込みの U 委譲を無効化する（Interrupt Matrix の `INTMTX_SIG_IDX_ASSERT_IN_SEC_REG` に書く）。
    6. MPP=M にして mret（M モードのままサービス関数へ）。
  - 戻りは TEE から M-ecall（mcause=11）を発行し、`_machine_ecall` で REE のコンテキストを復元して MPP=U、mret（`:276-322`）。
  - REE がクリティカルセクション中（uintthresh≠0）なら、mintthresh に uintthresh をミラーします（`:341-349`）。
- **割り込み**: REE (U) 用の割り込みは一度 M モードで受け（`_tee_ns_intr_handler`）、ucause/uintthresh を手動で設定してから、uepc を「U→M 復帰用 ecall」に向けて U モードのハンドラへ mret します（`:408-521`）。U モード割り込みのハードウェア委譲に頼らず、**M モードでいったん受けてソフトウェアで U モードへ転送する方式**です。自作 OS で「割り込みをユーザ空間ドライバへ通知する」場合の参考になります。
- **保護設定**（C5: `subproject/main/soc/esp32c5/esp_tee_pmp_pma_prot_cfg.c:68-167`）:
  - PMP で REE 側の領域だけを許可し、すべて L 付き。TEE の領域は PMP に載せないので、U モードは拒否されます。
  - TEE 自身の text/data は PMA (13–15) で RX/RW に固定。
  - 周辺は PMP で RW 全開にし、細かい制御は APM（TEE コントローラ）で行います。
  - P4 で同じことをするなら、APM の TEE コントローラの代わりに `PMS_COREn_UM_*` を使うことになります（推測）。

---

## 7. ESP Privilege Separation（ESP32-C3/S3 向け、2022）

ユーザーの記憶にある「Espressif が ESP32-C3 向けに出した M/U 特権分離の取り組み」は、**ESP Privilege Separation (Beta)** というフレームワークでした。
- リポジトリ: https://github.com/espressif/esp-privilege-separation
- 文書: https://docs.espressif.com/projects/esp-privilege-separation/en/latest/esp32c3/
- 告知ブログ: "Introducing ESP Privilege Separation"（Sachin Parekh, 2022-06-19）[R8]
- 事例紹介: "ESP Privilege Separation: A Case Study"（Shubham Kulkarni, 2022-07-05。RainMaker を protected 側に置く例）[R9]

**学会論文、arXiv、ホワイトペーパーは見つかりませんでした**（Web 検索の範囲。見つかったのは Espressif の公式ブログ、プログラミングガイド、ニュースページ、Hackster.io の記事と riscv.org への転載だけ）。以下は、ソースを `git clone --depth 1`（コミット f8867cb, 2022-12-12）して読んだ結果です。ファイルパスはリポジトリのルートからの相対パスです。

### 7.1 構成
- ファームウェアを独立した 2 つのバイナリに分けます（README.md の "Features"、`docs/en/technical-details/architecture.rst:9-26`）。
  - **protected_app**: ESP-IDF そのもの。FreeRTOS、Wi-Fi、LwIP、ドライバを含み、全メモリと全周辺にアクセスできます。`app_main` が入口です。
  - **user_app**: ビジネスロジックだけの薄いアプリ。
- 起動の流れ（`architecture.rst:52-57`）: ROM → IDF 2nd stage bootloader → protected_app。protected_app が権限を設定し、フラッシュの user パーティションから user_app をロードします。
- **user_app のロード方式**（`components/protected/esp_priv_access/esp_priv_access_image_utility.c:103-195`）:
  - user パーティションには、ESP-IDF 標準の `esp_image` 形式（ヘッダとセグメント）でイメージを置きます。
  - IRAM/DRAM 向けのセグメントは `spi_flash_read` で固定のロードアドレスへコピーします（`:186-193`）。
  - IROM/DROM のセグメントは、キャッシュ MMU（`Cache_Ibus_MMU_Set`/`Cache_Dbus_MMU_Set`）で専用の仮想領域にマップします。範囲は UIROM 0x4240_0000–0x4280_0000、UDROM 0x3C40_0000–0x3C80_0000（`:141-147`、`components/shared/esp_priv_build_utils/include/soc_defs.h:58-62`）。
  - **再配置はしません**。user_app は予約領域 `_reserve_w1_{iram,dram}` に合わせた専用のリンカスクリプト（`components/shared/esp_priv_build_utils/ld/user_app/esp32c3/user.ld`）で固定アドレスにリンクされます。protected 側と user 側のビルドがずれると assert で検出します（`esp_priv_access_setup.c:524-527`）。
  - エントリアドレスはイメージヘッダから取り、World Controller に「World1 の入口」として登録したうえで、user のメインタスクを生成します（`:503-528`）。
- **user_app 単体での更新と再起動**:
  - 専用の `user_ota` データパーティションで 2 面の OTA を行います（`components/protected/esp_priv_access/esp_priv_access_ota_utils.c:34-50`、`components/protected/esp_user_ota/`）。
  - 署名検証（user_app 用の secure boot）は `esp_priv_access_secure_boot.c` と `tools/esp_ps_secure_boot_util.py` が担当します。
  - user 側で例外が起きた場合や再起動が必要な場合、protected 側は user タスクだけを停止し、資源を解放して user_app を再ロードします（`esp_priv_access_setup.c:567-627`）。protected_app はリブートしません。

### 7.2 使った保護機構: PMP ではなく World Controller + PMS
- C3 のハードウェア（ESP32-C3 TRM v1.4 [R10]）:
  - CPU は M と U を実装しています（misa.U=1, §1.4.2 Register 1.6 misa p.36）。
  - PMP は **16 領域、最小粒度 4 B。ただし標準に非準拠**です。静的な優先順位がなく、重なった領域は「どれか 1 つに一致すれば許可」になります。NAPOT の最大は 1 GB（§1.8.2 p.57）。ESP-IDF の C3 用コードもこの制約に合わせ、重ならない TOR でアドレス空間を下から順に埋めています（IDF6 `components/esp_hw_support/port/esp32c3/cpu_region_protect.c:14-27`）。
  - **PMS（Permission Control）の章自身が PMP の不足を挙げています**: 「PMP は最大 16 領域で、多数の周辺や複数種類のメモリの管理には足りないことがある」「PMP は CPU しか制御できず、GDMA は制御できない」。そのため PMS で補完すると書かれています（§14.1 p.315）。チェックは PMP → PMS の順に行われ、PMP で失敗すれば例外、PMS で失敗すれば割り込みになります（同頁）。
  - PMS が「特権環境」と「非特権環境」を区別する方法は 2 つあり、`PMS_PRIVILEGE_MODE_SEL` で選びます。**0（既定）は World Controller の World0/World1、1 は CPU の M/U モード**です（§14.3 p.316–317）。
  - World Controller（15 章 p.413–）:
    - 電源投入時、CPU は Secure World (World0) にいます。
    - 「指定したアドレスの命令を実行した瞬間に World1 へ切り替わる」（WORLD_PREPARE / TRIGGER_ADDR / UPDATE を設定）。
    - World1 から World0 へは、割り込みと例外でしか戻れません（`docs/en/technical-details/world_and_permission_control.rst:33-65`）。
- **フレームワークが選んだのは World 方式です。user_app も CPU の M モードで動いています。** 根拠:
  - syscall ハンドラは `EXCCAUSE_ECALL = 11`（**ECALL from M**）で入口を判定しています（`components/protected/esp_syscall/src/riscv/syscall_handler.S:28`、IDF 用パッチ `idf-patches/privilege-separation_support_v4.4.3.patch` の vectors.S 変更部分）。
  - user へ戻るときは、MPP を変えずに World Controller の TRIGGER_ADDR に mepc を設定して mret しています（`syscall_handler.S:178-190, 235`）。

  つまり**分離は CPU の特権モードではなく、World タグ付きのバスアクセスを PMS が検査することで成り立っています**。
- なぜ U モード + PMP にしなかったのか、文書に明記はありません。考えられる理由（推測）:
  1. C3 の PMP は優先順位がなく、16 領域しかなく、DMA を守れないので、PMP 単独では足りない（TRM が自ら認めている）。
  2. PMS の SRAM/フラッシュキャッシュ/周辺に対する「split line」方式の設定が World ベースのほうが既存コードと合っていた。
  3. **同じフレームワークを Xtensa の ESP32-S3 にも展開する**ので（README "Supported SoCs"、`components/protected/esp_syscall/src/xtensa/`、`.../include/esp32s3/wcntl_ll.h`）、RISC-V の U モードに依存しない World 方式で共通化した。
  - 注意（推測・未検証）: World1 のコードも M モードなので、CSR（mtvec、mstatus、pmpcfg など）の書き換えを PMS では防げません。World Controller がトラップの入口を検査する機能（`esp_priv_access_setup.c:152-178` の `wcntl_ll_set_mtvec_base`/`set_entry_check`）がどこまで防げるかは確認できていません。
- **メモリ分離の具体例**（`esp_priv_access_setup.c:194-330`）:
  - IRAM/DRAM は PMS の「split line」で領域に分け、World1 には user 用の予約領域だけを RW または RX で許可します。
  - ROM、RTC、フラッシュの I/D キャッシュは World1 に許可しないか、user 用の 4 MB 窓だけを許可します。
  - 違反は PMS の割り込み（IRAM/DRAM/キャッシュ/PIF 用）で検出し、user_app を再起動します（`:104-150, 629-737`）。

### 7.3 システムコール
- **入口**: user 側のラッパが `a0`–`a6` に引数、`a7` に番号を入れて `ecall` します（`docs/en/technical-details/system-call.rst:20-35`、`components/user/syscall_wrapper/include/riscv/syscall_macros.h`）。
- **ハンドラ**（`syscall_handler.S:47-235`）:
  1. TCB の TLS に置いたタスクごとの**カーネルスタック**へ切り替える（`:52-75`、`docs/en/technical-details/design-choices.rst:4-19`。既定 2560 B）。
  2. mcause/mepc(+4)/mstatus を退避する（`:78-89`）。
  3. **割り込みを許可した状態で** `syscall_table[a7]` を呼ぶ（`:103-127`）。syscall の実行中もプリエンプトされます（design-choices.rst:35-37 "It is possible to context switch a task that is currently servicing a system call"）。
  4. 番号の範囲チェックを行う（`:113-115`）。
  5. 戻り値のポインタを検証する（`CONFIG_ESP_SYSCALL_VERIFY_RETURNED_POINTERS`, `:118-141`）。
  6. World1 への切り替えを予約して mret する（`:178-235`）。
- **テーブルの生成**:
  - `syscall.tbl`（約 190 行。"番号 ABI 名前 実装関数" の形式。例 `258 common vTaskDelay sys_vTaskDelay`）と、ユーザ定義の `CUSTOM_SYSCALL_TBL` を連結します。
  - それをシェルスクリプト（`syscallhdr.sh`/`syscalltbl.sh`）で処理して、`syscall_def.h`（`__NR_*`）とテーブルを生成します（`components/protected/esp_syscall/CMakeLists.txt:35-71`、`components/protected/esp_syscall/syscall/syscall.tbl`）。
  - ABI が `common` のものは、user 側のリンク時に `--defsym=<sym>=usr_<sym>` で置き換えます。これにより、user コードは ESP-IDF と同じ API 名（`xTaskCreate` など）のまま syscall を呼べます（`system-call.rst:167-193`）。
- **引数とポインタの検証**:
  - 実装側の `sys_*` 関数で、ポインタが user 領域にあるかを `is_valid_user_d_addr()` や `is_valid_udram_addr()` で個別にチェックします（`components/protected/esp_syscall/src/esp_syscalls.c:112-122` など、99 か所）。
  - FreeRTOS のキュー、タスク、タイマなどのハンドルは、そのまま user に渡さず、`esp_map` で ID に変換しています（`components/protected/esp_syscall/include/esp_map.h:21-26`）。
- **コスト**: User→Protected 125 サイクル、Protected→User 95 サイクル（`system-call.rst:195-215`）。
- **user 側の割り込みとイベント**:
  - user が登録したハンドラは **ISR コンテキストでは実行しません**。protected 側の割り込みやイベントを受けて、user 空間の**ディスパッチャタスク**が呼び出します（"softisr" と呼ぶことを提案）。
  - ディスパッチャの優先度を最高にした場合で、+1275 サイクルの遅延です（`design-choices.rst:39-55`）。
- ヒープは protected と user で別のインスタンスを持ちます。syscall が user 側のメモリを必要とする場合は、user が自分で確保して渡します（`design-choices.rst:75-88`）。

### 7.4 周辺アクセス
- 既定では World1 から全周辺を取り上げます（`esp_priv_access_setup.c:400-448` の `esp_priv_access_revoke_world1_peripheral_permissions`）。`esp_priv_access_set_periph_perm(periph, world, perm)` で周辺ごとに許可できます（`:739-741`、PMS の PIF 権限）。
- ただし推奨されているのは**ドライバを protected 側に置き、VFS（open/read/write/ioctl）の syscall で公開する**方式です。例として SPI を VFS でラップしたもの（`components/shared/drivers/spi/vfs_spi.c`）があります（`design-choices.rst:57-73`）。

### 7.5 対応チップ、開発状況、制限
- 対応: **ESP32-C3 と ESP32-S3 のみ**（README "Supported SoCs"）。ESP-IDF v4.3/v4.4 にパッチ（`idf-patches/privilege-separation_support_v4.{3,4}.3.patch`）を当てて使います。C6/P4 への展開はありません。
- 状態: "Beta"。**最後の push は 2022-12-12** です。GitHub 上はアーカイブされていませんが（`archived: false`、2026-10 時点の API）、実質的に開発は止まっています。
- 後継として明示された資料は見つかりませんでした。ただ、C6 以降の ESP-TEE（§6）は「M モード = 信頼側、U モード = 非信頼側、ecall で遷移、APM で周辺を制御」という同じ発想を**実際の RISC-V U モード**で実装し直したものと位置づけられます（推測）。
- 既知の制限（文書から）:
  - user_app は 1 つで、固定アドレスにリンクする（再配置なし）。
  - カーネルスタックはタスクあたり 2560 B を追加で消費する。
  - user の ISR は遅延実行のみ。
  - ネイティブの IDF API の一部は `usr_` 版を使う必要がある。

### 7.6 ESP32-P4 への応用可能性
- **P4 には World Controller がなく、C5/C6 の TEE コントローラ（security mode）もありません**（§3.5）。P4 の APM は、C3 PMS の `PMS_PRIVILEGE_MODE_SEL=1` 側に相当する「**CPU の M/U モードそのもの**」で判定します（`PMS_COREn_MM_*` / `PMS_COREn_UM_*`, TRM §20.3.1 p.1588, §20.4.1 p.1591）。したがって、**P4 では C3 の World 方式は再現できず、本物の U モード + PMP（+ APM の UM ビット）で組むことになります**。目標アーキテクチャ（カーネル = M、アプリ = U）にはむしろ素直に合います。
- C3 との違いで有利な点:
  - P4 の PMP は標準どおり**静的優先順位があり、重なりを使える**ので、「デフォルト拒否 + 例外的な許可」を少ないエントリで書けます。
  - 割り込み、ecall、mret による U との往復はハードウェアが MPP を管理するので、World の切り替えレジスタ操作は不要です。
- 不利な点、注意点:
  - P4 の APM はキャッシュ経由のメモリ（L2MEM、PSRAM、フラッシュ XIP）と TCM を見ません（表 20.1-1 p.1586）。**C3 の PMS の split line のような「メモリをバス側で World ごとに分割する」手段がありません。** メモリの隔離は PMP だけが頼りです。
  - 粒度は 128 B（C3 は 4 B）。
  - **v1.x（Tab5）は 16 エントリ**。§3.4 のロック済みエントリ問題を避けた前提でのエントリ見積もり（推測）:
    - カーネル側は「U には見せない」ので基本的にエントリ不要。
    - user 用は、text (RX) + rodata (R) + data/bss/heap/stack (RW) で NAPOT なら 3 エントリ、TOR なら 4–6 エントリ。
    - これに共有バッファ 1 つ、MMIO 窓 0–2 つ（APM の UM ビットで細かく絞れるので、周辺領域全体を 1 エントリで許可してもよい）。
    - キャッシュ有り/無しのエイリアスは、U に許可しなければ PMP の既定拒否で塞がるので追加エントリは不要。
    - **アプリ 1 つあたり 4–8 エントリで収まり、16 エントリでも C3 方式相当（protected + 1 user_app）の構成は成り立ちます。** 複数のアプリを同時に保護するには、コンテキストスイッチ時に PMP を書き換える必要があります。
  - DMA 経由の迂回は C3 と同じく PMP では防げません。P4 には DMA APM がありますが、**v1.x では APM-560**（§3.5）があるため、DMA を U に渡さない方針が無難です。
- **流用できる設計要素**（推測）:
  - タスクごとのカーネルスタックと TLS による管理。
  - syscall 実行中のプリエンプション許可。
  - syscall テーブルの自動生成（`.tbl` → `__NR_*`）。Rust なら build.rs やマクロで同じことができます。
  - ハンドルの ID 化（esp_map）。
  - 「user の ISR はディスパッチャタスクで遅延実行」。
  - 「ドライバは protected 側に置き、VFS 風の syscall で公開」。
  - user イメージは `esp_image` 形式でパーティションに置き、署名検証してからロードし、単体で OTA と再起動ができる。

---

## 8. OS 設計への含意（提案）

1. **ブート経路**: ESP-IDF 2nd stage bootloader を使う場合は `CONFIG_BOOTLOADER_REGION_PROTECTION_ENABLE=n` にし、カーネル起動直後に pmpcfg と pma_cfg を読んでロックされていないことを確認します（ロックされていたら panic）。
2. **PMP の割り当て案（v1.x、16 エントリ）**:
   - 0–5: カーネル用（ロックなし。M モードは無視されるので「U モードに見せない」効果だけ）
   - 6–11: カレントタスク用の動的エントリ（コンテキストスイッチで書き換え。例: text RX、rodata R、data+heap RW、stack RW、共有/IPC RW、MMIO 窓）
   - 12–15: 予備

   TOR を使うと 1 領域に 2 エントリ必要になるので、**NAPOT（2 の冪境界）で配置できるようアプリのロードアドレスとサイズを揃える**とエントリを節約できます。v3.x なら 32 エントリあるので余裕があります。
3. **カーネル自身の W^X**: L ビットを立てると M モードにも適用されますが、解除できなくなります。開発中は L を立てず、PMA でカーネルの text を RX に固定するという ESP-TEE と同じ手もあります（PMA は U モードにも効くので、ユーザ領域には使えない点に注意）。
4. **アプリの動的ロード**: MMU がないので、アプリは位置独立 (PIE) にするかロード時に再配置します。ロード先のメモリ（L2MEM、PSRAM）にロードしてから **fence.i** とキャッシュ同期を行います（統合時注記: 文書 02 §2.2 によると ESP-IDF 6.0 は P4 で fence.i を使っておらず、キャッシュコントローラ / ROM 関数による writeback + invalidate を使う。fence.i が効くかは要実機確認）（PSRAM や flash の XIP 領域から実行する場合のキャッシュ整合性は別資料で確認）。
5. **syscall ABI**: a7 = 番号、a0–a5 = 引数、a0 = 戻り値（Linux 風）。トラップ入口は `csrrw sp, mscratch, sp`（あるいは mscratchcsw）で即座にカーネルスタックへ切り替えます。lock-up 対策として、トラップ入口で U モードの sp は信用しません。
6. **ユーザポインタの検証**: `mstatus.MPRV=1` かつ MPP=U で、ロード/ストアに PMP 検査を効かせられます。ただし違反するとトラップ中の例外となって **lock-up するおそれ**があります。ソフトウェアで範囲を事前にチェック（タスクのメモリ領域と照合）する方式が安全です（推測）。
7. **割り込み**: すべて M モードで受けます。v1.x/v3.x の閾値 API の違い（MMIO か CSR か）を HAL で吸収します。ティックは SYSTIMER。U モードのアプリへはイベントやメッセージで通知します。
8. **MMIO**: 既定ではすべて PMP で拒否します。ユーザ空間ドライバを作るなら「PMP の窓 + APM UM_ALLOW」の 2 段にし、DMA を持つ周辺は DMA APM を設定するかカーネル管理にします。APM、Interrupt Matrix、CLIC、CLINT、SYSREG/CLKRST のレジスタは絶対に U モードへ開放しません。
9. **拡張状態**: 当面は F だけ（mstatus.FS で遅延保存）。HWLP/PIE は OFF。v1.x の EXT_ILL バグに注意します（FS=Off からのトラップ判定にオペコードのデコードが必要）。
10. **LP コアと DMA は隔離境界の外**です。カーネルが管理します。

---

## 9. 未確認事項 / 要実機確認

- [ ] Tab5 実機のチップリビジョン（eFuse）。v1.0 なら PMP 16 エントリ、非標準 CLIC、U 委譲なし、という前提を確認する。
- [ ] v1.x で misa.U=1 になっていて、mstatus.MPP=0 → mret で実際に U モードに入れるか（v1.x 用の IDF ヘッダに `PMS_COREn_UM_*` があるので U モード自体は存在すると推測）。
- [ ] v1.x で U モード実行中に M 割り込みが mstatus.MIE と無関係に入るか、mintthresh（MMIO）の影響を受けるか。
- [ ] v1.x の PMP L ビットの挙動（v3.x で「改善」とあるだけで、v1.x の問題の中身は不明 [R3]）。XL ビットが v1.x にあるか。
- [ ] ROM / ブートローダが pma_cfg・pmpcfg をロックした状態でカーネルに制御を渡すか（`CONFIG_BOOTLOADER_REGION_PROTECTION_ENABLE=n` の効果を含めて）。
- [ ] mscratchcsw/mscratchcswl が v1.x にあるか。
- [ ] ミスアラインのロード/ストアの扱い（mcause 4 が存在しない）。→ **一部確認（11 §2.3）**: PMP で禁止された領域へのミスアラインのストアは 7（アクセスフォールト）になった（6 ではない）。ハードウェアがミスアラインを処理するという推測と矛盾しない。PMP の境界をまたぐミスアラインアクセスの挙動（C6/H2 には `SOC_CPU_MISALIGNED_ACCESS_ON_PMP_MISMATCH_ISSUE` があるが、P4 には定義なし）。
- [ ] デバッガの停止・再開の後、`mret` しても U モードに入らないことがある仕組み（§5.4、doc/36）。再現は doc/36 §2。Espressif に問い合わせる候補。v3.x でも起きるか。
- [ ] U モードから fxcr / cpu_gpio_* / jvt / uhwloop* / hpmcounter にアクセスできるか（CSR の特権チェック）。
- [ ] CLINT の mtime のクロック周波数と、`time` CSR の源。
- [ ] clicintattr.MODE が書き込み可能か（TRM は R/W、IDF ヘッダは RO 固定）。
- [ ] misa.F=0 の表記が実機でも同じか。
- [ ] Lock-up 時にハードウェアがリセットするか（`SOC_CPU_HAS_LOCKUP_RESET` の具体的な動作）。
- [ ] ESP Privilege Separation の C3 で、World1（M モード）のコードが CSR を書き換えるのを World Controller がどこまで防いでいるか（P4 設計には直接影響しないが、参考実装の安全性評価として）。
- [ ] APM レジスタ (PMS) の物理アドレスと、PMP で塞ぐべき範囲（TRM 表 9.3-2 で確認する。別担当の領域）。

---

## 10. 参考文献

- [R1] ESP32-P4 Technical Reference Manual, Pre-release v0.7 (2026-08-20). https://www.espressif.com/sites/default/files/documentation/esp32-p4_technical_reference_manual_en.pdf
  - 2 章 High-Performance CPU（§2.1–2.2 p.68–70、§2.4 p.71、§2.5 p.71–111、§2.6–2.7 p.111–128、§2.8–2.9 p.129–141、§2.10 p.142–150、§2.11 p.152–168、§2.13 p.172–179）
  - 5 章 Low-Power CPU（p.633–657）
  - 13 章 Interrupt Matrix（p.1224–1266）
  - 16 章 System Timer（p.1507–）
  - 20 章 Permission Control (PMS)（p.1586–1686）
  - Revision History（p.3700）
- [R2] ESP-IDF ローカルソース
  - `~/esp-idf/6.0`（v6.0-beta1-624-g6534932340）
    - `components/esp_hw_support/port/esp32p4/cpu_region_protect.c`
    - `components/esp_hw_support/port/esp32p4/Kconfig.hw_support`
    - `components/soc/esp32p4/include/soc/{soc_caps.h,soc.h,clic_reg.h,interrupt_reg.h}`
    - `components/soc/esp32p4/register/hw_ver{1,3}/soc/`
    - `components/riscv/include/riscv/{csr.h,csr_clic.h}`
    - `components/riscv/vectors.S`
    - `components/freertos/FreeRTOS-Kernel/portable/riscv/portasm.S`
    - `components/bootloader_support/src/bootloader_mem.c`
    - `components/bootloader/Kconfig.projbuild`
    - `components/soc/project_include.cmake`
    - `components/esp_tee/`（`Kconfig.projbuild`、`src/esp_tee_u2m_switch.S`、`subproject/main/arch/riscv/esp_tee_vectors_clic.S`、`esp_tee_vector_table_clic.S`、`subproject/main/soc/esp32c5/esp_tee_pmp_pma_prot_cfg.c`）
  - `~/esp-idf/master`（d0b65797c30）: `components/esp_tee/subproject/main/soc/`（P4 なしを確認）
- [R3] ESP32-P4 Chip Revision v3.x User Guide v1.1 (2026-08). https://documentation.espressif.com/esp32-p4-chip-revision-v3.x_user_guide_en.html （§1.1 High-Performance CPU、§1.9 Interrupt Matrix、§1.11 Permission Control）
- [R4] ESP32-P4 Series SoC Errata. https://docs.espressif.com/projects/esp-chip-errata/en/latest/esp32p4/03-errata-description/index.html （APM-560 ほか）／ v1.0 タグ: https://docs.espressif.com/projects/esp-chip-errata/en/latest/esp32p4/_tags/v1-0.html
- [R5] Apache NuttX, "M5Stack Tab5" board documentation（Tab5 は ESP32-P4 rev v1.0、`CONFIG_ESP32P4_SELECTS_REV_LESS_V3=y`）. https://nuttx.apache.org/docs/latest/platforms/risc-v/esp32p4/boards/esp32p4-tab5/index.html
- [R6] The RISC-V Instruction Set Manual, Volume II: Privileged Architecture v1.10, §3.1.6.1 "Privilege and Global Interrupt-Enable Stack in mstatus register"（上位特権の割り込みは、下位特権の実行中は常に有効）. https://riscv.org/specifications/privileged-isa/
- [R8] Sachin Parekh, "Introducing ESP Privilege Separation", Espressif Developer Blog, 2022-06-19. https://developer.espressif.com/blog/introducing-esp-privilege-separation/ ／ リポジトリ https://github.com/espressif/esp-privilege-separation （コミット f8867cb, 2022-12-12）／ プログラミングガイド https://docs.espressif.com/projects/esp-privilege-separation/en/latest/esp32c3/index.html （World and Permission Control: https://docs.espressif.com/projects/esp-privilege-separation/en/latest/esp32c3/technical-details/world_and_permission_control.html ）／ ニュース https://www.espressif.com/en/news/ESP-Privilege-Separation ／ Hackster.io 記事 https://hackster.io/news/espressif-launches-esp-privilege-separation-giving-the-esp32-c3-a-secure-split-personality-b8540bb535b5
- [R9] Shubham Kulkarni, "ESP Privilege Separation: A Case Study", Espressif Developer Blog, 2022-07-05. https://developer.espressif.com/blog/esp-privilege-separation-a-case-study/
- [R10] ESP32-C3 Technical Reference Manual v1.4. https://www.espressif.com/sites/default/files/documentation/esp32-c3_technical_reference_manual_en.pdf （§1.4.2 Register 1.6 misa p.36、§1.8 Memory Protection p.57、14 章 Permission Control (PMS) p.315–、15 章 World Controller (WCL) p.413–）
- [R11] ESP32-P4 Series SoC Errata v1.4, §3.11 [APM-560] p.12
- [R7] RISC-V Core-Local Interrupt Controller (CLIC) specification. https://github.com/riscv/riscv-fast-interrupt （TRM が準拠を表明している版は 2023-10-10。本調査では原文の該当節を取得できなかったため、CLIC の挙動は TRM の記述を優先した）
