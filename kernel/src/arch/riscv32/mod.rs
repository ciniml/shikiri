pub mod csr;
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
    la sp, __stack_top
    la t0, __bss_start
    la t1, __bss_end
1:  bgeu t0, t1, 2f
    sw zero, 0(t0)
    addi t0, t0, 4
    j 1b
2:  call {kmain}
3:  wfi
    j 3b
    "#,
    kmain = sym crate::kmain,
);

/// Points mtvec at `trap_entry`. `mode_bits` selects the trap mode (0 = direct
/// on QEMU virt, 3 = CLIC on ESP32-P4).
pub fn init_trap_vector(mode_bits: usize) {
    let base = trap::trap_entry as *const () as usize;
    csr::write_csr!("mtvec", base | mode_bits);
}
