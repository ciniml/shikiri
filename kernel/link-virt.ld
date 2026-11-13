/*
 * Copyright (c) 2026 Kenta IDA
 * Distributed under the Boost Software License, Version 1.0.
 * (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
 * SPDX-License-Identifier: BSL-1.0
 */

/* QEMU virt: RAM starts at 0x8000_0000. The kernel owns the first 1 MiB; the
 * app pool (see platform/virt.rs) follows it.
 *
 * With -bios none QEMU jumps to the start of RAM rather than to the ELF
 * entry, so text gets its own region at 0x8000_0000 (_start comes first in
 * .text). */
MEMORY
{
    TEXT   : ORIGIN = 0x80000000, LENGTH = 512K
    RODATA : ORIGIN = 0x80080000, LENGTH = 256K
    RAM    : ORIGIN = 0x800C0000, LENGTH = 256K
}

REGION_ALIAS("REGION_TEXT", TEXT);
REGION_ALIAS("REGION_RODATA", RODATA);
REGION_ALIAS("REGION_LOG", RAM);
REGION_ALIAS("REGION_BSS", RAM);
/* Not TEXT: QEMU starts at its beginning, where _start must be. */
REGION_ALIAS("REGION_HOT", RODATA);
REGION_ALIAS("REGION_STACK1", RAM);
/* 92K: the 4 KiB I/O buffer moved from the stack to .bss (doc/34). */
KERNEL_STACK = 92K;
KERNEL_STACK1 = 32K;

INCLUDE kernel-common.ld
