/* Shared kernel layout. Platform scripts define the RAM region and the
 * REGION_TEXT / REGION_RODATA aliases before including this file. */
ENTRY(_start)

SECTIONS
{
    /* ESP-IDF app descriptor; must start the first flash segment (empty on
     * platforms that do not use the ESP-IDF bootloader). */
    .flash.appdesc : ALIGN(4) {
        KEEP(*(.flash.appdesc))
    } > REGION_RODATA

    .rodata : ALIGN(4) {
        *(.rodata .rodata.* .srodata .srodata.*)
    } > REGION_RODATA

    /* The ESP32-P4 bootloader requires exactly two flash-mapped segments
     * (bootloader_utility.c unpack_load_app), so keep rodata and text apart. */
    .text_gap (NOLOAD) : {
        . = . + 8;
        . = ALIGN(4) + 0x20;
    } > REGION_RODATA

    .text : ALIGN(4) {
        KEEP(*(.text._start))
        *(.text .text.*)
    } > REGION_TEXT

    .data : ALIGN(4) {
        *(.data .data.* .sdata .sdata.*)
    } > RAM

    .bss (NOLOAD) : ALIGN(4) {
        __bss_start = .;
        *(.sbss .sbss.* .bss .bss.* COMMON)
        . = ALIGN(4);
        __bss_end = .;
    } > RAM

    .stack (NOLOAD) : ALIGN(16) {
        __stack_bottom = .;
        . += 16K;
        __stack_top = .;
    } > RAM

    __kernel_end = .;

    /* Log ring and RTT control block (kernel/src/log.rs). Not loaded and not
     * zeroed: the kernel initialises it at boot (and, from L3, first checks
     * what the previous boot left there). */
    .log_mem (NOLOAD) : ALIGN(16) {
        *(.log_mem .log_mem.*)
    } > REGION_LOG

    /* Log schemas (doc/12). Not loaded; read by host tools from the ELF. */
    .t5log (INFO) : { KEEP(*(.t5log .t5log.*)) }

    /DISCARD/ : { *(.eh_frame .eh_frame_hdr) }
}
