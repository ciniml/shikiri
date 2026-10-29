/* ESP32-P4 rev v1.x (M5Stack Tab5), booted by the ESP-IDF 2nd stage
 * bootloader.
 *
 * - FLASH: the cache-mapped flash window. Code and read-only data run from
 *   here (XIP), like an ESP-IDF app; +0x20 skips the image header. The
 *   bootloader maps exactly two segments: appdesc + rodata, then text.
 * - RAM: lower L2MEM, below the region the bootloader itself runs from
 *   (~0x4FF2_9ED0 on this ROM/bootloader, doc/07 §8) and the ROM reserved
 *   area (doc/02 §1.1). The app pool is the upper region
 *   0x4FF4_0000-0x4FFA_0000 (platform/esp32p4v1.rs).
 * - LP_SRAM: uncached, kept across resets, readable by a debugger without
 *   halting the core; holds the log ring and the RTT control block (doc/12).
 *   The top from 0x5010_FA80 belongs to the LP ROM. */
MEMORY
{
    FLASH   : ORIGIN = 0x40000020, LENGTH = 0x200000 - 0x20
    RAM     : ORIGIN = 0x4FF00000, LENGTH = 0x29000
    LP_SRAM : ORIGIN = 0x50108000, LENGTH = 0x7A80
}

REGION_ALIAS("REGION_TEXT", FLASH);
REGION_ALIAS("REGION_RODATA", FLASH);
REGION_ALIAS("REGION_LOG", LP_SRAM);

INCLUDE kernel-common.ld
