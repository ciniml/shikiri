/*
 * Copyright (c) 2026 Kenta IDA
 * Distributed under the Boost Software License, Version 1.0.
 * (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
 * SPDX-License-Identifier: BSL-1.0
 */

/* XIP layout for tab5-os apps (doc/15): like t5app.ld, but the data is
 * linked 0x0800_0000 above the text, the distance between the ESP32-P4's
 * flash and PSRAM windows. The text then runs from flash where it lies and
 * the data sits at the same offset in the PSRAM window. */
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

    . = 0x08000000;

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
