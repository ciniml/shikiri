/* Shared kernel layout. Platform scripts define the RAM region, the
 * REGION_TEXT / REGION_RODATA / REGION_HOT / REGION_LOG / REGION_BSS /
 * REGION_STACK1 aliases and the KERNEL_STACK and KERNEL_STACK1 (second
 * core) sizes before including this file. */
ENTRY(_start)

SECTIONS
{
    /* ESP-IDF app descriptor; must start the first flash segment (empty on
     * platforms that do not use the ESP-IDF bootloader). */
    .flash.appdesc : ALIGN(4) {
        KEEP(*(.flash.appdesc))
    } > REGION_RODATA

    /* The read-only data of .hot_text's modules (jump tables and such),
     * beside it. Before .rodata so that its patterns take these first. */
    .hot_rodata : ALIGN(4) {
        __hot_start = .;
        *(.rodata.*9t5_kernel5sched* .rodata.*9t5_kernel7process* .rodata.*9t5_kernel6handle*)
        *(.rodata.*9t5_kernel7channel* .rodata.*9t5_kernel3eth* .rodata.*9t5_kernel4wlan*)
        *(.rodata.*9esp32p4v14emac* .rodata.*9esp32p4v14sdio*)
    } > REGION_HOT

    .rodata : ALIGN(4) {
        *(.rodata .rodata.* .srodata .srodata.*)
    } > REGION_RODATA

    /* The ESP32-P4 bootloader requires exactly two flash-mapped segments
     * (bootloader_utility.c unpack_load_app), so keep rodata and text apart. */
    .text_gap (NOLOAD) : {
        . = . + 8;
        . = ALIGN(4) + 0x20;
    } > REGION_RODATA

    /* Code the kernel runs all the time (doc/58 R5): the trap path, the
     * scheduler, the syscalls, the handles and the network devices. On the
     * ESP32-P4 it runs from RAM rather than through the flash window and
     * the L2 cache the apps' PSRAM shares.
     * Before .text so that its patterns take these first. */
    .hot_text : ALIGN(64) {
        __hot_text_start = .;
        KEEP(*(.text.trap_entry))
        *(.text.switch_to_user_asm)
        *(.text.*9t5_kernel4arch* .text.*9t5_kernel5sched* .text.*9t5_kernel7process*)
        *(.text.*9t5_kernel6handle* .text.*9t5_kernel7channel* .text.*9t5_kernel3shm*)
        *(.text.*9t5_kernel3smp* .text.*9t5_kernel4time* .text.*9t5_kernel3eth* .text.*9t5_kernel4wlan*)
        *(.text.*9esp32p4v14emac* .text.*9esp32p4v14sdio*)
        *(.text.*8Platform8eth_recv .text.*8Platform8eth_send .text.*8Platform11eth_pending)
        *(.text.*8Platform9alarm_due .text.*8Platform9set_alarm .text.*8Platform11clear_alarm)
        *(.text.memcpy .text.memmove .text.memset .text.memcmp .text.bcmp .text.*17compiler_builtins*)
        . = ALIGN(64);
        __hot_text_end = .;
    } > REGION_HOT

    .text : ALIGN(4) {
        KEEP(*(.text._start))
        *(.text .text.*)
    } > REGION_TEXT

    /* Code that must not run from flash: on the ESP32-P4 it runs while the
     * L2 cache, and with it the flash window, is suspended (doc/14 §5).
     * Loaded into RAM by the bootloader (P4) or QEMU (virt). Both ends are
     * on 64-byte (L1 cache line) boundaries, so writes to the data next to
     * it never share a line with code. */
    .ram_text : ALIGN(64) {
        __ram_text_start = .;
        KEEP(*(.ram_text .ram_text.*))
        . = ALIGN(64);
        __ram_text_end = .;
    } > RAM

    .data : ALIGN(4) {
        *(.data .data.* .sdata .sdata.*)
    } > RAM

    .bss (NOLOAD) : ALIGN(4) {
        __bss_start = .;
        *(.sbss .sbss.* .bss .bss.* COMMON)
        . = ALIGN(4);
        __bss_end = .;
    } > REGION_BSS

    .stack (NOLOAD) : ALIGN(16) {
        __stack_bottom = .;
        . += KERNEL_STACK;
        __stack_top = .;
    } > RAM

    /* The second core's stack (doc/33 §2). */
    .stack1 (NOLOAD) : ALIGN(16) {
        __stack1_bottom = .;
        . += KERNEL_STACK1;
        __stack1_top = .;
    } > REGION_STACK1

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
