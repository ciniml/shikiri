# 02. ESP32-P4 のメモリ構成とアプリ配置

対象: M5Stack Tab5（ESP32-P4）上で Rust 自作 OS を作る。カーネルは M モード、アプリは U モード。動的ロードとカーネル・アプリの更新を想定する。
調査日: 2026-10-29。出典の略号は末尾の「参考文献」を参照。

---

## OS 設計への結論

- **アドレス空間は命令バスとデータバスで共通**。予約域以外のアドレスはどれも命令フェッチとデータアクセスの両方ができる [TRM §9.3.1 p.902]。ハーバード型の IRAM/DRAM 分離（ESP32/S3 にあるもの）はない。ESP-IDF も L2MEM を 1 つの `sram_seg (RWX)` として扱っている [IDF memory.ld.in:84]。
- **実行できる領域**: L2MEM、PSRAM（MMU 経由）、フラッシュ XIP（MMU 経由）、HP SPM（8KB）、LP SRAM。ハードウェア上はどこからでもフェッチできる。ESP-IDF は外部メモリ・ROM・L2MEM を含む 0x4000_0000–0x4FFF_FFFF を PMA で cacheable かつ RWX に設定している [IDF cpu_region_protect.c:79-81]。実行の可否は **PMP（と PMA）の設定だけで決まる**。
- **外部メモリ MMU**: ページは 64KB 固定。エントリはフラッシュ用 1024 個、PSRAM 用 1024 個で、それぞれ 64MB の仮想窓になる。エントリは MMIO レジスタ（index/content）で **実行時に書き換えられる** [IDF ext_mem_defs.h:19,84-96; mmu_ll.h:126-141,265-300]。
  - フラッシュ上の 64KB 境界にある任意のオフセットや、PSRAM 上の任意の物理ページ群を、任意の 64KB 境界の仮想アドレスへ連続した形でマップできる。**アプリを固定仮想アドレスにリンクすれば、再配置は不要にできる。**
  - MMU は **全コア・全特権モードで共有される 1 枚のテーブル**で、エントリに権限ビットはない（valid・target・暗号化ビットのみ）。プロセスごとのアドレス空間の切り替えと保護は、MMU の付け替えと PMP の組み合わせで自前で作るしかない。
- **Tab5 のサイズは窓にすべて収まる**: フラッシュ 16MB は 256 ページで 64MB 窓に全域マップ可能。PSRAM 32MB は 512 ページで 64MB 窓に全域マップ可能。
- **キャッシュ保守は fence.i に頼らない**: コードを書き込んだら「D キャッシュ（L1、外部メモリなら L2 も）を writeback → I キャッシュ（両コアの L1、外部メモリなら L2 も）を invalidate」をキャッシュコントローラ操作で行う。ESP-IDF 6.0 には P4 で `fence.i` を使う箇所が一つもない。ROM の `Cache_WriteBack_Addr` / `Cache_Invalidate_Addr` を使っている [IDF cache_ll.h:483-507,644-665; esp_image_format.c:257-261]。fence.i が L1 I キャッシュに効くかは未確認。
- **アプリ配置の推奨**: **PSRAM にロードし、PSRAM MMU で固定仮想アドレス（例: 0x4A00_0000〜）にマップする**方式を主にする。SD からのロードもフラッシュからのロードも同じ経路で扱え、フラッシュ書き込み（OTA 更新）中もアプリが止まらない。フラッシュ XIP 直接マップは「読むだけ・コピーなし・常駐」系の最適化として残す。L2MEM はカーネル（とホットパス）用に取っておく。
- **チップリビジョン差が大きい**: Tab5（無印）に載っているのは **ESP32-P4 v1.x（v1.0 の報告あり）**。v3.x 搭載品は「Tab5X」として区別されている [M5GFX PR#343]。v1.x と v3.x では **L2 キャッシュが L2MEM の上端に取られるか下端に取られるか**、ROM の予約域、**PMP エントリ数（16 か 32 か）**、最大クロック（360MHz か 400MHz か）、PSRAM 最高速度（200MHz か 250MHz か）が違う。ESP-IDF でも v1.x と v3.x のバイナリは互換性がない [IDF Kconfig.hw_support:4-11; v3 UG]。**OS は v1.x 前提で作り、v3.x も扱えるようにリンカスクリプトを分けておく**。

---

## 1. アドレスマップ

HP CPU から見たアドレスマップ。主な出典は TRM Table 9.3-1 [TRM §9.3.1 p.904-905] と ESP-IDF soc.h [IDF soc.h:147-223]。

| 範囲 | サイズ | 内容 | キャッシュ | 命令フェッチ | 備考 |
|---|---|---|---|---|---|
| 0x0000_0000–0x1FFF_FFFF | – | 予約 | – | – | ESP-IDF は PMA で「無効」に設定 [cpu_region_protect.c:50-51] |
| 0x2000_0000–0x2FFF_FFFF | – | CPU サブシステム（CLIC/CLINT・デバッグ） | 非 | – | ESP-IDF の `SOC_CPU_SUBSYSTEM_LOW/HIGH` [soc.h:217-218]。TRM の表ではここも「Reserved」と書かれている |
| 0x3010_0000–0x3010_1FFF | 8 KB | **HP SPM**（ESP-IDF 6.0 では「TCM」、master では「SPM」） | 非（直結。1 アクセス 2 サイクル） | 可 | アトミック命令とパリティに対応 [TRM §9.3.2 p.906,908]。IDF のセグメントは `tcm_idram_seg (RX)` [memory.ld.in:56] |
| 0x3FF0_0000–0x3FF1_FFFF | 128 KB | HP CPU 周辺（Trace、Bus/L2MEM/SPM Monitor、**キャッシュコントローラ** 0x3FF1_0000） | 非 | – | [TRM Table 9.3-2 p.912; reg_base.h:8,30]。U モードからは PMP で塞ぐ |
| **0x4000_0000–0x43FF_FFFF** | 64 MB | **外部フラッシュの仮想窓**（Flash MMU） | L1+L2 | 可 | [TRM §9.3.3.1 p.909] |
| 0x4400_0000–0x47FF_FFFF | – | 予約 | – | – | |
| **0x4800_0000–0x4BFF_FFFF** | 64 MB | **外部 PSRAM の仮想窓**（PSRAM MMU） | L1+L2 | 可 | [TRM §9.3.3.1 p.909] |
| 0x4FC0_0000–0x4FC1_FFFF | 128 KB | HP ROM | L2 経由 | 可 | ROM の cache/flash 関数がある |
| **0x4FF0_0000–0x4FFB_FFFF** | 768 KB | **HP L2MEM**（一部を L2 キャッシュとして使う） | L1 経由（L2 キャッシュは通らない） | 可 | CPU の半分のクロックで動作し、ECC 付き [TRM §9.3.2 p.905-908; IDF cache_ll.h:1083-1086] |
| 0x5000_0000–0x500F_FFFF | 1 MB | HP 周辺 MMIO | 非 | – | MSPI（Flash MMU 0x5008_C000、PSRAM MMU 0x5008_E000）を含む |
| 0x5010_0000–0x5010_3FFF | 16 KB | LP ROM | – | – | |
| 0x5010_8000–0x5010_FFFF | 32 KB | **LP SRAM** | 非（AHB 経由） | 可 | 末尾 0x5010_FA80 以降は LP ROM のスタックとデータ [memory.ld.in:43-45] |
| 0x5011_0000–0x5012_FFFF | 128 KB | LP 周辺 MMIO | 非 | – | |
| 0x8000_0000–0x83FF_FFFF | 64 MB | フラッシュの非キャッシュ（直結）エイリアス | 非 | 可 | 主にデバッグ用で遅い [TRM §9.3.3.1] |
| 0x8800_0000–0x8BFF_FFFF | 64 MB | PSRAM の非キャッシュエイリアス | 非 | 可 | |
| 0x8FC0_0000–0x8FC1_FFFF | 128 KB | HP ROM の非キャッシュエイリアス | 非 | 可 | |
| 0x8FF0_0000–0x8FFB_FFFF | 768 KB | L2MEM の非キャッシュエイリアス | 非 | 可 | `CACHE_LL_L2MEM_NON_CACHE_ADDR(a) = a + 0x4000_0000` [cache_ll.h:27] |

補足
- 0x4xxx_xxxx を cacheable として扱うか non-cacheable として扱うかは **CPU の PMA 設定で決まる** [TRM Table 9.3-1 脚注]。PMA は 16 エントリあり、粒度は 128B、M モードからしか設定できない。PMA は **特権モードに関係なく** 全アクセスに適用され、L ビットを立てるとリセットまで変更できない [TRM §2.10.2 p.147-149]。
- 0x8xxx_xxxx の直結エイリアスは、U モード分離では **抜け道** になる。PMP で必ず塞ぐこと。ESP-IDF の v3 用設定ではキャッシュ側と非キャッシュ側の両方に同じ PMP を設定している [cpu_region_protect.c:89-94]。
- DMA から見えるのは L2MEM（0x4FF0_0000〜）、フラッシュ窓、PSRAM 窓 [TRM §9.3.4 p.911-912]。

### 1.1 L2MEM の内訳（リビジョンで異なる）

TRM（v3.x 準拠と思われる）によると、L2MEM 768KB を L2 RAM と L2 キャッシュでどう分けるかは 4 通りから選ぶ [TRM §9.3.2 p.906]。

| Config | L2 RAM | L2 キャッシュ |
|---|---|---|
| 0 | 256 KB | 512 KB |
| 1 | 512 KB | 256 KB |
| 2 | 640 KB | 128 KB |
| 3 | 768 KB | 0 KB |

ESP-IDF の Kconfig で選べるのは 128/256/512KB だけで、既定は 128KB / 64B ライン [IDF Kconfig.cache:3-21]。0KB（Config 3）は選べない。外部メモリへのアクセスは L2 キャッシュ経由が前提になっている（`CACHE_LL_EXT_MEM_VIA_L2CACHE` [cache_ll.h:39]）ため、PSRAM やフラッシュを使うなら 0KB は実質使えない（推測）。

**v1.x（Tab5 無印）** の配置。`CONFIG_ESP32P4_SELECTS_REV_LESS_V3=y` の場合で、L2 キャッシュ 128KB のときの数値を示す [IDF memory.ld.in:22-33,80-82; bootloader.ld.in:13-16,52]。

| 範囲 | 用途 |
|---|---|
| 0x4FF0_0000–0x4FF2_CBD0（約 179 KB） | `sram_low`（RWX）。アプリ用 |
| 0x4FF2_CBD0–0x4FF3_FFFF | 2nd stage bootloader が使う領域と ROM の予約域。0x4FF2_96B8–0x4FF3_AFC0 は DL モード専用のバッファ、0x4FF3_AFC0–0x4FF3_FBA4 は CPU1 の起動スタック、**0x4FF3_FBA4–0x4FF4_0000 は ROM の .bss/.data** |
| 0x4FF4_0000–0x4FFA_0000（384 KB） | `sram_high`（ESP-IDF の定義は RW）。サイズは 0x80000 − L2 キャッシュ容量 |
| 0x4FFA_0000–0x4FFB_FFFF（128 KB） | **L2 キャッシュ（上端）** |

**v3.x** の配置 [IDF memory.ld.in:18-21; bootloader.rev3.ld.in]。v3 UG には「キャッシュ領域を L2MEM の上端ではなく下端から取るように変更した」とある。

| 範囲 | 用途 |
|---|---|
| 0x4FF0_0000–(+L2 キャッシュ容量) | **L2 キャッシュ（下端）** |
| 〜0x4FFA_EFC0 | `sram_seg`（RWX） |
| 0x4FFA_EFC0–0x4FFB_FFFF | bootloader と ROM の予約域（ROM スタックの開始は 0x4FFB_CFC0 [soc.h:221]） |

ESP-IDF master では境界が少しずれている（SRAM_END 0x4FFA_DFC0、SRAM_LOW_END 0x4FF2_BBD0 [master memory.ld.in]）。自作 OS では **自前の 2nd stage ローダ（またはカーネル本体）が ROM の予約域に触らない** ようにする。L2 キャッシュのサイズ設定には ROM の `Cache_Set_L2_Cache_Mode` を使う [esp_rom cache.h:467]。

---

## 2. キャッシュ

### 2.1 構成 [TRM §9.3.3.2 p.909-910; DS §4.1.3.3 p.47]
- L1 I キャッシュ: 16 KB、64B ライン、4-way。ESP-IDF は `CACHE_MAP_L1_ICACHE_0` と `_1` を別々に扱っている [cache_ll.h:483-492]。このため **コアごとに I キャッシュがある** と読める（推測。16KB がコアごとの値か合計かは TRM に明記がない）。
- L1 D キャッシュ: 64 KB、64B ライン、2-way、write-back と write-through に対応。ESP-IDF の API では `CACHE_MAP_L1_DCACHE` が 1 つしかなく、**両コアで共有** していると読める（推測）[cache_ll.h:502-507]。
- L2 キャッシュ: 128/256/512 KB、64B または 128B ライン、8-way。**外部メモリ（フラッシュと PSRAM）と ROM 専用**。
- **L2MEM は L1 キャッシュを経由してアクセスされる**（`SOC_CACHE_INTERNAL_MEM_VIA_L1CACHE` [soc_caps.h:183]）。L2MEM のアドレスの保守は level 1 だけ、外部メモリの窓は level 2 まで行う [cache_ll.h:1075-1093]。
- 操作の種類: Write-back、Clean、Invalidate（範囲指定と全体）、Preload、Lock/Unlock [TRM §9.3.3.3 p.910-911]。ESP-IDF はこれらを ROM 関数 `Cache_WriteBack_Addr(map, addr, size)` と `Cache_Invalidate_Addr(...)` で呼んでいる。ROM 上のアドレスは例えば `Cache_Invalidate_Addr = 0x4fc003e4` [esp32p4.rom.ld:186]。ROM の ld は ECO ごとに分かれている（`esp32p4.rom.eco5.ld`）ので、ROM 関数のアドレスがリビジョン間で同じかは要確認。

### 2.2 コード書き込み後に必要な手順
1. コピー先のアドレスへ書き込む。キャッシュ経由の通常ストアでよい。
2. **D キャッシュを writeback する**。L2MEM なら L1 D だけ、PSRAM なら L1 D と L2 の両方。
   - ESP-IDF の bootloader も、L2MEM にコードをロードした後は D キャッシュを writeback している。コメントは「dcache 経由で書いたデータを icache で読むので writeback が要る」[esp_image_format.c:257-261, 722-727]。
3. **I キャッシュを invalidate する**。両コアの L1 I（`CACHE_MAP_L1_ICACHE_MASK`）と、外部メモリなら L2 も。`cache_hal_invalidate_addr` は全種別と全 ID を invalidate する [cache_hal.c:266-278]。
4. その領域を実行する。
- **fence.i**: TRM では命令リストの中に FENCE.I が出てくるだけで、キャッシュとの関係は書かれていない（例: §2.6.6.3 LR.W の予約を解除する条件の一つ p.117）。ESP-IDF 6.0 には fence.i の使用例がない。**L1 I キャッシュの一貫性は保証されていないものとみなし、上の 2 と 3 を必ず行う**。fence.i はパイプラインのフラッシュ用として併用しておく（推測。実機で検証が要る）。
- DMA とキャッシュは一貫していない。DMA の前後には `esp_cache_msync` に相当する writeback/invalidate が要る [esp_cache.h:25-81]。

---

## 3. 外部メモリ MMU

### 3.1 仕様（ESP-IDF の定義から）
| 項目 | 値 | 出典 |
|---|---|---|
| ページサイズ | **64 KB 固定**（`mmu_ll_set_page_size` は 64KB 以外を assert で弾く） | ext_mem_defs.h:19; mmu_ll.h:126-141 |
| MMU の数 | 2（Flash MMU = id 0、PSRAM MMU = id 1） | mmu_ll.h:24-25; soc_caps.h:414-417 |
| エントリ数 | **各 1024**。64KB × 1024 = 64MB で窓の大きさと一致する | ext_mem_defs.h:86-87 |
| 仮想ベース | フラッシュ 0x4000_0000、PSRAM 0x4800_0000。I と D で同じ仮想アドレスを共有する | ext_mem_defs.h:95-96; soc_caps.h:416 |
| エントリ形式（Flash） | bit[10:0] 物理ページ番号、bit12 VALID、bit13 SENSITIVE（暗号化） | ext_mem_defs.h:57-77 |
| エントリ形式（PSRAM） | bit[9:0] 物理ページ番号、bit10 ACCESS_PSRAM、bit11 VALID、bit12 SENSITIVE | 同上 |
| アクセス方法 | Flash: `SPI_MEM_C_MMU_ITEM_INDEX_REG` = 0x5008_C380 にエントリ番号を書き、`..._CONTENT_REG` = 0x5008_C37C に値を書く。PSRAM: 0x5008_E380 と 0x5008_E37C | mmu_ll.h:265-300; spi_mem_c_reg.h; reg_base.h:52-54 |
| 物理アドレス上限 | Flash は 11bit → 128MB、PSRAM は 10bit → 64MB。`*_MAX_PADDR_PAGE_NUM`（32768 / 16384）とは食い違うが、実際にマップできるのは値マスクの範囲 | ext_mem_defs.h:77-85 |
| エントリの権限ビット | **なし**（RWX の区別も U/M の区別もない） | 同上 |

TRM v0.7 の本文には MMU の章がなく、「MMU で物理アドレスに変換する」と一文あるだけ [TRM §9.3.3.1 p.909]。上の表は ESP-IDF のソースから読み取ったもの。データシートには「キャッシュ経由で 64KB ブロックを 64MB の命令/データ空間にマップできる」とある [DS §4.1.3.1 p.46]。

### 3.2 実行時の書き換え
- ESP-IDF の `esp_mmu_map()` / `esp_mmu_unmap()` は、実行時に任意の物理領域（64KB 境界）を空いている仮想領域へマップする。手順は「**キャッシュ停止（P4 では他コアの停止と割り込み禁止を伴う） → エントリ書き込み → バス有効化 → その仮想範囲の cache invalidate → キャッシュ再開**」[esp_mmu_map.c:377-395, 437-456]。P4 ではキャッシュの freeze を使わず、disable と enable で済ませている（コメントに「他コアのストールに制限がある」とある）。
- マップできる領域は、Flash 窓・PSRAM 窓とも EXEC/READ/WRITE/8bit/32bit がすべて許可されている [esp_mm/port/esp32p4/ext_mem_layout.c:17-34]。
- **ESP-IDF の PSRAM XIP の実装（`CONFIG_SPIRAM_XIP_FROM_PSRAM`）** [mmu_psram_flash_v2.c:54-108,139-169]
  1. フラッシュの末尾ページ（0x43FF_0000）に、フラッシュの該当物理ページを 1 枚ずつマップする。
  2. 同じ仮想アドレスへ PSRAM の物理ページをマップする（BOOT_INIT の場合）。
  3. 両方の範囲を invalidate してから `memcpy` で 64KB をコピーする。
  4. 全部終わったら `cache_hal_writeback_addr` で書き戻す。
  - これでリンク時の仮想アドレス（.text の 0x4000_0020〜）はそのままに、中身だけ PSRAM に入れ替わる。ld でも `CONFIG_SPIRAM_FETCH_INSTRUCTIONS` が有効なら irom_seg は 0x4800_0020 から始まる [memory.ld.in:58-74]。
  - Kconfig のヘルプによると、P4 ではフラッシュと PSRAM が別々の MSPI バスにつながっている。そのため PSRAM から XIP していれば、フラッシュ操作（MSPI1 での書き込み）中でも IRAM 以外のコードが動き続ける [esp_psram/esp32p4/Kconfig.spiram:67-87]。
- **キャッシュは仮想アドレスでタグ付けされている前提で扱う**（推測。ESP-IDF がマップの変更後に「仮想アドレス範囲」を invalidate していることから）。同じ仮想アドレスに別の物理ページを付け替えるときは、次の順にする。
  1. 旧マッピングで dirty なデータを writeback する。
  2. エントリを書き換える。
  3. その仮想範囲を L1 I/D と L2 で invalidate する。
- MMU は 1 枚しかなく、全コアで共有される。**2 コアで「同じ仮想アドレスに別のアプリ」を同時に置くことはできない**。

### 3.3 フラッシュイメージの制約
ESP-IDF のアプリイメージは `paddr % 64KB == vaddr % 64KB` を満たすように 0x20 オフセットでリンクされている（0x18 のファイルヘッダと 0x08 のセグメントヘッダの分）[memory.ld.in:61-71]。自作のアプリ形式では **コードとデータの本体を 64KB 境界から置き、リンクアドレスも 64KB 境界にする** と XIP マップが単純になる。

---

## 4. 実行可能領域と保護（PMA / PMP）

- **PMA**: 16 エントリ。ESP-IDF は、予約の隙間を「無効（NONE）」で埋め、0x4000_0000–0x4FFF_FFFF を NAPOT 1 つで **RWX かつ cacheable** にしている [cpu_region_protect.c:41-86]。ブート時には ROM が PMA エントリ 15 で同じことをしている（同ファイルのコメント:79-80）。つまり **ハードウェア上は L2MEM / PSRAM 窓 / フラッシュ窓のどこからでも実行できる**。
- **PMP**: TRM には「最大 32 領域」とある [TRM §2.1 p.69]。ただし **v3.x で 32 に増えた** [v3 UG §1.1] ので、**v1.x（Tab5 無印）は 16 エントリ**（ESP-IDF の rev<3 用コードもエントリ 0–15 しか使っていない [cpu_region_protect.c:235-357]）。粒度は 128B [soc_caps.h:208]。v3 には「24 番と 28 番のエントリは TOR に使えない（DIG-752）」という問題がある [cpu_region_protect.c:183,217]。
- ESP-IDF の MEMPROT 有効時の PMP 構成は、L2MEM を「`_iram_text_end` までは RX、それ以降は RW」に、フラッシュ窓を「irom は RX、drom は R」に分けている [cpu_region_protect.c:136-187]。U モード分離を作るときもこの方針がそのまま使える。
- **U モードの観点**: RISC-V の標準動作では、どの PMP エントリにも一致しない U モードのアクセスは拒否される。したがって、アプリに「自分の text（RX）・data（RW）・スタック」を許可するエントリを数本足すだけでよい。ただし v1.x では **16 エントリをカーネル用と分け合う** ことになるので、アプリの窓は **2 のべき乗サイズで境界に揃え（NAPOT）**、エントリ数を節約する設計にすること。
- MMIO（0x5000_0000〜、0x3FF0_0000〜）とキャッシュコントローラ、0x8xxx の直結エイリアスは、U モードには許可しない。

---

## 5. Tab5 固有の事情

- フラッシュ 16MB、PSRAM 32MB [espp m5stack-tab5 README]。フラッシュは 64MB 窓の 1/4、PSRAM は 64MB 窓の 1/2 で、どちらも全域を同時にマップできる。
- ESP32-P4 の PSRAM は 16 ライン（HEX）で 1.8V、最大 250MHz DDR [DS §4.1.3.1 p.46]。ESP-IDF の既定は HEX モード 200MHz で、**250MHz は v3.x のみ** 選べる [Kconfig.spiram:11-45]。Tab5 無印（v1.x）では 200MHz が上限（ESP-IDF の Kconfig に基づく）。理論帯域は 16 × 2 × 200MHz = 6.4 Gbit/s。データシートの式で計算した値 [DS p.46]。
- 外部フラッシュは SPI/Dual/Quad/QPI で最大 120MHz [DS §4.1.3.1 p.46]。帯域は PSRAM の数分の 1 程度（推測。4bit × 120MHz SDR として約 0.48 Gbit/s）。
- チップリビジョン: Tab5 無印は v1.x（M5GFX PR#343 のテストに「Tab5 (v1.0)」とある。v1.3 の個体報告もある [RetroESP32-P4 issue#1]）。v3.x を積んだものは Tab5X と呼ばれている。ESP-IDF 6.0 の既定は v3.x 向けなので、**v1.x 向けには `CONFIG_ESP32P4_SELECTS_REV_LESS_V3=y` が必要**。設定を間違えるとブートしない [Kconfig.hw_support:4-11; xiaozhi Tab5 README]。リビジョンは eFuse の `EFUSE_RD_MAC_SPI_SYS_2_REG` のビット [5:4] とビット [3:0] で判別できる [Errata Table 1.1]。実機の値は未確認。
- 関係しそうな errata
  - APM-560: 権限のない AHB アクセスが、その後の PSRAM・フラッシュアクセスを止めることがある。v0.0〜v3.0 が対象 [Errata Table 2.1]。**U モードのアプリが不正アクセスしたときにシステム全体が止まる可能性がある** ので要確認。
  - MSPI-749: v3.0 のみ。

---

## 6. アプリ配置の選択肢比較

前提: アプリは U モードで動き、カーネルが実行時にロードする。ロード元はフラッシュのパーティションまたは SD。

| | A. L2MEM にロード | B. PSRAM にロードし MMU で固定仮想アドレスへ | C. フラッシュを MMU で XIP マップ |
|---|---|---|---|
| 容量 | v1.x で最大約 560KB（L2 キャッシュ 128KB、ROM 予約を除く）。カーネルと奪い合いになる | 32MB（カーネル用ヒープを除いたもの） | フラッシュのパーティション容量。最大 16MB |
| 速度 | 最速（CPU の 1/2 クロック、L1 経由） | L1/L2 キャッシュに当たれば速い。外れると PSRAM 200MHz × 16bit DDR | キャッシュが外れたときが最も遅い（Quad SPI、最大 120MHz） |
| 再配置 | アドレスが物理で固定なので、PIE で再配置するか、アプリごとに配置を決め打ちする | **不要**。どの物理ページでも固定仮想アドレス（例 0x4A00_0000）にマップできる | **不要**。64KB 境界に置けば固定仮想アドレスにマップできる |
| SD からのロード | 可 | 可 | 不可（一度フラッシュに書き込む必要がある） |
| ロードのコスト | コピー＋D writeback＋I invalidate | コピー＋MMU 設定＋L1/L2 の writeback と invalidate | MMU 設定＋invalidate だけ（コピーなし） |
| フラッシュ書き込み中 | 動き続ける | 動き続ける（MSPI のバスが別） | **止まる**。フラッシュの消去・書き込み中はフラッシュキャッシュを止める必要がある [spi_flash cache_utils.c:295-307] |
| 断片化 | 物理的に連続した領域が必要 | 64KB 単位なので物理的に連続していなくてよい（ページング的に使える） | パーティション内で連続している必要がある |
| 書き込み可能なデータ | 同じ領域に置ける | .data/.bss も PSRAM のページにマップできる | .data/.bss は別途 RAM が必要 |
| 保護（PMP） | NAPOT の境界合わせが必要 | 仮想窓を 2 のべき乗に揃えれば PMP エントリ 1〜3 本で足りる | 同じ |
| マルチタスク | 共存は容易（アドレスが違うため） | 同じ仮想アドレスを使うアプリは同時に 1 つだけ。切り替えのたびに MMU の付け替えと invalidate が要る。共存させるならアプリごとに仮想スロットを分ける | B と同じ |

### 推奨

> **統合時注記**: 文書 04 は「固定仮想アドレス＋MMU の張り替え」ではなく静的 PIE＋ロード時再配置を推奨しており、07 では両者を折衷して「PSRAM は起動時に一度だけ線形マップし、アプリは PIE でその中の任意位置へ再配置する」案を採った（07 §3）。以下はこの章単独での推奨として残す。

1. **主方式は B（PSRAM＋MMU）**
   - アプリのリンクアドレスを固定する。案: text/rodata は 0x4A00_0000〜、data/bss/heap/stack はその後ろに置き、合計 4MB や 8MB などの 2 のべき乗の窓にする。カーネルは 64KB 単位で PSRAM の物理ページを割り当て、イメージをコピーしてマップし、writeback と invalidate を行い、PMP でその窓だけを U モードに開ける。
   - 理由は 4 つ。
     - SD とフラッシュを同じ経路で扱える。
     - 再配置が要らない。
     - フラッシュの OTA 更新中もアプリが止まらない。
     - 32MB あるので容量に余裕がある。
   - 複数のアプリを同時に常駐させるなら、「スロットごとに仮想ベースを変えたリンク（例: スロット n = 0x4A00_0000 + n × 4MB）」と「単一スロット＋切り替え時に再マップ」のどちらにするかを決める必要がある。タブレット UI の「前面アプリは 1 つ」というモデルなら後者が簡単（推測）。
2. **カーネルは L2MEM（と必要ならフラッシュ XIP）**
   - トラップハンドラ、MMU やフラッシュを操作するコード、キャッシュ保守のコードは L2MEM に常駐させる。フラッシュ書き込み中や MMU の書き換え中にキャッシュを止めても、その間に実行し続けられるようにするため。ESP-IDF が IRAM_ATTR を要求しているのと同じ理由 [esp_mmu_map.c:377-395]。
   - カーネル本体の大きな部分は、フラッシュ XIP か、ESP-IDF と同様に起動時に PSRAM へコピーする方式（B と同じ仕組み）にする。
3. **C（フラッシュ XIP）は補助**。容量が大きく、読むだけで、更新頻度が低いアセットや「システムアプリ」に使う。ただし、そのアプリが動いている間はフラッシュに書き込めない。
4. **A（L2MEM）はアプリには使わない**。カーネル、DMA バッファ、高速なスタックのために温存する。

---

## 7. 未確認事項

- **fence.i が L1 I キャッシュを invalidate するか**。TRM にも ESP-IDF にも記述がない。実機で「PSRAM／L2MEM にコードを書く → fence.i だけ → 実行」が失敗するか試す必要がある。
- **L1 I キャッシュがコアごとに 16KB か、合計で 16KB か**。D キャッシュが本当に両コアで共有されているか（コア間で D の一貫性を取る操作が要るか）。
- **キャッシュのタグが仮想アドレスか物理アドレスか**。ESP-IDF の挙動からは仮想だと推測できるが、TRM に明記がない。同じ物理 PSRAM ページを 2 つの仮想アドレスにマップ（エイリアス）したときの一貫性も未確認。
- Tab5 実機のチップリビジョン（v1.0 か v1.3 か）。eFuse で確認が必要（本調査では実機に触れていない）。
- v1.x での L2 キャッシュ 0KB（Config 3）の可否と、ROM が起動時に設定している既定の L2 キャッシュサイズ。
- Flash MMU の物理ページ番号の幅。値マスクの 11bit と `MAX_PADDR_PAGE_NUM` = 32768 が食い違っている。16MB しか使わないので実害はない。
- ROM の cache 関数のアドレスが v1.x と v3.x で同じか（rom.ld と rom.eco5.ld の差分）。
- APM-560（不正な AHB アクセスで PSRAM やフラッシュが止まる）が、U モードの不正アクセス（PMP 違反は CPU 内で止まるので AHB には出ない、と推測）でも起きるか。
- TRM v0.7 は「最新のチップリビジョン」向けに更新されている（Revision History、2026-08-20）。L2MEM の図 9.3-2 などが v1.x の配置と一致するかは未確認。v1.x の配置は ESP-IDF のリンカスクリプトから推定した。
- esp-hal 1.1.2 は ESP32-P4 に未対応（`ld/` に esp32p4 がない）。Rust 側のメモリ定義は自前で用意する必要がある。

---

## 参考文献

- [TRM] ESP32-P4 Technical Reference Manual, Pre-release v0.7（2026-08-20）。§2.1 Features p.69、§2.10.2 PMA Checker p.147-149、§9 System and Memory p.901-915（Table 9.3-1 p.904-905、§9.3.2 p.905-909、§9.3.3 p.909-911、§9.3.4 p.911-912、Table 9.3-2 p.912）。https://www.espressif.com/sites/default/files/documentation/esp32-p4_technical_reference_manual_en.pdf
- [DS] ESP32-P4 Series Datasheet v0.7。Table 1-1 p.11、§1.3 Chip Revision p.11-12、§4.1.3.1 p.45-46、§4.1.3.3 p.47。https://www.espressif.com/sites/default/files/documentation/esp32-p4_datasheet_en.pdf
- [Errata] ESP32-P4 Series SoC Errata v1.4。Table 1.1/1.2、Table 2.1、§3.3。https://docs.espressif.com/projects/esp-chip-errata/en/latest/esp32p4/
- [v3 UG] ESP32-P4 Chip Revision v3.x User Guide。§1.1、§1.5、§2.1.2。https://documentation.espressif.com/esp32-p4-chip-revision-v3.x_user_guide_en.html
- ESP-IDF v6.0（`~/esp-idf/6.0`、v6.0-beta1-624-g6534932340）。以下のパスは `components/` からの相対。
  - soc/esp32p4/include/soc/soc.h:147-223
  - soc/esp32p4/include/soc/ext_mem_defs.h:14-126
  - soc/esp32p4/include/soc/soc_caps.h:180-183, 206-208, 413-417, 567
  - soc/esp32p4/register/hw_ver{1,3}/soc/reg_base.h:8,30,52-54、spi_mem_c_reg.h（MMU_ITEM_INDEX/CONTENT）
  - hal/esp32p4/include/hal/mmu_ll.h:24-300
  - hal/esp32p4/include/hal/cache_ll.h:27-54, 483-507, 644-665, 1075-1093
  - hal/cache_hal.c:266-291
  - esp_system/ld/esp32p4/memory.ld.in:18-124
  - esp_system/port/soc/esp32p4/Kconfig.cache:3-45
  - esp_hw_support/port/esp32p4/cpu_region_protect.c:41-400
  - esp_hw_support/port/esp32p4/Kconfig.hw_support:4-70
  - esp_mm/esp_mmu_map.c:377-456、esp_mm/port/esp32p4/ext_mem_layout.c:17-34、esp_mm/include/esp_mmu_map.h、esp_cache.h
  - esp_psram/xip_impl/mmu_psram_flash_v2.c:54-169、esp_psram/esp32p4/Kconfig.spiram:1-105
  - bootloader/subproject/main/ld/esp32p4/bootloader.ld.in:13-52、bootloader.rev3.ld.in
  - bootloader_support/src/esp_image_format.c:257-261, 720-727, 792-797
  - spi_flash/cache_utils.c:295-307
  - esp_rom/esp32p4/ld/esp32p4.rom.ld:186、esp_rom/esp32p4/include/esp32p4/rom/cache.h:467-646
- ESP-IDF master（`~/esp-idf/master`、d0b65797c30）: esp_system/ld/esp32p4/memory.ld.in（SRAM_END などの差分）
- esp-hal 1.1.2（`~/.cargo/registry/src/index.crates.io-*/esp-hal-1.1.2/ld/`）: esp32p4 なし
- M5GFX PR#343（Tab5 = v1.0、Tab5X = v3.0 以降）: https://github.com/m5stack/M5GFX/pull/343
- RetroESP32-P4 issue#1（Tab5 の v1.3 個体）: https://github.com/giltal/RetroESP32-P4/issues/1
- xiaozhi-esp32 の m5stack-tab5 README（Rev 1.x と IDF の既定設定の不一致）: https://github.com/78/xiaozhi-esp32/blob/main/main/boards/m5stack-tab5/README.md
- espp m5stack-tab5 README（16MB フラッシュ / 32MB PSRAM）: https://components.espressif.com/components/espp/m5stack-tab5/versions/1.3.0/readme
