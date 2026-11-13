/*
 * Copyright (c) 2026 Kenta IDA
 * Distributed under the Boost Software License, Version 1.0.
 * (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
 * SPDX-License-Identifier: BSL-1.0
 */

/* ESP32-P4 chip v3.x (ECO5, the EV-Board v1.6.x; doc/58 R1b), booted by the
 * ESP-IDF 2nd stage bootloader built for v3 (boot/sdkconfig.v3). The same
 * sections as link-esp32p4v1.ld; only L2MEM is laid out differently:
 *
 * - 0x4FF0_0000-0x4FF2_0000: the 128 KiB L2 cache (v3 puts it at the
 *   bottom; v1.x at the top).
 * - RAM, RAM_HI: from the cache up to the app pool. RAM_HI ends where it
 *   does on v1.x, so the app pool and the Wi-Fi and Ethernet rings
 *   (0x4FF8_0000-0x4FFA_0000, platform/esp32p4v1.rs, emac.rs) stay put.
 * - BOOT_HI: from the pool to the ROM's stack for starting core 1. The
 *   bootloader runs at its top (0x4FFA_C2C0-0x4FFB_AFC0, bootloader.rev3.ld.in);
 *   free once the kernel runs, so only for core 1's stack.
 * - 0x4FFB_AFC0-0x4FFC_0000: the ROM's core 1 stack and data
 *   (rom_spiflash_legacy_data = 0x4FFB_FFE8, esp32p4.rom.eco5.ld).
 * - LP_SRAM: as on v1.x. */
MEMORY
{
    FLASH   : ORIGIN = 0x40000020, LENGTH = 0x200000 - 0x20
    RAM     : ORIGIN = 0x4FF20000, LENGTH = 0x25000
    RAM_HI  : ORIGIN = 0x4FF45000, LENGTH = 0x4FF80000 - 0x4FF45000
    BOOT_HI : ORIGIN = 0x4FFA0000, LENGTH = 0x4FFBAFC0 - 0x4FFA0000
    LP_SRAM : ORIGIN = 0x50108000, LENGTH = 0x7A80
}

REGION_ALIAS("REGION_TEXT", FLASH);
REGION_ALIAS("REGION_RODATA", FLASH);
REGION_ALIAS("REGION_LOG", LP_SRAM);
REGION_ALIAS("REGION_BSS", RAM_HI);
REGION_ALIAS("REGION_HOT", RAM_HI);
REGION_ALIAS("REGION_STACK1", BOOT_HI);
KERNEL_STACK = 128K;
KERNEL_STACK1 = 64K;

INCLUDE kernel-common.ld
