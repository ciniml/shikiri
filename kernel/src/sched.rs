//! Round-robin scheduler driven by a one-shot timer (doc/07 §4.3, doc/10).
//!
//! The kernel is a loop: pick a ready process, arm the alarm for the end of
//! its time slice (or the earliest wake-up of a sleeping process, whichever
//! comes first), run it until it traps, and act on the trap. The kernel never
//! runs with interrupts enabled, so there is no locking; the alarm only ever
//! interrupts U-mode.

use crate::arch::riscv32::csr::cause;
use crate::arch::riscv32::fpu::{self, FpState};
use crate::arch::riscv32::pmp;
use crate::arch::riscv32::trap::{self, Trap};
use crate::arch::riscv32::csr::mstatus;
use crate::platform::{Current, Platform};
use crate::process::{Exit, Next, Process, State};
use crate::time;

pub const MAX_PROCS: usize = 4;
pub const SLICE_MS: u64 = 10;

/// Counters the self-test checks to see that each path was exercised.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub preemptions: u32,
    pub fpu_switches: u32,
    pub fpu_saves: u32,
    pub idle_waits: u32,
}

/// Owner of the FPU registers (see `arch::riscv32::fpu`).
struct Fpu {
    owner: Option<usize>,
    /// The owner wrote the registers since they were loaded.
    dirty: bool,
    saved: [FpState; MAX_PROCS],
}

impl Fpu {
    fn grant(&mut self, to: usize, stats: &mut Stats) {
        if let Some(o) = self.owner {
            if self.dirty {
                fpu::save(&mut self.saved[o]);
                stats.fpu_saves += 1;
            }
        }
        fpu::restore(&self.saved[to]);
        self.owner = Some(to);
        self.dirty = false;
        stats.fpu_switches += 1;
    }

    fn release(&mut self, i: usize) {
        if self.owner == Some(i) {
            self.owner = None;
            self.dirty = false;
        }
    }
}

/// Runs `procs` until all have exited or `limit_ms` has passed. Processes
/// still alive at the limit are killed.
pub fn run(procs: &mut [Process], limit_ms: u64) -> Stats {
    assert!(procs.len() <= MAX_PROCS);
    let mut fpu = Fpu { owner: None, dirty: false, saved: [FpState::default(); MAX_PROCS] };
    let mut stats = Stats::default();
    let limit = Current::now() + time::ms_to_ticks(limit_ms);
    let mut next = 0;

    loop {
        let now = Current::now();
        if now >= limit {
            for p in procs.iter_mut() {
                if p.exit().is_none() {
                    p.state = State::Exited(Exit::Killed);
                }
            }
            break;
        }
        let mut wake = limit;
        let mut woken = None;
        for (i, p) in procs.iter_mut().enumerate() {
            if let State::Sleeping { until } = p.state {
                if until <= now {
                    p.state = State::Ready;
                    woken.get_or_insert(i);
                } else {
                    wake = wake.min(until);
                }
            }
        }

        // A process whose sleep just ended runs next instead of waiting for
        // its round-robin turn (up to a slice per busy process).
        if let Some(w) = woken {
            next = w;
        }
        let n = procs.len();
        let Some(i) = (0..n).map(|k| (next + k) % n).find(|&i| procs[i].state == State::Ready) else {
            if procs.iter().all(|p| p.exit().is_some()) {
                break;
            }
            // Everyone is asleep.
            Current::set_alarm(wake);
            Current::wait_for_interrupt();
            Current::clear_alarm();
            stats.idle_waits += 1;
            continue;
        };
        next = (i + 1) % n;

        Current::set_alarm(wake.min(now + time::ms_to_ticks(SLICE_MS)));
        run_slice(&mut procs[i], i, &mut fpu, &mut stats);
        Current::clear_alarm();
        if procs[i].exit().is_some() {
            fpu.release(i);
        }
    }
    Current::clear_alarm();
    stats
}

/// Runs process `i` until it is preempted, gives up the CPU or ends.
fn run_slice(p: &mut Process, i: usize, fpu: &mut Fpu, stats: &mut Stats) {
    pmp::set_process(&p.region);
    loop {
        let owns_fpu = fpu.owner == Some(i);
        let fs = if owns_fpu { mstatus::FS_CLEAN } else { mstatus::FS_OFF };
        let t = trap::run_user(&mut p.frame, fs);
        if owns_fpu && fpu::dirty() {
            fpu.dirty = true;
        }
        match t {
            Trap::Interrupt { code } if code == Current::TIMER_IRQ => {
                stats.preemptions += 1;
                return;
            }
            Trap::Interrupt { code } => panic!("unexpected interrupt {code}"),
            Trap::Syscall => {
                // ecall is always 4 bytes long.
                p.frame.pc += 4;
                match p.syscall() {
                    Next::Resume => {}
                    Next::Switch => return,
                }
            }
            // The first FP instruction of a process that does not own the
            // FPU. Hand it over and retry; if the instruction is illegal for
            // another reason it faults again with the FPU enabled.
            Trap::Exception { code: cause::ILLEGAL_INSN, .. } if !owns_fpu => {
                fpu.grant(i, stats);
            }
            Trap::Exception { code, pc, tval } => {
                p.state = State::Exited(Exit::Fault { code, pc, tval });
                return;
            }
        }
    }
}
