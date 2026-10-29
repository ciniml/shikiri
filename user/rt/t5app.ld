/* Static-PIE layout for tab5-os apps. Linked at 0; the kernel relocates the
 * image to wherever it loads it. [0, __t5_text_end) becomes R-X and
 * [__t5_text_end, __t5_mem_end) RW- under PMP, so the boundary is aligned to
 * the ESP32-P4 PMP granularity (128 bytes). */
ENTRY(_start)

SECTIONS
{
    . = 0;
    .text : {
        KEEP(*(.text._start))
        *(.text .text.*)
    }
    .rodata : ALIGN(4) {
        *(.rodata .rodata.* .srodata .srodata.*)
    }
    /* Dynamic-linking metadata. Only the host tool reads it. */
    .dynsym : { *(.dynsym) }
    .dynstr : { *(.dynstr) }
    .hash : { *(.hash) }
    .gnu.hash : { *(.gnu.hash) }
    .rela.dyn : { *(.rela.dyn .rela.*) }

    . = ALIGN(128);
    __t5_text_end = .;

    .dynamic : { *(.dynamic) }
    .got : { *(.got .got.*) }
    .data : {
        *(.data .data.* .sdata .sdata.*)
    }
    .bss (NOLOAD) : ALIGN(4) {
        *(.sbss .sbss.* .bss .bss.* COMMON)
    }
    . = ALIGN(16);
    __t5_mem_end = .;

    /* Log schemas (doc/12). Not loaded; read by host tools from the ELF. */
    .t5log (INFO) : { KEEP(*(.t5log .t5log.*)) }

    /DISCARD/ : { *(.eh_frame .eh_frame_hdr .comment .riscv.attributes) }
}
