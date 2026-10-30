//! Round-robin scheduler driven by a one-shot timer (doc/07 §4.3, doc/10).
//!
//! The kernel is a loop: pick a ready process, arm the alarm for the end of
//! its time slice (or the earliest wake-up of a sleeping process, whichever
//! comes first), run it until it traps, and act on the trap. The kernel never
//! runs with interrupts enabled, so there is no locking; the alarm only ever
//! interrupts U-mode.

use t5_abi::event;
use t5_abi::log::Addr;

use crate::arch::riscv32::csr::{cause, mstatus};
use crate::arch::riscv32::fpu::{self, FpState};
use crate::arch::riscv32::pmp;
use crate::arch::riscv32::trap::{self, Trap};
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

/// Processes in fixed slots (slot `i` uses pool slot `i` and FPU save area
/// `i`). The kernel runs it in two ways: the self-test runs a set of
/// processes until they exit, the service loop runs forever and starts and
/// stops processes from `poll` (doc/14 §3).
pub struct Scheduler {
    pub slots: [Option<Process>; MAX_PROCS],
    fpu: Fpu,
    pub stats: Stats,
    next: usize,
}

/// How often `run(None, ..)` calls `poll` while processes keep the CPU busy
/// or everyone sleeps.
pub const POLL_MS: u64 = 10;

impl Scheduler {
    pub fn new() -> Scheduler {
        Scheduler {
            slots: [None, None, None, None],
            fpu: Fpu { owner: None, dirty: false, saved: [FpState::default(); MAX_PROCS] },
            stats: Stats::default(),
            next: 0,
        }
    }

    pub fn free_slot(&self) -> Option<usize> {
        self.slots.iter().position(Option::is_none)
    }

    /// Stops the process with `pid`. Its slot is freed by `reap`.
    pub fn kill(&mut self, pid: u32) -> bool {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if let Some(p) = slot
                && p.pid == pid
                && p.exit().is_none()
            {
                p.state = State::Exited(Exit::Killed);
                self.fpu.release(i);
                log_exit(p, Exit::Killed);
                return true;
            }
        }
        false
    }

    /// Frees the slots of processes that have exited.
    pub fn reap(&mut self) {
        for slot in self.slots.iter_mut() {
            if slot.as_ref().is_some_and(|p| p.exit().is_some()) {
                *slot = None;
            }
        }
    }

    /// With `Some(limit_ms)`: runs until every process has exited or the
    /// limit has passed (processes still alive then are killed). With
    /// `None`: runs forever, calling `poll` at least every `POLL_MS`.
    pub fn run(&mut self, limit_ms: Option<u64>, poll: &mut dyn FnMut(&mut Scheduler)) {
        let limit = limit_ms.map(|ms| Current::now() + time::ms_to_ticks(ms));
        let mut next_poll = Current::now();

        loop {
            Current::feed_watchdog();
            Current::display_service();
            crate::hostlink::poll();
            let now = Current::now();
            if limit.is_none() && now >= next_poll {
                poll(self);
                next_poll = now + time::ms_to_ticks(POLL_MS);
            }
            if let Some(limit) = limit
                && now >= limit
            {
                for slot in self.slots.iter_mut().flatten() {
                    if slot.exit().is_none() {
                        slot.state = State::Exited(Exit::Killed);
                        log_exit(slot, Exit::Killed);
                    }
                }
                break;
            }
            // The earliest of: the limit (self-test) or the next poll
            // (service), and the wake-ups of sleeping or waiting processes.
            let mut wake = limit.unwrap_or(next_poll.max(now));
            let mut woken = None;
            for (i, p) in self.slots.iter_mut().enumerate() {
                let Some(p) = p else { continue };
                if p.try_wake(now) {
                    woken.get_or_insert(i);
                } else if let Some(t) = p.wake_at() {
                    wake = wake.min(t);
                }
            }

            // A process whose sleep just ended runs next instead of waiting
            // for its round-robin turn (up to a slice per busy process).
            if let Some(w) = woken {
                self.next = w;
            }
            let n = MAX_PROCS;
            let ready = |s: &Option<Process>| s.as_ref().is_some_and(|p| p.state == State::Ready);
            let Some(i) = (0..n).map(|k| (self.next + k) % n).find(|&i| ready(&self.slots[i])) else {
                if limit.is_some() && self.slots.iter().flatten().all(|p| p.exit().is_some()) {
                    break;
                }
                // Everyone is asleep (or nothing runs). Wake at least every
                // second to feed the watchdog.
                Current::set_alarm(wake.min(now + time::ms_to_ticks(1000)));
                Current::wait_for_interrupt();
                Current::clear_alarm();
                self.stats.idle_waits += 1;
                continue;
            };
            self.next = (i + 1) % n;

            Current::set_alarm(wake.min(now + time::ms_to_ticks(SLICE_MS)));
            let p = self.slots[i].as_mut().unwrap();
            run_slice(p, i, &mut self.fpu, &mut self.stats);
            Current::clear_alarm();
            if let Some(exit) = p.exit() {
                self.fpu.release(i);
                log_exit(p, exit);
            }
        }
        Current::clear_alarm();
    }
}

/// Logs how `p` ended and closes its handles, so a channel's other end
/// learns of it now rather than when the slot is reused.
fn log_exit(p: &mut Process, exit: Exit) {
    p.handles.close_all();
    match exit {
        Exit::Code(code) => event!(Info, "proc.exit", pid = p.pid, code = code),
        Exit::Fault { code, pc, tval } => event!(
            Warn,
            "proc.fault" => "{pid} killed by {cause_name} at {pc}",
            pid = p.pid,
            cause = code,
            cause_name = cause::name(code),
            pc = Addr(pc as u32),
            tval = Addr(tval as u32)
        ),
        Exit::Killed => event!(Warn, "proc.killed", pid = p.pid),
    }
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
            Trap::Interrupt { code } if Current::handle_irq(code) => {}
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
