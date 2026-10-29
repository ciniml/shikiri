# M3 調査: 実行中のフラッシュの読み出し・消去・書き込み（ESP32-P4 v1.0）

対象は ESP32-P4 rev v1.0（Tab5）。カーネルは M モードのベアメタルで、コードはフラッシュから XIP で動いている。読むのは `apps`（0x42_0000、8MB）にある T5A ファイルで、のちに `appmeta`（0x41_0000）と `apps` へ再起動なしで書き込む。
出典の表記: パスは `~/esp-idf/6.0/components/` からの相対パス（`docs/...` だけは `~/esp-idf/6.0/` から）。（推測）は推測、（判断）は判断を表す。MMU の基本仕様（64KB ページ、エントリ 1024 個、レジスタ）は doc/02 §3 にあるので、ここでは繰り返さない。

## 1. フラッシュを仮想アドレスにマップする（読み出しの本命）

### 1.1 ESP-IDF の呼び出し経路
`spi_flash_mmap()` → `esp_mmu_map()` → `s_do_mapping()` → `mmu_hal_map_region()` → `mmu_ll_write_entry()` の順に呼ばれる [spi_flash/flash_mmap.c:88; esp_mm/esp_mmu_map.c:436-456; hal/mmu_hal.c:92-114; hal/esp32p4/include/hal/mmu_ll.h:265-300]。

`s_do_mapping` の中身は次のとおり [esp_mmu_map.c:377-456]。
1. `s_stop_cache()`: P4 では freeze を使わず、`spi_flash_disable_interrupts_caches_and_other_cpu()` を呼ぶ（コメントは「他コアのストールに制限があるため」）[esp_mmu_map.c:377-385]。中身は、他コアを IPC でスピンさせ、IRAM にない割り込みを止め、`spi_flash_disable_cache()` を呼ぶこと [spi_flash/cache_utils.c:125-193]。
2. `spi_flash_disable_cache()` は、分岐予測を切ってから（分岐予測もキャッシュ要求を出すため）`esp_cache_suspend_ext_mem_cache()` を呼ぶ [cache_utils.c:340-347]。止めるのは **L2 キャッシュだけ**（`cache_hal_suspend(CACHE_LL_LEVEL_EXT_MEM=2, ALL)` → `Cache_Suspend_L2_Cache()`）[esp_mm/esp_cache_utils.c:27-37; hal/esp32p4/include/hal/cache_ll.h:351-356,366-387]。L1 は止めない。L2MEM へのアクセスは L1 を通るので、L1 を止めると RAM 上のコードまで動かなくなるため（推測）。
3. エントリを書き込む。
4. `cache_ll_l1_enable_bus()` を呼ぶが、P4 では中身が空 [cache_ll.h:1020-1024]。
5. `cache_hal_invalidate_addr(vaddr, size)` を呼ぶ。フラッシュ窓のアドレスは level 2 と判定され [cache_ll.h:1075-1093]、`cache_ll_invalidate_addr(2, ALL, ...)` が **L1 I（両コア）、L1 D、L2 の 3 つ** を `Cache_Invalidate_Addr` で invalidate する [cache_ll.h:483-521,536-557]。invalidate は L2 を再開する **前** に行う。
6. `Cache_Resume_L2_Cache(autoload)` で L2 を再開し、割り込みと他コアを戻す。

### 1.2 エントリの書式（Flash MMU、P4）
- レジスタ: `SPI_MEM_C_MMU_ITEM_INDEX_REG` = **0x5008_C380**、`SPI_MEM_C_MMU_ITEM_CONTENT_REG` = **0x5008_C37C**。index を書いてから content を書く [mmu_ll.h:286-292; soc/esp32p4/register/hw_ver1/soc/spi_mem_c_reg.h:2630,2642; reg_base.h:9,52]。読み出しも同じ手順で、index を書いて content を読む [mmu_ll.h:311-330]。2 つのレジスタは別々に書くので操作はアトミックではない。**MMU を書き換える処理を同時に 2 つ走らせてはいけない**（判断）。
- 値: `content = (paddr >> 16) | BIT(12)`。BIT(12) が VALID、BIT(10) が ACCESS_PSRAM（フラッシュでは 0）、BIT(13) が SENSITIVE（フラッシュ暗号化を使うときだけ立てる）[ext_mem_defs.h:57-66; mmu_ll.h:233-280]。無効化するときは 0 を書く [mmu_ll.h:339-359]。物理ページ番号は bit[10:0] [ext_mem_defs.h:77]。
- エントリ番号: `entry = (vaddr & 0x03FF_FFFF) >> 16`。フラッシュ窓は 0x4000_0000–0x43FF_FFFF で、エントリは 1024 個（0〜1023）[ext_mem_defs.h:34-38,87-96; mmu_ll.h:199-221]。
- 例: `apps` の先頭（paddr 0x42_0000）を vaddr 0x4142_0000 にマップするには、エントリ番号 `0x142`（=322）に `0x1042` を書く。

### 1.3 bootloader が使っているエントリ
- bootloader は MMU をすべて無効にしてから、rodata と text のセグメントを `mmu_hal_map_region` でマップする [bootloader_support/src/bootloader_utility.c:1035-1132]。さらに **最後のエントリ 1023（vaddr 0x43FF_0000）** にも rodata の先頭ページをマップする（アプリがブートパーティションを見つけるため）[bootloader_utility.c:1084-1085; mmu_ll.h:28-29]。
- 本カーネルのリンク先は FLASH 0x4000_0020 から、長さ 2MB（`kernel/link-esp32p4v1.ld`）。rodata と text は同じ領域に続けて置かれている。したがって使われているのは **エントリ 0 から最大 31 と、エントリ 1023**（推測。実機で MMU を読み出して確かめる）。
- **残りのエントリ（32〜1022）は自由に使える**（判断）。

### 1.4 ROM 関数で代用できるか
- `Cache_FLASH_MMU_Set(sensitive, vaddr, paddr, psize=64, num, fixed)` が ROM の 0x4FC0_0518 にあり、v1 用 ld（`esp32p4.rom.ld:263`）と eco5 用 ld で同じアドレス [esp_rom/esp32p4/include/esp32p4/rom/cache.h:296-336]。ただし P4 の ESP-IDF はこれを使わず、`mmu_hal` でレジスタを直接書いている（`cache_flash_mmu_set` を使うのは ESP32 だけ）。ROM 関数がキャッシュの invalidate までするとは書かれていない（推測）。**レジスタ 2 本を書くだけなので自前で書く方が確実**（判断）。
- キャッシュ操作の ROM 関数（v1 のアドレスは `esp_rom/esp32p4/ld/esp32p4.rom.ld`。下の関数は eco5 でも同じアドレス）
  - `Cache_Invalidate_Addr(map, addr, size)` = 0x4FC0_03E4（:186）
  - `Cache_WriteBack_Addr` = 0x4FC0_03F4（:190）
  - `Cache_Suspend_L2_Cache()` = 0x4FC0_0508 / `Cache_Resume_L2_Cache(autoload)` = 0x4FC0_050C（:259-260）
  - `map` の値: L1_ICACHE_0 = BIT0、L1_ICACHE_1 = BIT1、L1_DCACHE = BIT4、L2_CACHE = BIT5 [rom/cache.h:228-233]
- ROM 関数の **データ** はリビジョンでアドレスが違う。`rom_cache_internal_table_ptr` は v1 が 0x4FF3_FFD8、v3 が 0x4FFB_FFD8。`rom_spiflash_legacy_data` は v1 が 0x4FF3_FFE8、v3 が 0x4FFB_FFE8 [esp32p4.rom.ld:169,282; esp32p4.rom.eco5.ld:169,301]。v1 では **0x4FF3_FBA4–0x4FF4_0000（ROM の .bss）を壊さない** こと（doc/02 §1.1）。ld の使い分けは esp_rom/CMakeLists.txt:100-107 にある。
- ROM（0x4FC0_xxxx）のコードは L2 を止めている間も実行できる（推測）。根拠は、ESP-IDF が L2 を suspend/disable した状態で `Cache_Invalidate_Addr`、`Cache_Resume_L2_Cache`、`esp_rom_spiflash_read` を呼んでいること [esp_mmu_map.c:436-456; bootloader_flash.c:449-475]。doc/02 の表で HP ROM を「L2 経由」としている点は、見直しが要る（推測）。

### 1.5 最小の手順（マップの追加と変更）
```
// 前提: この関数（と呼ばれる関数すべて）は L2MEM に置く。スタックも L2MEM。
mstatus.MIE = 0                        // 割り込み禁止
csrc mhcr, (1<<4)|(1<<5)|(1<<12)       // 分岐予測を切る (MHCR=0x7c1) [riscv/include/riscv/rv_utils.h:49-52,498-505]
a = Cache_Suspend_L2_Cache()           // 0x4FC0_0508。戻り値は autoload の状態
for i in 0..n:
    write32(0x5008_C380, entry0 + i)
    write32(0x5008_C37C, ((paddr >> 16) + i) | 0x1000)
Cache_Invalidate_Addr(0x03, vaddr, n*64K)   // L1 I (core0|1)
Cache_Invalidate_Addr(0x10, vaddr, n*64K)   // L1 D
Cache_Invalidate_Addr(0x20, vaddr, n*64K)   // L2
Cache_Resume_L2_Cache(a)               // 0x4FC0_050C
csrs mhcr, ...; mstatus.MIE を元に戻す
```
- 起動直後に 1 回だけ、使っていないエントリにマップを足す場合、厳密には L2 を止めなくてよいかもしれない（推測）。それでも ESP-IDF と同じ手順にそろえる（判断）。
- 同じ vaddr を付け替えるときは、先に旧マッピングの dirty 行を writeback する（doc/02 §3.2）。フラッシュ窓は読み出し専用で使うので、dirty 行はできない前提でよい（判断）。

## 2. SPI1 で直接読む（`esp_rom_spiflash_read`）
- ROM 関数 `esp_rom_spiflash_read(src, dest, len)` は 0x4FC0_0158（`esp32p4.rom.ld:126`、eco5 と同じ）。src、dest、len はどれも 4 バイト境界が必要 [esp_rom/include/esp_rom_spiflash.h:238-252]。
- P4 でも使える。P4 の 2nd stage bootloader が `bootloader_flash_read_no_decrypt()` でこれを使っている。ただし **前後で L2 を disable** している [bootloader_support/bootloader_flash/src/bootloader_flash.c:449-475]。
- アプリ側の `esp_flash_read`（SPI1 + HAL）も同じ。auto-suspend を使わない構成では、`spi1_start()` がキャッシュを止める [spi_flash/spi_flash_os_func_app.c:26,105-141]。キャッシュが止まっている間、受け側のバッファは DRAM になければならない [spi_flash/esp_flash_api.c:880]。
- **制約**: SPI1 を使っている間に SPI0（キャッシュ）がフラッシュを読むと衝突する。そのため実行中のコードは L2MEM か ROM になければならず、割り込みも止める。マップ方式より不利なので、**読み出しは §1 の mmap 方式にする**（判断）。SPI1 での読み出しは、書き込み後のベリファイなど限られた用途に使う。
- ROM ドライバは `rom_spiflash_legacy_data->chip`（`device_id`, `chip_size`, `block_size`, `sector_size`, `page_size`, `status_mask`）を見て動く [esp_rom_spiflash.h:35-42,361-368]。この値は bootloader が `esp_rom_spiflash_config_param(..., size*1MB, 64K, 4K, 256, 0xffff)` で設定している [bootloader_flash_config_esp32p4.c:125]。カーネルが ROM の .bss を壊さなければ、設定はそのまま残る（推測）。

## 3. 実行中の消去と書き込み

### 3.1 ESP-IDF のやり方（auto-suspend なし、P4 の既定）
- フラッシュ操作のたびに `spi1_start()` で次のことを行う [cache_utils.c:125-193]。
  1. スケジューラを止める。
  2. 他コアを IPC で `spi_flash_op_block_func`（IRAM 上のスピン）に入れる。
  3. IRAM にない割り込みを止める。
  4. 分岐予測を切り、L2 を suspend する。
  操作が終わったら `spi1_end()` で元に戻す。
- 消去は **セクタ（4KB）かブロック（64KB）を 1 回ずつ** 行い、1 回ごとに `end()` → `start()` でキャッシュと割り込みをいったん戻す [esp_flash_api.c:648-699]。
- 最後に `flash_end_flush_cache()` → `spi_flash_check_and_flush_cache(addr,len)` を呼ぶ。書き換えた物理範囲がどこかにマップされていれば、**キャッシュを止めたまま**、その vaddr を 64KB ページ単位で `cache_hal_invalidate_addr` する [esp_flash_api.c:214-231; flash_mmap.c:299-322; spi_flash/memspi_host_driver.c:132-135]。
- PSRAM を使う構成では、L2 を止める前に L1 D の内部メモリ分を writeback する（L1 の自動 writeback で固まるのを避けるため）[esp_cache_utils.c:29-35]。PSRAM を使う M4 で必要になる。
- bootloader 側の `bootloader_flash_write/erase_sector` は、ROM の `esp_rom_spiflash_write/erase_sector` をそのまま呼ぶ [bootloader_flash.c:560-621]。L2MEM だけで動く bootloader だからできること。

### 3.2 ROM の消去・書き込み関数（v1 = eco5 で同じアドレス、`esp32p4.rom.ld:110-141`）
| 関数 | アドレス | 備考 |
|---|---|---|
| `esp_rom_spiflash_erase_sector(sector_num)` | 0x4FC0_014C | 20h、4KB。引数は **セクタ番号**（addr/4096） |
| `esp_rom_spiflash_erase_block(block_num)` | 0x4FC0_0150 | D8h、64KB |
| `esp_rom_spiflash_write(dest, src*, len)` | 0x4FC0_0154 | 4 バイト境界。事前の消去が必要 |
| `esp_rom_spiflash_read(src, dest*, len)` | 0x4FC0_0158 | §2 |
| `esp_rom_spiflash_unlock()` | 0x4FC0_015C | BP ビットの解除 |
| `esp_rom_spiflash_wait_idle(chip*)` | 0x4FC0_0118 | |
| `esp_rom_spiflash_read_status` / `write_status` | 0x4FC0_0188 / 0x4FC0_0190 | |
| `esp_rom_spiflash_erase_area` (= `SPIEraseArea`) | 0x4FC0_0160 | `esp32p4.rom.api.ld:62` |

- 戻り値は 0 が OK、1 が ERR、2 が TIMEOUT [esp_rom_spiflash.h:44-48]。
- これらの関数は **完了するまで内部で待つ**（推測。ESP32 用パッチ実装 `esp_rom/patches/esp_rom_spiflash.c:135-220` の構造から）。そのため、キャッシュを止めている時間は消去にかかる時間そのものになる。
- ページ（256B）をまたぐ書き込みは ROM 側で分割される（推測。同じパッチ実装 :181-200）。実機で確かめるまでは、**呼ぶ側で 256B 境界ごとに分けて渡す**（判断）。
- BP（ブロック保護）は、bootloader が起動時に `bootloader_flash_unlock()` で解除している [bootloader_flash_config_esp32p4.c:236-239]。最後の `bootloader_enable_wp()` は WEL を落とす（WRDI）だけで、ROM の write/erase は毎回 WREN を送る（推測）。
- **キャッシュを有効にしたまま呼ぶのは不可**。SPI1 で消去や書き込みをしている間に SPI0 がフラッシュを読むと、壊れたデータを読むか止まる。auto-suspend がなければハードウェアは調停しない [docs/en/api-reference/peripherals/spi_flash/spi_flash_concurrency.rst:20-25,85; auto_suspend.inc:22]。

### 3.3 auto-suspend（P4 では使えない）
- `soc_caps.h` には `SOC_SPI_MEM_SUPPORT_AUTO_SUSPEND/AUTO_RESUME/SW_SUSPEND` が立っている [soc/esp32p4/include/soc/soc_caps.h:570-576]。Kconfig `SPI_FLASH_AUTO_SUSPEND` も P4 で選べる（既定は n）[spi_flash/Kconfig:86-102]。
- しかし公式ドキュメントには「**S3/C2/C3/C6/H2 のみ対応。P4 は将来対応するかもしれない**」とある [docs/en/api-reference/peripherals/spi_flash/spi_flash_optional_feature.rst:32-37]。v1 では tRS の個別設定もできない（`!ESP32P4_SELECTS_REV_LESS_V3` [spi_flash/Kconfig:116; spi_flash/esp_flash_spi_init.c:141,532]）。
- 使えるフラッシュも限られる（XM25QxxC、GD25QxxE、FM25Q32。75h/7Ah と SUS ビットの位置に条件がある）[auto_suspend.inc:6-20]。Tab5 のフラッシュが該当するかは分かっていない。
- **M3 では使わない**（判断）。

### 3.4 カーネルがすること（最小の安全な手順）
1. **フラッシュ操作の関数を L2MEM に置く**。`#[link_section = ".ram_text"]` などで、`.data` と同じく RAM にロードされるセクションを作る。リンカスクリプトの変更が必要。
   - 呼び出し先がフラッシュ（0x40xx_xxxx）にあってはならない。`memcpy`/`memset`（compiler_builtins）、パニック経路、境界チェック、`match` のジャンプテーブル（.rodata）、定数プールが落とし穴になる。
   - ビルド後に objdump でその範囲を検査する。具体的には、`jal`/`auipc` の飛び先と、ロードするアドレスがフラッシュ窓に入っていないことを機械的に確認する（判断）。
2. **データも RAM に置く**。書き込む元のバッファ（T5A の受信バッファ）、スタック、ROM に渡すポインタは、どれも L2MEM 上になければならない。`const` の配列はフラッシュに置かれるので不可。
3. 割り込みを禁止する（`mstatus.MIE=0`）。トラップベクタはフラッシュ上の .text にあるので、例外が起きても止まる。この区間では例外を起こさないようにする。
4. 分岐予測を切り（MHCR）、`Cache_Suspend_L2_Cache()` を呼ぶ。
5. ROM の `erase_sector` を **1 回だけ**、あるいは `write` を 256B 単位で呼ぶ。
6. 書き換えた範囲がマップされていれば、`Cache_Invalidate_Addr(0x33 相当: L1 I×2 / L1 D / L2, vaddr, len)` を呼ぶ（§1.5 と同じく 3 回に分けて呼ぶ）。
7. `Cache_Resume_L2_Cache(a)` を呼び、分岐予測と割り込みを戻す。RWDT を使っているならここで feed する。
8. 4KB ごとに 1〜7 を繰り返す。消去 1 回ぶんの時間（数十 ms 程度、最悪数百 ms（推測、チップ依存））だけ割り込みが止まる。
- Core1 は今は使っていない（doc/10）。使うようになったら、IDF と同じく、相手コアを RAM 上のスピンに入れてから止める。

### 3.5 リスク
- **フラッシュ上のコードを 1 命令でも実行すると止まる**。L2 が suspend されているので、L1 にない行を読みに行ったところで固まる（推測）。デバッガからは PC が 0x40xx_xxxx で止まって見えるはず。
- **ota_0（0x1_0000–0x21_0000）を消すとカーネルが壊れる**。erase/write 関数の入口で、`appmeta`/`apps` 以外のアドレスを拒否する。
- 書き込み中に電源が落ちると中途半端な状態が残る。appmeta に世代番号と CRC を持たせ、書き込みが完了してから有効化する（A/B）設計にする（判断）。
- DMA（DSI、PPA など）がフラッシュ上のデータを読んでいる最中に消去すると、壊れたデータを読む [spi_flash_concurrency.rst:85]。DMA の転送元は RAM か PSRAM に置く。
- 割り込みを止めている時間が長くなると、systimer の tick が遅れる。RWDT の設定時間にも注意する。

## 4. P4 v1 に固有の注意（ESP-IDF のコードから）
- `CONFIG_P4_REV3_MSPI_*` は **v3 専用**（`default y if ESP32P4_REV_MIN_300`）。スリープからの電源投入後に MSPI がクラッシュする問題への対策で、v1 には関係しない [esp_hw_support/lowpower/port/esp32p4/Kconfig.p4_rev3_mspi_workaround:1-10]。
- SPI1 の HAL には ECO0（v0.0）向けの回避策がある（連続モードビットの補正 [esp_hal_mspi/spi_flash_hal_common.inc:145-155]、4 バイトアドレスのビット [esp_hal_mspi/esp32p4/include/hal/spimem_flash_ll.h:570-577]）。条件は `!ESP_CHIP_REV_ABOVE(rev, 1)`、つまり rev < 1 の v0.0 だけで、**v1.0（rev=100）は通常経路を通る** [soc/include/soc/chip_revision.h:31]。ROM 関数を使う場合はこの HAL を通らない。
- v1 の ROM .bss は 0x4FF3_xxxx にあり、v3 では 0x4FFB_xxxx に移る（§1.4）。ROM の関数アドレスは、ここで使うものについては両リビジョンで同じ。
- `ESP32P4_SELECTS_REV_LESS_V3` で変わるのは、ここで扱う範囲では次の 2 つだけ: tRS の個別設定ができないこと（§3.3）、PSRAM の 250MHz が選べないこと [esp_psram/esp32p4/Kconfig.spiram:33-35]。
- Espressif の Errata（https://docs.espressif.com/projects/esp-chip-errata/en/latest/esp32p4/ ）は今回は読んでいない。M3 の実装前に、MSPI とキャッシュの項目を確認する。

## 5. 推奨する手順（M3 での実装案）
1. **読み出し窓（M3 前半）**: 起動時に、フラッシュ全体（16MB = 256 ページ）を **vaddr 0x4100_0000 + paddr** に線形にマップする。エントリ 256〜511 を使い、値は `i | 0x1000`。こうすると、`apps` は 0x4142_0000、`appmeta` は 0x4141_0000 から読める（判断）。
   - 手順は §1.5 で、関数は L2MEM に置く。カーネル自身のエントリ（0〜31、1023）とは重ならない。
   - T5A のヘッダや本体は、ここから `read_volatile` かコピーで読む。XIP で実行したいアプリは、64KB 境界に置いて別の vaddr にマップする（doc/02 §3.3）。
   - U モードからこの窓が見えないように、PMP で塞ぐ。
2. **ROM 呼び出しの土台**: `platform/esp32p4v1.rs` に ROM 関数の定数を追加する（`Cache_Suspend_L2_Cache` 0x4FC0_0508、`Cache_Resume_L2_Cache` 0x4FC0_050C、`esp_rom_spiflash_erase_sector` 0x4FC0_014C、`_write` 0x4FC0_0154、`_read` 0x4FC0_0158）。既存の `Cache_Invalidate_Addr` と同じやり方で関数ポインタとして呼ぶ。
3. **`.ram_text` セクション**: リンカスクリプトに RAM 上のコードセクションを追加し、`flash_op_erase_sector(addr)` と `flash_op_write(addr, src, len≤256)` だけをそこに置く（中身は §3.4）。ビルド時に、そのセクションからフラッシュ窓への参照がないかを検査するスクリプトを `tools/` に用意する。
4. **書き込み API（M3 後半）**: `flash_write_range(off, buf)` を作る。内容は次のとおり。
   - 範囲チェック（appmeta/apps のみ）
   - 4KB ずつ消去（1 回ごとに割り込みを戻す）
   - 256B ずつ書き込み
   - 読み出し窓を invalidate する
   - 窓から読み戻してベリファイ
   - 最後に appmeta を更新
5. ログは LP SRAM の RTT に出す。フラッシュ操作の前後にも出せる。ただし、L2 を止めている間は .rodata の文字列や書式化コードを使えないので、止めている区間ではログを出さない。

## 6. 実機で確かめること
- [ ] MMU のエントリ 0〜40 と 1023 の内容を読み出し、カーネルが使っているエントリが §1.3 の推測どおりか確かめる。
- [ ] 0x4100_0000 + 0x42_0000 から読んだ値が、esptool の `read_flash 0x420000` の結果と一致するか。
- [ ] `rom_spiflash_legacy_data`（0x4FF3_FFE8 が指す先）の `device_id` と `chip_size`（16MB になっているか）。JEDEC ID からフラッシュの型番を調べ、データシートで tSE（セクタ消去時間）を確認する。
- [ ] RAM 上の関数から `Cache_Suspend_L2_Cache` → `Cache_Resume_L2_Cache` だけを呼び、戻ってこられるか（ROM が L2 を止めていても動くかの確認も兼ねる）。
- [ ] `apps` の末尾付近（使っていないセクタ）で、erase → write → invalidate → 窓からの読み戻しが一致するか。invalidate を省いたときに古い値が見えるかも確かめ、キャッシュが仮想アドレスで引かれていることの裏付けにする。
- [ ] 消去 1 回あたり割り込みが止まる時間を systimer で測り、RWDT の設定時間と比べて余裕があるか。
- [ ] 256B ページをまたぐ `esp_rom_spiflash_write` が正しく書けるか（ROM が分割するかどうか）。
- [ ] わざとフラッシュ上の関数を呼んだときに固まる様子（PC の値）を probe-rs で一度見ておき、デバッグのときに見分けられるようにする。
