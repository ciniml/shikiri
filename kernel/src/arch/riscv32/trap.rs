// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! Switching to U-mode and back.
//!
//! `switch_to_user` saves the kernel's callee-saved registers, loads the
//! process's registers and `mret`s. When the process traps, `trap_entry`
//! saves its registers into the same `TrapFrame` and returns from
//! `switch_to_user` as if it were an ordinary function call, so the kernel
//! drives processes from a plain loop (the same structure as Tock's RISC-V
//! port).
//!
//! `mscratch` holds the current process's `TrapFrame` while U-mode runs and
//! is 0 while the kernel runs. The entry path swaps `sp` with `mscratch`
//! before touching memory and never trusts the user `sp`: a fault before the
//! registers are saved would lock up the ESP32-P4 core (doc/01 §0).

use super::csr::{cause, mstatus, read_csr};

/// Register state of a process. Offsets are used by the assembly below.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TrapFrame {
    /// x0..x31; `regs[0]` is unused.
    pub regs: [usize; 32], // 0
    pub pc: usize,         // 128
    pub kernel_sp: usize,  // 132
    pub mcause: usize,     // 136
    pub mtval: usize,      // 140
}

pub mod reg {
    pub const SP: usize = 2;
    pub const A0: usize = 10;
    pub const A1: usize = 11;
    pub const A2: usize = 12;
    pub const A3: usize = 13;
    pub const A4: usize = 14;
    pub const A5: usize = 15;
    pub const A7: usize = 17;
}

/// Why a process stopped running.
#[derive(Clone, Copy, Debug)]
pub enum Trap {
    Syscall,
    Exception { code: usize, pc: usize, tval: usize },
    Interrupt { code: usize },
}

impl TrapFrame {
    pub fn trap(&self) -> Trap {
        let code = self.mcause & cause::CODE_MASK;
        if self.mcause & cause::INTERRUPT != 0 {
            Trap::Interrupt { code }
        } else if code == cause::ECALL_U {
            Trap::Syscall
        } else {
            Trap::Exception { code, pc: self.pc, tval: self.mtval }
        }
    }
}

unsafe extern "C" {
    fn switch_to_user_asm(frame: *mut TrapFrame);
    pub fn trap_entry();
}

/// Runs the process described by `frame` in U-mode until it traps. `fs` is
/// the `mstatus.FS` value the process runs with (see `fpu`).
pub fn run_user(frame: &mut TrapFrame, fs: usize) -> Trap {
    // In CLIC mode mret also restores the interrupt level from mcause.mpil;
    // clear it so U-mode always runs at level 0. mcause.mpp/mpie alias the
    // mstatus fields, so mstatus is written after it. On QEMU virt this is a
    // plain write to mcause.
    super::csr::write_csr!("mcause", 0);
    // The kernel itself keeps MIE=0 (doc/07 §4.3). MPIE=1 lets the timer
    // interrupt preempt U-mode.
    let mut ms = read_csr!("mstatus");
    ms &= !(mstatus::MPP_MASK | mstatus::MPRV | mstatus::FS_MASK);
    ms |= mstatus::MPIE | fs;
    super::csr::write_csr!("mstatus", ms);
    // A process must never go on in M-mode (doc/36: one did, after a
    // debugger halted the hart around this `mret`). Should MPP not be U
    // here, stop.
    let (back, mc) = (read_csr!("mstatus"), read_csr!("mcause"));
    if back & mstatus::MPP_MASK != 0 || (mc >> 28) & 3 != 0 {
        panic!("MPP not U before mret: mstatus={:#x} mcause={:#x}", back, mc);
    }
    unsafe { switch_to_user_asm(frame) };
    frame.trap()
}

core::arch::global_asm!(
    r#"
    .section .text.switch_to_user_asm, "ax"
    .global switch_to_user_asm
    .align 2
switch_to_user_asm:
    addi sp, sp, -64
    sw ra, 0(sp)
    sw s0, 4(sp)
    sw s1, 8(sp)
    sw s2, 12(sp)
    sw s3, 16(sp)
    sw s4, 20(sp)
    sw s5, 24(sp)
    sw s6, 28(sp)
    sw s7, 32(sp)
    sw s8, 36(sp)
    sw s9, 40(sp)
    sw s10, 44(sp)
    sw s11, 48(sp)
    sw sp, 132(a0)
    csrw mscratch, a0
    lw t0, 128(a0)
    csrw mepc, t0
    lw x1, 4(a0)
    lw x2, 8(a0)
    lw x3, 12(a0)
    lw x4, 16(a0)
    lw x5, 20(a0)
    lw x6, 24(a0)
    lw x7, 28(a0)
    lw x8, 32(a0)
    lw x9, 36(a0)
    lw x11, 44(a0)
    lw x12, 48(a0)
    lw x13, 52(a0)
    lw x14, 56(a0)
    lw x15, 60(a0)
    lw x16, 64(a0)
    lw x17, 68(a0)
    lw x18, 72(a0)
    lw x19, 76(a0)
    lw x20, 80(a0)
    lw x21, 84(a0)
    lw x22, 88(a0)
    lw x23, 92(a0)
    lw x24, 96(a0)
    lw x25, 100(a0)
    lw x26, 104(a0)
    lw x27, 108(a0)
    lw x28, 112(a0)
    lw x29, 116(a0)
    lw x30, 120(a0)
    lw x31, 124(a0)
    lw x10, 40(a0)
    mret

    # mtvec target. 64-byte aligned because the ESP32-P4 CLIC requires it.
    .section .text.trap_entry, "ax"
    .global trap_entry
    .align 6
trap_entry:
    csrrw sp, mscratch, sp
    beqz sp, 1f
    # From U-mode: sp = &TrapFrame, mscratch = user sp.
    sw x1, 4(sp)
    sw x3, 12(sp)
    sw x4, 16(sp)
    sw x5, 20(sp)
    sw x6, 24(sp)
    sw x7, 28(sp)
    sw x8, 32(sp)
    sw x9, 36(sp)
    sw x10, 40(sp)
    sw x11, 44(sp)
    sw x12, 48(sp)
    sw x13, 52(sp)
    sw x14, 56(sp)
    sw x15, 60(sp)
    sw x16, 64(sp)
    sw x17, 68(sp)
    sw x18, 72(sp)
    sw x19, 76(sp)
    sw x20, 80(sp)
    sw x21, 84(sp)
    sw x22, 88(sp)
    sw x23, 92(sp)
    sw x24, 96(sp)
    sw x25, 100(sp)
    sw x26, 104(sp)
    sw x27, 108(sp)
    sw x28, 112(sp)
    sw x29, 116(sp)
    sw x30, 120(sp)
    sw x31, 124(sp)
    csrr t0, mscratch
    sw t0, 8(sp)
    csrw mscratch, zero
    csrr t0, mepc
    sw t0, 128(sp)
    csrr t0, mcause
    sw t0, 136(sp)
    csrr t0, mtval
    sw t0, 140(sp)
    # Back onto the kernel stack and return from switch_to_user_asm.
    lw sp, 132(sp)
    lw ra, 0(sp)
    lw s0, 4(sp)
    lw s1, 8(sp)
    lw s2, 12(sp)
    lw s3, 16(sp)
    lw s4, 20(sp)
    lw s5, 24(sp)
    lw s6, 28(sp)
    lw s7, 32(sp)
    lw s8, 36(sp)
    lw s9, 40(sp)
    lw s10, 44(sp)
    lw s11, 48(sp)
    addi sp, sp, 64
    ret

    # From M-mode: undo the swap (mscratch is 0 again) and report. The
    # kernel runs with interrupts off (MIE=0), so this is always a bug.
1:  csrrw sp, mscratch, sp
    csrr a0, mcause
    csrr a1, mepc
    csrr a2, mtval
    j {kernel_trap}
    "#,
    kernel_trap = sym kernel_trap,
);

extern "C" fn kernel_trap(mcause: usize, mepc: usize, mtval: usize) -> ! {
    let code = mcause & cause::CODE_MASK;
    panic!(
        "trap in kernel: mcause={:#x} ({}) mepc={:#x} mtval={:#x}",
        mcause,
        cause::name(code),
        mepc,
        mtval
    );
}
