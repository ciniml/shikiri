/* ESP32-P4 rev v1.x (M5Stack Tab5), booted by the ESP-IDF 2nd stage
 * bootloader.
 *
 * - FLASH: the cache-mapped flash window. Code and read-only data run from
 *   here (XIP), like an ESP-IDF app; +0x20 skips the image header. The
 *   bootloader maps exactly two segments: appdesc + rodata, then text.
 * - RAM: lower L2MEM, below the region the bootloader itself runs from
 *   (~0x4FF2_9ED0 on this ROM/bootloader, doc/07 §8) and the ROM reserved
 *   area (doc/02 §1.1). Code the bootloader loads, data and the stack.
 * - RAM_HI: the lower part of upper L2MEM, for .bss (doc/30 §2). The
 *   bootloader loads only .hot_text and .hot_rodata there, and with PSRAM
 *   up the apps no longer use it; no process gets a PMP entry for it, so
 *   U-mode cannot touch it.
 *   The rest up to the L2 cache, 0x4FF8_0000-0x4FFA_0000, is the app pool
 *   when there is no PSRAM (platform/esp32p4v1.rs).
 *   .hot_text and .hot_rodata, the code the kernel runs all the time and
 *   its read-only data (doc/58 R5), come first.
 * - BOOT_LO: where the bootloader runs, up to the ROM's stack for starting
 *   core 1 (doc/02 §1.1). Free once the kernel runs, so only for what
 *   nothing loads: core 1's stack, used from when core 1 starts.
 * - LP_SRAM: uncached, kept across resets, readable by a debugger without
 *   halting the core; holds the log ring and the RTT control block (doc/12).
 *   The top from 0x5010_FA80 belongs to the LP ROM. */
MEMORY
{
    FLASH   : ORIGIN = 0x40000020, LENGTH = 0x200000 - 0x20
    RAM     : ORIGIN = 0x4FF00000, LENGTH = 0x29000
    RAM_HI  : ORIGIN = 0x4FF40000, LENGTH = 0x40000
    BOOT_LO : ORIGIN = 0x4FF29000, LENGTH = 0x4FF3AFC0 - 0x4FF29000
    LP_SRAM : ORIGIN = 0x50108000, LENGTH = 0x7A80
}

REGION_ALIAS("REGION_TEXT", FLASH);
REGION_ALIAS("REGION_RODATA", FLASH);
REGION_ALIAS("REGION_LOG", LP_SRAM);
REGION_ALIAS("REGION_BSS", RAM_HI);
REGION_ALIAS("REGION_HOT", RAM_HI);
REGION_ALIAS("REGION_STACK1", BOOT_LO);
/* The scheduler (8 process slots) lives on it (doc/30 §4). */
KERNEL_STACK = 128K;
KERNEL_STACK1 = 64K;

INCLUDE kernel-common.ld
