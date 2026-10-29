#![no_std]
#![no_main]

//! M2 test app: fills all 32 FP registers with a pattern unique to `id`,
//! busy-waits for `duration_ms` without giving up the CPU, then checks the
//! registers. Run two of these together: they are only switched by timer
//! preemption, and each switch has to move the FPU registers between them.
//!
//! Exit 1: FP registers changed. Exit 2: never saw a gap in time, i.e. was
//! never preempted.

use t5_rt::{event, time_ns};

t5_rt::entry!(main);

/// A jump in `time_ns()` this large means another process ran in between.
const GAP_NS: u64 = 2_000_000;

fn main(id: usize, duration_ms: usize) -> usize {
    let mut want = [0u32; 32];
    for (k, w) in want.iter_mut().enumerate() {
        *w = (id as u32) << 24 | (k as u32) << 16 | 0xA5A5;
    }
    let mut got = [0u32; 32];

    unsafe { load_fregs(&want) };
    let start = time_ns();
    let mut last = start;
    let mut gaps = 0u32;
    let mut iters = 0u32;
    while last - start < duration_ms as u64 * 1_000_000 {
        let t = time_ns();
        if t - last > GAP_NS {
            gaps += 1;
        }
        last = t;
        iters += 1;
    }
    unsafe { store_fregs(&mut got) };

    let bad = want.iter().zip(got.iter()).filter(|(w, g)| w != g).count();
    event!(Info, "spin.done", id = id, iterations = iters, gaps = gaps, fp_mismatches = bad);
    if bad != 0 {
        1
    } else if gaps == 0 {
        2
    } else {
        0
    }
}

/// The registers are declared clobbered so the compiler saves the callee-saved
/// ones around `main`; nothing between the two calls uses FP.
#[inline(always)]
unsafe fn load_fregs(v: &[u32; 32]) {
    unsafe {
        core::arch::asm!(
            "flw f0, 0({p})",
            "flw f1, 4({p})",
            "flw f2, 8({p})",
            "flw f3, 12({p})",
            "flw f4, 16({p})",
            "flw f5, 20({p})",
            "flw f6, 24({p})",
            "flw f7, 28({p})",
            "flw f8, 32({p})",
            "flw f9, 36({p})",
            "flw f10, 40({p})",
            "flw f11, 44({p})",
            "flw f12, 48({p})",
            "flw f13, 52({p})",
            "flw f14, 56({p})",
            "flw f15, 60({p})",
            "flw f16, 64({p})",
            "flw f17, 68({p})",
            "flw f18, 72({p})",
            "flw f19, 76({p})",
            "flw f20, 80({p})",
            "flw f21, 84({p})",
            "flw f22, 88({p})",
            "flw f23, 92({p})",
            "flw f24, 96({p})",
            "flw f25, 100({p})",
            "flw f26, 104({p})",
            "flw f27, 108({p})",
            "flw f28, 112({p})",
            "flw f29, 116({p})",
            "flw f30, 120({p})",
            "flw f31, 124({p})",
            p = in(reg) v.as_ptr(),
            out("f0") _, out("f1") _, out("f2") _, out("f3") _, out("f4") _, out("f5") _, out("f6") _, out("f7") _, out("f8") _, out("f9") _, out("f10") _, out("f11") _, out("f12") _, out("f13") _, out("f14") _, out("f15") _, out("f16") _, out("f17") _, out("f18") _, out("f19") _, out("f20") _, out("f21") _, out("f22") _, out("f23") _, out("f24") _, out("f25") _, out("f26") _, out("f27") _, out("f28") _, out("f29") _, out("f30") _, out("f31") _,
            options(nostack, readonly),
        );
    }
}

#[inline(always)]
unsafe fn store_fregs(v: &mut [u32; 32]) {
    unsafe {
        core::arch::asm!(
            "fsw f0, 0({p})",
            "fsw f1, 4({p})",
            "fsw f2, 8({p})",
            "fsw f3, 12({p})",
            "fsw f4, 16({p})",
            "fsw f5, 20({p})",
            "fsw f6, 24({p})",
            "fsw f7, 28({p})",
            "fsw f8, 32({p})",
            "fsw f9, 36({p})",
            "fsw f10, 40({p})",
            "fsw f11, 44({p})",
            "fsw f12, 48({p})",
            "fsw f13, 52({p})",
            "fsw f14, 56({p})",
            "fsw f15, 60({p})",
            "fsw f16, 64({p})",
            "fsw f17, 68({p})",
            "fsw f18, 72({p})",
            "fsw f19, 76({p})",
            "fsw f20, 80({p})",
            "fsw f21, 84({p})",
            "fsw f22, 88({p})",
            "fsw f23, 92({p})",
            "fsw f24, 96({p})",
            "fsw f25, 100({p})",
            "fsw f26, 104({p})",
            "fsw f27, 108({p})",
            "fsw f28, 112({p})",
            "fsw f29, 116({p})",
            "fsw f30, 120({p})",
            "fsw f31, 124({p})",
            p = in(reg) v.as_mut_ptr(),
            options(nostack),
        );
    }
}
