# 09. M1 実機立ち上げの記録（Tab5、ESP32-P4 rev v1.0）

- 日付: 2026-10-30
- 対象: M5Stack Tab5（MAC 30:ed:a0:e0:cb:86）。`espflash board-info` の結果は esp32p4 (revision v1.0)、フラッシュ 16MB、Secure Boot とフラッシュ暗号化は無効。
- 結果: **M1a と M1b を達成**。U モードのアプリが ecall で USB Serial/JTAG に出力し、保護テスト 7 件がすべて期待どおりにフォルトした。ログ全文は [logs/m1b-tab5-20261030.txt](logs/m1b-tab5-20261030.txt)。

## 手順

1. `make backup-flash`: 書き込み前のフラッシュ 16MB を全量読み出した（SHA-256 `8948aae7…5f96`）。リポジトリの外の `~/tab5-backups/` にも保存した。0x2000 のブートローダのマジック（0xE9）と旧パーティションテーブルが読めることを確認した。
2. `make flash-p4`: otadata を消去し、ブートローダ（0x2000）、パーティションテーブル（0x8000）、カーネル（0x10000）を書き込んだ。
3. 以後の修正では、カーネル（0x10000）だけを書き直した。

元のファーム（portable_terminal）に戻すには、バックアップを 0 番地から書き戻す: `espflash write-bin --port <PORT> 0 ~/tab5-backups/tab5-30eda0e0cb86-20261030-portable_terminal.bin`

## 実機で確認できた事実

| 項目 | 結果 | 関連する未確認事項 |
|---|---|---|
| ブートローダ直後の USB Serial/JTAG | 初期化なしで、FIFO に直接書くだけで出力できる | 06 §7、07 §8-4 |
| 作り直したブートローダ（`REGION_PROTECTION_ENABLE=n`）での PMP | pmpcfg0〜3 がすべて 0。ロックされたエントリはない | 07 §8-1 |
| misa | `0x40901125`。RV32、I/M/A/F/C/U と X（bit23）。S はない | 01 §0 |
| mtvec の CLIC モード | `base | 3` が読み戻せる（例 `0x40001e83`） | 01 §4.1 |
| U モードへの遷移 | mret で U に入れる（MPP=0、mstatus の書き込みだけで十分） | 07 §8-2 |
| U モードの ecall | mcause=8。CLIC モードでも上位ビットは 0 だった | 01 §5.1 |
| PMP（TOR 3 本、粒度 128B） | U から周辺 MMIO、カーネルの rodata（フラッシュ）、カーネル RAM（L2MEM）の読み書きはすべて拒否された。データ領域の実行と、自分のテキスト領域への書き込みも拒否された | 07 §3 |
| 例外の精度 | 今回のケースでは load/store/fetch のアクセスフォールトの mepc と mtval が正確だった（不正確になるケースは未確認） | 04 §4.2 |
| RWDT | カーネル起動時の停止処理で、20 秒間リセットされなかった | 06 §2 |

## つまずいた点と対処

### 1. ブートローダの `assert(rom_index == 2)`
- 現象: `Assert failed in unpack_load_app, bootloader_utility.c:842 (rom_index == 2)` で起動が止まった。
- 原因: P4（I/D の仮想アドレスを共有するチップ）のブートローダは、フラッシュにマップするセグメントがちょうど 2 つあることを前提にしている（`~/esp-idf/6.0/components/bootloader_support/src/bootloader_utility.c:803-843`）。最初のカーネルは、フラッシュ側に app descriptor の 1 セグメントしか持っていなかった。
- 対処: esp-hal の P4 用リンカスクリプトと同じく、「app descriptor + rodata」「隙間（NOLOAD）」「text」をフラッシュ窓に並べ、2 セグメントにした（`kernel/kernel-common.ld`）。カーネルのコードはフラッシュから XIP で実行する。

### 2. U モードの最初の命令で「0x7c への store access fault」
- 現象: すべてのアプリが、最初の命令の時点で mcause=7、mtval=0x7c で止まった。レジスタは初期値のままで、mepc は実行ごとに変わった。PMP で U に全域 RWX を与えても同じだった。
- 原因: カーネルが書き込んだアプリのコードが L1 D キャッシュに残ったままで、命令フェッチには L2MEM の古い内容が見えていた。古いバイト列がたまたま「0x7c へのストア」として実行されたと考えられる。`fence.i` だけでは足りない（02 §2.2 の懸念が当たった）。
- 対処: ロード後に ROM の `Cache_WriteBack_Addr(L1_DCACHE)` と `Cache_Invalidate_Addr(L1_ICACHE0|1)` を呼ぶ（`Platform::sync_code`、ROM のアドレスは `esp_rom/esp32p4/ld/esp32p4.rom.ld:186,190`）。PSRAM に載せるとき（M4）は、L2 キャッシュも対象にする必要がある。
- 教訓: 実機でなければ見つからない種類の不具合。案 B のエミュレータでは、L1 D/I キャッシュの非一貫性を（少なくとも検出用のモードとして）模すと、回帰テストに使える（判断）。

### 3. QEMU virt の起動アドレス
- `-bios none` の QEMU virt は、ELF のエントリではなく RAM の先頭（0x8000_0000）に飛ぶ。rodata を先頭に置いたら起動しなくなった。virt 用のリンカスクリプトで、text を 0x8000_0000 の専用領域に置いて対処した（`kernel/link-virt.ld`）。

## 未確認のまま残っていること

- PMA の状態。ブートローダが PMA を設定していないか、空きエントリがいくつあるか。まだダンプしていない。
- U モード実行中の割り込み（M2 でタイマ割り込みを入れるときに確認する）。
- Core1 の状態（起動していない前提）。
- ROM の cache 関数を、カーネルのスタックの状態にかかわらず安全に呼べるか（現状は M モードのカーネルスタックで呼んでいる）。
