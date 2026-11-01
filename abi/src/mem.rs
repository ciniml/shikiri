//! `memcpy` and `memset` for RV32 (doc/34 §13), for the kernel and the
//! apps; they replace the weak ones of `compiler_builtins`.
//!
//! Those move one word per loop pass, about five cycles per word on the P4
//! even from the cache. `memcpy` here moves eight words a pass (aligned)
//! or four (a source misaligned against the destination, each word joined
//! from two aligned loads by shifts): 1.30 → 0.68 and 2.55 → 1.66 cycles a
//! byte on the P4. It makes no misaligned access: the P4 does them in
//! hardware, but at about 23 cycles a word. The word read past the last
//! needed byte of a misaligned source lies in the same aligned word, so in
//! the same PMP granule.

#[cfg(target_arch = "riscv32")]
core::arch::global_asm!(
    "
    .section .text.memcpy, \"ax\", @progbits
    .globl memcpy
    .type memcpy, @function
    .p2align 2
memcpy:
    mv      t6, a0
    li      t0, 16
    bltu    a2, t0, 7f
    // Align the destination.
1:  andi    t0, t6, 3
    beqz    t0, 2f
    lbu     t1, 0(a1)
    sb      t1, 0(t6)
    addi    a1, a1, 1
    addi    t6, t6, 1
    addi    a2, a2, -1
    j       1b
2:  andi    t3, a1, 3
    bnez    t3, 5f
    // Both aligned: 32 bytes a pass, then words.
    li      t0, 32
    bltu    a2, t0, 4f
3:  lw      a3, 0(a1)
    lw      a4, 4(a1)
    lw      a5, 8(a1)
    lw      a6, 12(a1)
    lw      a7, 16(a1)
    lw      t1, 20(a1)
    lw      t2, 24(a1)
    lw      t4, 28(a1)
    sw      a3, 0(t6)
    sw      a4, 4(t6)
    sw      a5, 8(t6)
    sw      a6, 12(t6)
    sw      a7, 16(t6)
    sw      t1, 20(t6)
    sw      t2, 24(t6)
    sw      t4, 28(t6)
    addi    a1, a1, 32
    addi    t6, t6, 32
    addi    a2, a2, -32
    bgeu    a2, t0, 3b
4:  li      t0, 4
    bltu    a2, t0, 7f
    lw      t1, 0(a1)
    sw      t1, 0(t6)
    addi    a1, a1, 4
    addi    t6, t6, 4
    addi    a2, a2, -4
    j       4b
    // Source misaligned by t3 bytes: aligned loads, each output word made
    // of two (little-endian: the low part from the earlier one).
5:  slli    t4, t3, 3
    neg     t5, t4
    andi    a1, a1, -4
    lw      t1, 0(a1)
    li      t0, 16
6:  bltu    a2, t0, 8f
    lw      a4, 4(a1)
    lw      a5, 8(a1)
    lw      a6, 12(a1)
    lw      a7, 16(a1)
    srl     t1, t1, t4
    sll     a3, a4, t5
    or      t1, t1, a3
    sw      t1, 0(t6)
    srl     a4, a4, t4
    sll     a3, a5, t5
    or      a4, a4, a3
    sw      a4, 4(t6)
    srl     a5, a5, t4
    sll     a3, a6, t5
    or      a5, a5, a3
    sw      a5, 8(t6)
    srl     a6, a6, t4
    sll     a3, a7, t5
    or      a6, a6, a3
    sw      a6, 12(t6)
    mv      t1, a7
    addi    a1, a1, 16
    addi    t6, t6, 16
    addi    a2, a2, -16
    j       6b
8:  li      t0, 4
9:  bltu    a2, t0, 10f
    lw      a4, 4(a1)
    srl     t1, t1, t4
    sll     a3, a4, t5
    or      t1, t1, a3
    sw      t1, 0(t6)
    mv      t1, a4
    addi    a1, a1, 4
    addi    t6, t6, 4
    addi    a2, a2, -4
    j       9b
10: add     a1, a1, t3
    // The last bytes (and short copies).
7:  beqz    a2, 11f
    lbu     t1, 0(a1)
    sb      t1, 0(t6)
    addi    a1, a1, 1
    addi    t6, t6, 1
    addi    a2, a2, -1
    j       7b
11: ret
    .size memcpy, . - memcpy
    
    .section .text.memset, \"ax\", @progbits
    .globl memset
    .type memset, @function
    .p2align 2
memset:
    mv      t6, a0
    andi    a1, a1, 0xFF
    li      t0, 16
    bltu    a2, t0, 4f
1:  andi    t0, t6, 3
    beqz    t0, 2f
    sb      a1, 0(t6)
    addi    t6, t6, 1
    addi    a2, a2, -1
    j       1b
    // The byte in all four lanes; 32 bytes a pass, then words.
2:  slli    t0, a1, 8
    or      a1, a1, t0
    slli    t0, a1, 16
    or      a1, a1, t0
    li      t0, 32
    bltu    a2, t0, 3f
5:  sw      a1, 0(t6)
    sw      a1, 4(t6)
    sw      a1, 8(t6)
    sw      a1, 12(t6)
    sw      a1, 16(t6)
    sw      a1, 20(t6)
    sw      a1, 24(t6)
    sw      a1, 28(t6)
    addi    t6, t6, 32
    addi    a2, a2, -32
    bgeu    a2, t0, 5b
3:  li      t0, 4
6:  bltu    a2, t0, 4f
    sw      a1, 0(t6)
    addi    t6, t6, 4
    addi    a2, a2, -4
    j       6b
4:  beqz    a2, 7f
    sb      a1, 0(t6)
    addi    t6, t6, 1
    addi    a2, a2, -1
    j       4b
7:  ret
    .size memset, . - memset
    "
);
