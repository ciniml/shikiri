pub mod csr;
pub mod fpu;
pub mod pmp;
pub mod trap;

core::arch::global_asm!(
    r#"
    .section .text._start, "ax"
    .global _start
_start:
    csrci mstatus, 8        # MIE = 0
    csrw mscratch, zero     # 0 = the kernel is running
    # The kernel has no gp-relative data; make stray uses obvious.
    li gp, 0
    # Other harts (QEMU starts them all here) wait until core 0 lets them
    # go (doc/33 §2). On the ESP32-P4, Core1 starts at _start_secondary.
    csrr t0, mhartid
    bnez t0, 6f
    la sp, __stack_top
    la t0, __bss_start
    la t1, __bss_end
1:  bgeu t0, t1, 2f
    sw zero, 0(t0)
    addi t0, t0, 4
    j 1b
    # Paint the stack so its deepest use can be measured (stack_usage).
2:  la t0, __stack_bottom
    li t2, {paint}
4:  bgeu t0, sp, 5f
    sw t2, 0(t0)
    addi t0, t0, 4
    j 4b
5:  call {kmain}
3:  wfi
    j 3b
6:  la t0, {go}
7:  lw t1, 0(t0)
    beqz t1, 7b
    fence
    j _start_secondary

    .global _start_secondary
_start_secondary:
    csrci mstatus, 8
    csrw mscratch, zero
    li gp, 0
    la sp, __stack1_top
    call {kmain_secondary}
8:  wfi
    j 8b
    "#,
    kmain = sym crate::kmain,
    kmain_secondary = sym crate::smp::kmain_secondary,
    go = sym crate::smp::SECONDARY_GO,
    paint = const STACK_PAINT,
);

const STACK_PAINT: u32 = 0x5AC4_5AC4;

unsafe extern "C" {
    static __stack_bottom: u32;
    static __stack_top: u32;
    static __ram_text_start: u32;
    static __hot_start: u32;
    static __bss_end: u32;
    static __stack1_bottom: u32;
    static __stack1_top: u32;
}

/// The kernel's RAM (doc/61): its image (code run from RAM, data, the hot
/// code and bss) and its stacks (both cores'), in bytes.
pub fn kernel_ram() -> (usize, usize) {
    let a = |s: &u32| s as *const u32 as usize;
    unsafe {
        let stacks = a(&__stack_top) - a(&__stack_bottom) + a(&__stack1_top) - a(&__stack1_bottom);
        let low = a(&__stack_bottom) - a(&__ram_text_start);
        let high = a(&__bss_end).saturating_sub(a(&__hot_start));
        (low + high, stacks)
    }
}

/// This core's cycle counter (low 32 bits of `mcycle`): cheap, unlike the
/// SYSTIMER, but per core and at the CPU clock.
#[inline(always)]
pub fn cycles() -> u32 {
    let c: u32;
    unsafe { core::arch::asm!("csrr {0}, mcycle", out(reg) c) };
    c
}

/// The kernel stack's size and the most of it used so far (bytes): how far
/// down from the top the boot-time paint has been overwritten.
pub fn stack_usage() -> (u32, u32) {
    let bottom = core::ptr::addr_of!(__stack_bottom) as usize;
    let top = core::ptr::addr_of!(__stack_top) as usize;
    let mut a = bottom;
    while a < top && unsafe { core::ptr::read_volatile(a as *const u32) } == STACK_PAINT {
        a += 4;
    }
    ((top - bottom) as u32, (top - a) as u32)
}

/// Points mtvec at `trap_entry`. `mode_bits` selects the trap mode (0 = direct
/// on QEMU virt, 3 = CLIC on ESP32-P4).
pub fn init_trap_vector(mode_bits: usize) {
    let base = trap::trap_entry as *const () as usize;
    csr::write_csr!("mtvec", base | mode_bits);
}
