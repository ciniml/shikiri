/* ESP32-P4 rev v1.x with a 128 KiB L2 cache: the kernel lives in the lower
 * L2MEM region below the bootloader/ROM reserved area (doc/02 §1.1). The app
 * pool is the upper region 0x4FF4_0000-0x4FFA_0000 (platform/esp32p4v1.rs).
 *
 * FLASH is the cache-mapped flash window. Only the ESP-IDF app descriptor is
 * placed there for now: the 2nd stage bootloader and espflash expect it at the
 * start of the first flash-mapped segment. +0x20 skips the image header. */
MEMORY
{
    FLASH : ORIGIN = 0x40000020, LENGTH = 0x200000 - 0x20
    RAM   : ORIGIN = 0x4FF00000, LENGTH = 0x2CBD0
}

SECTIONS
{
    .flash.appdesc : ALIGN(4) {
        KEEP(*(.flash.appdesc))
    } > FLASH
}

INCLUDE kernel-common.ld
