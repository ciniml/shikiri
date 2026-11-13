/*
 * Copyright (c) 2026 Kenta IDA
 * Distributed under the Boost Software License, Version 1.0.
 * (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
 * SPDX-License-Identifier: BSL-1.0
 */

/*
 * Minimal M/U + PMP probe for QEMU (tab5-os emulator survey).
 *  (a) M-mode console output
 *  (b) PMP (2 NAPOT entries) + mret into U-mode
 *  (c) U-mode ecall -> mcause 8
 *  (d) U-mode access to non-PMP region -> mcause 1/5/7
 * Build with -DPLAT_VIRT (qemu virt, ns16550 @0x10000000) or
 * -DPLAT_ESP32C3 (espressif qemu -M esp32c3, UART0 FIFO @0x60000000).
 */
#if defined(PLAT_VIRT)
#define UART 0x10000000
#elif defined(PLAT_ESP32C3)
#define UART 0x60000000
#else
#error "define PLAT_*"
#endif

#define NAPOT_4K(sym) ((sym >> 2) | 0x1ff)

    .option norvc
    .section .text.start, "ax"
    .globl _start
_start:
    la   sp, kstack_top
    csrw mscratch, sp
    la   t0, trap
    csrw mtvec, t0
    la   a0, s_hello
    call puts
    la a0, s_misa;    call puts; csrr a0, misa;    call puthex
    la a0, s_mtvec;   call puts; csrr a0, mtvec;   call puthex
    /* try CLIC mode (mtvec.MODE=3) and read back */
    csrr t0, mtvec
    ori  t1, t0, 3
    csrw mtvec, t1
    la a0, s_mtvec3;  call puts; csrr a0, mtvec;   call puthex
    la   t0, trap
    csrw mtvec, t0
    la a0, s_mstatus; call puts; csrr a0, mstatus; call puthex

    /* PMP granularity probe on entry 15: write all-ones, read back */
    li   t0, -1
    csrw pmpaddr15, t0
    la a0, s_pmp15; call puts; csrr a0, pmpaddr15; call puthex
    csrw pmpaddr15, zero

    /* entry0: user text 4KiB NAPOT R-X, entry1: user data 4KiB NAPOT RW- */
    la   t0, utext
    srli t0, t0, 2
    ori  t0, t0, 0x1ff
    csrw pmpaddr0, t0
    la   t0, udata
    srli t0, t0, 2
    ori  t0, t0, 0x1ff
    csrw pmpaddr1, t0
    li   t0, (0x1d) | (0x1b << 8)
    csrw pmpcfg0, t0
    la a0, s_pmpcfg0; call puts; csrr a0, pmpcfg0;  call puthex
    la a0, s_pmpaddr0; call puts; csrr a0, pmpaddr0; call puthex

    /* enter U: MPP=0, MPIE=0 */
    la   t0, uentry
    csrw mepc, t0
    li   t0, 0x1880
    csrc mstatus, t0
    la a0, s_mret; call puts
    mret

    /* ---------------- trap handler (direct mode, 64B aligned) ----- */
    .balign 256
trap:
    csrrw sp, mscratch, sp
    addi sp, sp, -48
    sw   ra, 0(sp)
    sw   t0, 4(sp)
    sw   t1, 8(sp)
    sw   t2, 12(sp)
    sw   a0, 16(sp)
    sw   a1, 20(sp)
    sw   a2, 24(sp)
    sw   a7, 28(sp)

    la a0, s_trap;  call puts; csrr a0, mcause; call puthex_nonl
    la a0, s_mepc;  call puts; csrr a0, mepc;   call puthex_nonl
    la a0, s_mtval; call puts; csrr a0, mtval;  call puthex_nonl
    la a0, s_mpp;   call puts; csrr a0, mstatus; srli a0, a0, 11; andi a0, a0, 3; call puthex

    csrr t0, mcause
    li   t1, 8
    beq  t0, t1, is_ecall
    li   t1, 11
    beq  t0, t1, is_ecall
    li   t1, 1
    beq  t0, t1, is_ifault
    li   t1, 5
    beq  t0, t1, skip
    li   t1, 7
    beq  t0, t1, skip
    la a0, s_unexp; call puts
    j    halt
is_ecall:
    lw   a7, 28(sp)
    li   t1, 99
    beq  a7, t1, done
    j    skip
is_ifault:
    lw   t0, 0(sp)           /* user ra */
    csrw mepc, t0
    j    out
skip:
    csrr t0, mepc
    addi t0, t0, 4
    csrw mepc, t0
out:
    lw   ra, 0(sp)
    lw   t0, 4(sp)
    lw   t1, 8(sp)
    lw   t2, 12(sp)
    lw   a0, 16(sp)
    lw   a1, 20(sp)
    lw   a2, 24(sp)
    lw   a7, 28(sp)
    addi sp, sp, 48
    csrrw sp, mscratch, sp
    mret

done:
    la a0, s_done; call puts
halt:
#if defined(PLAT_VIRT)
    li   t0, 0x100000         /* sifive_test: 0x5555 = pass/exit */
    li   t1, 0x5555
    sw   t1, 0(t0)
#endif
1:  wfi
    j 1b

    /* ---------------- console ---------------- */
putc:
    li   t0, UART
    sw   a0, 0(t0)
    ret
puts:
    mv   t2, ra
    mv   t1, a0
1:  lbu  a0, 0(t1)
    beqz a0, 2f
    li   t0, UART
    sw   a0, 0(t0)
    addi t1, t1, 1
    j    1b
2:  mv   ra, t2
    ret
puthex_nonl:
    li   a2, 0
    j    3f
puthex:
    li   a2, 1
3:  mv   a1, a0
    li   t1, 28
    li   t0, UART
4:  srl  t2, a1, t1
    andi t2, t2, 0xf
    addi t2, t2, '0'
    li   a0, '9'
    ble  t2, a0, 5f
    addi t2, t2, 39
5:  sw   t2, 0(t0)
    addi t1, t1, -4
    bgez t1, 4b
    beqz a2, 6f
    li   t2, '\n'
    sw   t2, 0(t0)
6:  ret

kfunc:                         /* kernel code; U must not execute it */
    ret

    .section .rodata
s_hello:   .asciz "\n[M] hello from M-mode\n"
s_misa:    .asciz "[M] misa="
s_mtvec:   .asciz "[M] mtvec="
s_mtvec3:  .asciz "[M] mtvec after writing MODE=3: "
s_mstatus: .asciz "[M] mstatus="
s_pmp15:   .asciz "[M] pmpaddr15 after writing ~0 = "
s_pmpcfg0: .asciz "[M] pmpcfg0="
s_pmpaddr0:.asciz "[M] pmpaddr0="
s_mret:    .asciz "[M] mret -> U\n"
s_trap:    .asciz "[M] trap mcause="
s_mepc:    .asciz " mepc="
s_mtval:   .asciz " mtval="
s_mpp:     .asciz " MPP="
s_unexp:   .asciz "[M] UNEXPECTED trap, halt\n"
s_done:    .asciz "[M] U exit(ecall a7=99): DONE\n"

    /* ---------------- user text (PMP0, R-X) ---------------- */
    .section .utext, "ax"
    .balign 4096
utext:
uentry:
    li   a7, 1
    ecall                      /* (c) expect mcause 8 */
    la   t0, kdata
    lw   t1, 0(t0)             /* (d) expect mcause 5 */
    sw   t1, 0(t0)             /* (d) expect mcause 7 */
    la   t0, kfunc
    jalr ra, 0(t0)             /* (d) expect mcause 1, handler resumes at ra */
    la   t0, udata
    li   t1, 0x1234
    sw   t1, 0(t0)             /* allowed */
    lw   t2, 0(t0)
    li   t0, UART
    sw   t1, 0(t0)             /* MMIO from U: expect mcause 7 */
    la   t0, utext
    sw   t1, 0(t0)             /* write to own R-X text: expect 7 */
    la   t0, udata
    jalr ra, 0(t0)             /* exec own RW- data: expect 1 */
    li   a7, 99
    ecall                      /* exit */
1:  j 1b

    .section .udata, "aw"
    .balign 4096
udata:
    .word 0
    .balign 4096

    .section .kdata, "aw"
    .balign 4096
kdata:
    .word 0xdeadbeef
    .balign 16
    .space 2048
kstack_top:
