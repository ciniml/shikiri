//! Lazy FPU context switching (doc/07 §4.3).
//!
//! The FPU registers belong to one process at a time, the owner. Every other
//! process runs with `mstatus.FS = Off`, so its first FP instruction traps as
//! an illegal instruction; the scheduler then saves the owner's registers (only
//! if the owner dirtied them) and loads the new owner's. Processes that never
//! touch the FPU never pay for it.
//!
//! The kernel is built for `riscv32imac` and has no FP code of its own, so
//! the registers are never clobbered behind the owner's back.

use super::csr::{mstatus, read_csr, write_csr};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FpState {
    pub f: [u32; 32], // 0
    pub fcsr: u32,    // 128
}

unsafe extern "C" {
    fn fpu_save_asm(state: *mut FpState);
    fn fpu_restore_asm(state: *const FpState);
}

/// Whether the last U-mode run wrote FP registers or fcsr.
pub fn dirty() -> bool {
    read_csr!("mstatus") & mstatus::FS_MASK == mstatus::FS_DIRTY
}

/// FP instructions trap while FS is Off, M-mode included.
fn enable() {
    let ms = read_csr!("mstatus");
    write_csr!("mstatus", (ms & !mstatus::FS_MASK) | mstatus::FS_INITIAL);
}

pub fn save(state: &mut FpState) {
    enable();
    unsafe { fpu_save_asm(state) };
}

pub fn restore(state: &FpState) {
    enable();
    unsafe { fpu_restore_asm(state) };
}

core::arch::global_asm!(
    r#"
    .option push
    .option arch, +f

    .section .text.fpu_save_asm, "ax"
    .global fpu_save_asm
    .align 2
fpu_save_asm:
    fsw f0, 0(a0)
    fsw f1, 4(a0)
    fsw f2, 8(a0)
    fsw f3, 12(a0)
    fsw f4, 16(a0)
    fsw f5, 20(a0)
    fsw f6, 24(a0)
    fsw f7, 28(a0)
    fsw f8, 32(a0)
    fsw f9, 36(a0)
    fsw f10, 40(a0)
    fsw f11, 44(a0)
    fsw f12, 48(a0)
    fsw f13, 52(a0)
    fsw f14, 56(a0)
    fsw f15, 60(a0)
    fsw f16, 64(a0)
    fsw f17, 68(a0)
    fsw f18, 72(a0)
    fsw f19, 76(a0)
    fsw f20, 80(a0)
    fsw f21, 84(a0)
    fsw f22, 88(a0)
    fsw f23, 92(a0)
    fsw f24, 96(a0)
    fsw f25, 100(a0)
    fsw f26, 104(a0)
    fsw f27, 108(a0)
    fsw f28, 112(a0)
    fsw f29, 116(a0)
    fsw f30, 120(a0)
    fsw f31, 124(a0)
    frcsr t0
    sw t0, 128(a0)
    ret

    .section .text.fpu_restore_asm, "ax"
    .global fpu_restore_asm
    .align 2
fpu_restore_asm:
    flw f0, 0(a0)
    flw f1, 4(a0)
    flw f2, 8(a0)
    flw f3, 12(a0)
    flw f4, 16(a0)
    flw f5, 20(a0)
    flw f6, 24(a0)
    flw f7, 28(a0)
    flw f8, 32(a0)
    flw f9, 36(a0)
    flw f10, 40(a0)
    flw f11, 44(a0)
    flw f12, 48(a0)
    flw f13, 52(a0)
    flw f14, 56(a0)
    flw f15, 60(a0)
    flw f16, 64(a0)
    flw f17, 68(a0)
    flw f18, 72(a0)
    flw f19, 76(a0)
    flw f20, 80(a0)
    flw f21, 84(a0)
    flw f22, 88(a0)
    flw f23, 92(a0)
    flw f24, 96(a0)
    flw f25, 100(a0)
    flw f26, 104(a0)
    flw f27, 108(a0)
    flw f28, 112(a0)
    flw f29, 116(a0)
    flw f30, 120(a0)
    flw f31, 124(a0)
    lw t0, 128(a0)
    fscsr t0
    ret

    .option pop
    "#
);
