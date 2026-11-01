//! Round-robin scheduler driven by a one-shot timer (doc/07 §4.3, doc/10).
//!
//! The kernel is a loop: pick a ready process, arm the alarm for the end of
//! its time slice (or the earliest wake-up of a sleeping process, whichever
//! comes first), run it until it traps, and act on the trap. The kernel never
//! runs with interrupts enabled; the alarm only ever interrupts U-mode.
//!
//! Two cores (doc/33 §5): the scheduler running on core 0 is published in
//! `ACTIVE`, and core 1 runs its processes too (`run_secondary`). Both hold
//! the big kernel lock (`smp::lock`) whenever they touch it and let go only
//! while they run a process or sleep. A process runs on one core at a time
//! (`on_core`). The scheduler is reached through a raw pointer and borrowed
//! only while the lock is held, never across `run_user` or a sleep.

use t5_abi::event;
use t5_abi::log::Addr;

use crate::arch::riscv32::csr::{cause, mstatus};
use crate::arch::riscv32::fpu::{self, FpState};
use crate::arch::riscv32::pmp;
use crate::arch::riscv32::trap::{self, Trap};
use crate::platform::{Current, Platform};
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::process::{Exit, Next, Process, State};
use crate::prof;
use crate::smp;
use crate::time;

/// Processes at once (doc/30 §4). With the PSRAM, each gets 1/8 of the app
/// pool (3 MiB).
pub const MAX_PROCS: usize = 8;
pub const SLICE_MS: u64 = 10;
/// Cores.
pub const CORES: usize = 2;
/// `on_core` value of a process no core is running.
const NONE: u8 = 0xFF;
/// How long core 1 sleeps at most when it has nothing to run. A process
/// made ready elsewhere wakes it sooner (`kick_if_ready`, doc/33 §10).
const SECONDARY_IDLE_MS: u64 = 1000;
/// After a system call, the other core is checked for at most once per
/// this many cycles per core (1 ms at 360 MHz): checking after every call
/// cost `net` about 13% of a core, and Wi-Fi went from 8.9 to 7.8 Mbps
/// (doc/33 §12). Counted in cycles because reading the SYSTIMER takes
/// about 2 µs on the P4 (doc/34).
const KICK_CHECK_CYCLES: u32 = 360_000;

/// Counters the self-test checks to see that each path was exercised.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub preemptions: u32,
    pub fpu_switches: u32,
    pub fpu_saves: u32,
    pub idle_waits: u32,
    /// Time (ticks) each core slept (doc/31).
    pub idle_ticks: [u64; CORES],
    /// Slices each core ran (doc/33).
    pub slices: [u32; CORES],
    /// Timer interrupts before the alarm time, per core (doc/33 §8).
    pub spurious: [u32; CORES],
    /// Interrupts each core sent to wake the other (doc/33 §10).
    pub kicks: [u32; CORES],
    /// Interrupts from the other core each core took while running a
    /// process (a kick that came late, or `smp::quiesce`).
    pub ipis: [u32; CORES],
}

/// A core's FPU registers: whose state they hold (see `arch::riscv32::fpu`).
/// Given up at the end of each slice, saved if written, so a process can go
/// on on the other core.
#[derive(Clone, Copy)]
struct CoreFpu {
    owner: Option<usize>,
    /// The owner wrote the registers since they were loaded.
    dirty: bool,
}

/// Processes in fixed slots (slot `i` uses pool slot `i` and FPU save area
/// `i`). The kernel runs it in two ways: the self-test runs a set of
/// processes until they exit, the service loop runs forever and starts and
/// stops processes from `poll` (doc/14 §3).
pub struct Scheduler {
    pub slots: [Option<Process>; MAX_PROCS],
    /// The core running each slot's process, or `NONE`.
    on_core: [u8; MAX_PROCS],
    fpu: [CoreFpu; CORES],
    fpu_saved: [FpState; MAX_PROCS],
    pub stats: Stats,
    next: usize,
    /// At the last `ps`: the time and each core's idle total (ticks), so
    /// `ps` gives the use since then (doc/31).
    pub cpu_mark: (u64, [u64; CORES]),
    /// Per core: the cycle count (`mcycle`) of its next check after a
    /// system call.
    kick_check_at: [u32; CORES],
    /// Per core: when its sleep began, or 0 while it is awake. `ps` counts
    /// a sleep in progress up to now, so a window gets only its own part.
    pub asleep_since: [u64; CORES],
}

/// The scheduler core 0 is running, for core 1 (null: none).
static ACTIVE: AtomicPtr<Scheduler> = AtomicPtr::new(core::ptr::null_mut());

/// How often `run(None, ..)` calls `poll` while processes keep the CPU busy
/// or everyone sleeps.
pub const POLL_MS: u64 = 10;

impl Scheduler {
    pub fn new() -> Scheduler {
        Scheduler {
            slots: [const { None }; MAX_PROCS],
            on_core: [NONE; MAX_PROCS],
            fpu: [CoreFpu { owner: None, dirty: false }; CORES],
            fpu_saved: [FpState::default(); MAX_PROCS],
            stats: Stats::default(),
            next: 0,
            cpu_mark: (Current::now(), [0; CORES]),
            asleep_since: [0; CORES],
            kick_check_at: [0; CORES],
        }
    }

    pub fn free_slot(&self) -> Option<usize> {
        (0..MAX_PROCS).find(|&i| self.slots[i].is_none() && self.on_core[i] == NONE)
    }

    /// Stops the process with `pid`. Its slot is freed by `reap`. If the
    /// other core is running it, that core finishes it when it comes back
    /// (logs, closes its handles).
    pub fn kill(&mut self, pid: u32) -> bool {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if let Some(p) = slot
                && p.pid == pid
                && p.exit().is_none()
            {
                p.state = State::Exited(Exit::Killed);
                if self.on_core[i] == NONE {
                    log_exit(p, Exit::Killed);
                }
                return true;
            }
        }
        false
    }

    /// Frees the slots of processes that have exited (and no core runs).
    pub fn reap(&mut self) {
        for i in 0..MAX_PROCS {
            if self.on_core[i] == NONE && self.slots[i].as_ref().is_some_and(|p| p.exit().is_some()) {
                self.slots[i] = None;
            }
        }
    }

    /// Counts the sleeps in progress up to `now` (`ps`).
    pub fn settle_idle(&mut self, now: u64) {
        for c in 0..CORES {
            if self.asleep_since[c] != 0 {
                self.stats.idle_ticks[c] += now.saturating_sub(self.asleep_since[c]);
                self.asleep_since[c] = now;
            }
        }
    }

    /// Ends this core's sleep in the counts.
    fn woke(&mut self, core: usize) {
        let since = core::mem::take(&mut self.asleep_since[core]);
        if since != 0 {
            self.stats.idle_ticks[core] += Current::now().saturating_sub(since);
        }
    }

    /// Whether the other core runs one of the processes.
    fn busy_elsewhere(&self) -> bool {
        self.on_core.iter().any(|&c| c != NONE)
    }

    /// The next ready process no core runs, round robin from `next`.
    fn pick(&self) -> Option<usize> {
        let ready = |i: usize| self.on_core[i] == NONE && self.slots[i].as_ref().is_some_and(|p| p.state == State::Ready);
        (0..MAX_PROCS).map(|k| (self.next + k) % MAX_PROCS).find(|&i| ready(i))
    }

    /// Wakes processes whose sleep or wait is over (no core runs them);
    /// the first woken, and the earliest wake-up of the rest (from `wake`).
    fn wake(&mut self, now: u64, mut wake: u64) -> (Option<usize>, u64) {
        let mut woken = None;
        for i in 0..MAX_PROCS {
            if self.on_core[i] != NONE {
                continue;
            }
            let Some(p) = self.slots[i].as_mut() else { continue };
            if p.try_wake(now) {
                woken.get_or_insert(i);
            } else if let Some(t) = p.wake_at() {
                wake = wake.min(t);
            }
        }
        (woken, wake)
    }
}

/// With `Some(limit_ms)`: runs until every process has exited or the limit
/// has passed (processes still alive then are killed). With `None`: runs
/// forever, calling `poll` at least every `POLL_MS`. Core 0 only, with the
/// lock held; core 1 runs the same processes meanwhile.
pub fn run(this: *mut Scheduler, limit_ms: Option<u64>, poll: &mut dyn FnMut(&mut Scheduler)) {
    let limit = limit_ms.map(|ms| Current::now() + time::ms_to_ticks(ms));
    let mut next_poll = Current::now();
    ACTIVE.store(this, Ordering::Release);

    loop {
        Current::feed_watchdog();
        Current::display_service();
        crate::hostlink::poll();
        crate::wlan::poll();
        let now = Current::now();
        // Borrowed until the process runs or the core sleeps.
        let s = unsafe { &mut *this };
        if limit.is_none() && now >= next_poll {
            poll(s);
            next_poll = now + time::ms_to_ticks(POLL_MS);
        }
        if let Some(limit) = limit
            && now >= limit
        {
            // Processes the other core runs are finished by it when it
            // comes back.
            for i in 0..MAX_PROCS {
                if let Some(p) = s.slots[i].as_mut()
                    && p.exit().is_none()
                {
                    p.state = State::Exited(Exit::Killed);
                    if s.on_core[i] == NONE {
                        log_exit(p, Exit::Killed);
                    }
                }
            }
            break;
        }
        // The earliest of: the limit (self-test) or the next poll
        // (service), and the wake-ups of sleeping or waiting processes.
        let (woken, wake) = s.wake(now, limit.unwrap_or(next_poll.max(now)));
        // A process whose sleep just ended runs next instead of waiting
        // for its round-robin turn (up to a slice per busy process).
        if let Some(w) = woken {
            s.next = w;
        }
        let Some(i) = s.pick() else {
            if limit.is_some() && s.slots.iter().flatten().all(|p| p.exit().is_some()) {
                break;
            }
            if crate::hostlink::pending() || crate::wlan::pending() {
                continue;
            }
            // Everyone is asleep (or nothing runs). Wake at least every
            // second to feed the watchdog.
            Current::set_alarm(wake.min(now + time::ms_to_ticks(1000)));
            sleep(this);
            continue;
        };
        s.next = (i + 1) % MAX_PROCS;
        run_one(this, i, wake.min(now + time::ms_to_ticks(SLICE_MS)));
    }
    // Wait until the other core has given up every process of this
    // scheduler, then withdraw it.
    while unsafe { (*this).busy_elsewhere() } {
        smp::pause();
    }
    ACTIVE.store(core::ptr::null_mut(), Ordering::Release);
    Current::clear_alarm();
    if limit.is_some() {
        let st = unsafe { (*this).stats };
        event!(
            Debug,
            "sched.done",
            slices0 = st.slices[0],
            slices1 = st.slices[1],
            preemptions = st.preemptions,
            kicks0 = st.kicks[0],
            kicks1 = st.kicks[1],
            ipis0 = st.ipis[0],
            ipis1 = st.ipis[1]
        );
    }
}

/// Sleeps with the lock let go (the alarm is set), counting the time.
fn sleep(this: *mut Scheduler) {
    let core = smp::hartid() & 1;
    unsafe { (*this).asleep_since[core] = Current::now() };
    smp::idle();
    let s = unsafe { &mut *this };
    s.woke(core);
    s.stats.idle_waits += 1;
    Current::clear_alarm();
}

/// Runs slot `i` for a slice on this core until `deadline` (lock held).
fn run_one(this: *mut Scheduler, i: usize, deadline: u64) {
    Current::set_alarm(deadline);
    let core = smp::hartid() & 1;
    let p: *mut Process = {
        let s = unsafe { &mut *this };
        s.on_core[i] = core as u8;
        s.slots[i].as_mut().unwrap()
    };
    let start = Current::now();
    {
        let s = unsafe { &mut *this };
        s.stats.slices[core] += 1;
        // Another ready process for the other core, if it sleeps.
        kick_if_ready(s, core);
    }
    run_slice(this, p, i, core, deadline);
    let s = unsafe { &mut *this };
    let p = unsafe { &mut *p };
    p.cpu += Current::now() - start;
    Current::clear_alarm();
    // Give the FPU up, saved if written, so the process can go on anywhere.
    let f = &mut s.fpu[core];
    if f.owner == Some(i) {
        if f.dirty {
            fpu::save(&mut s.fpu_saved[i]);
            s.stats.fpu_saves += 1;
        }
        *f = CoreFpu { owner: None, dirty: false };
    }
    s.on_core[i] = NONE;
    if let Some(exit) = p.exit() {
        log_exit(p, exit);
    }
}

/// Wakes the other core if it sleeps and a process no core runs is ready
/// (lock held). Called when this core starts a process and after system
/// calls (`kick_after_syscall`), which may give a waiting process its event.
fn kick_if_ready(s: &mut Scheduler, core: usize) {
    if !smp::other_asleep() {
        return;
    }
    s.wake(Current::now(), u64::MAX);
    if s.pick().is_some() && smp::kick() {
        s.stats.kicks[core] += 1;
    }
}

/// `kick_if_ready` after a system call, at most every
/// `KICK_CHECK_CYCLES`.
fn kick_after_syscall(s: &mut Scheduler, core: usize) {
    if !smp::other_asleep() {
        return;
    }
    let now = crate::arch::riscv32::cycles();
    if (now.wrapping_sub(s.kick_check_at[core]) as i32) < 0 {
        return;
    }
    s.kick_check_at[core] = now.wrapping_add(KICK_CHECK_CYCLES);
    kick_if_ready(s, core);
}

/// Core 1's loop (doc/33 §5): runs ready processes of the published
/// scheduler; sleeps when there are none until a wake-up, a kick from core
/// 0 or `SECONDARY_IDLE_MS`.
pub fn run_secondary() -> ! {
    loop {
        let this = ACTIVE.load(Ordering::Acquire);
        let now = Current::now();
        // Sleep no later than the first wake-up: core 0 may be asleep on
        // an alarm set before a process this core ran began to wait.
        let mut wake = now + time::ms_to_ticks(SECONDARY_IDLE_MS);
        let picked = if this.is_null() {
            None
        } else {
            let s = unsafe { &mut *this };
            let (woken, w) = s.wake(now, wake);
            wake = w;
            if let Some(w) = woken {
                s.next = w;
            }
            let i = s.pick();
            if let Some(i) = i {
                s.next = (i + 1) % MAX_PROCS;
            }
            i
        };
        match picked {
            Some(i) => run_one(this, i, now + time::ms_to_ticks(SLICE_MS)),
            None => {
                Current::set_alarm(wake);
                if !this.is_null() {
                    unsafe { (*this).asleep_since[1] = Current::now() };
                }
                smp::idle();
                Current::clear_alarm();
                // Only if the same scheduler is still published (the
                // self-test makes a new one per case; a new one has 0).
                let now_active = ACTIVE.load(Ordering::Acquire);
                if !this.is_null() && now_active == this {
                    unsafe { (*this).woke(1) };
                }
            }
        }
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

/// Runs process `p` (slot `i`) on `core` until it is preempted at
/// `deadline`, gives up the CPU or ends. The lock is let go while it runs
/// in U-mode. A profiled process also traps at each sample tick
/// (`prof`).
fn run_slice(this: *mut Scheduler, p: *mut Process, i: usize, core: usize, deadline: u64) {
    let p = unsafe { &mut *p };
    pmp::set_process(&p.region);
    // A sample that fell due while the process did not run is not taken
    // when it comes back: it would look like kernel time.
    prof::resumed(p.pid);
    loop {
        let profiled = prof::next(p.pid);
        if let Some(at) = profiled {
            Current::set_alarm(deadline.min(at));
        }
        let entry_pc = p.frame.pc;
        let (owns_fpu, fs) = {
            let s = unsafe { &*this };
            let owns = s.fpu[core].owner == Some(i);
            (owns, if owns { mstatus::FS_CLEAN } else { mstatus::FS_OFF })
        };
        smp::sync_fetch();
        smp::unlock();
        let t = trap::run_user(&mut p.frame, fs);
        // Where the trap came from (doc/36): never M-mode for a process.
        let from_m = crate::arch::riscv32::csr::read_csr!("mstatus") & mstatus::MPP_MASK != 0;
        smp::lock();
        let s = unsafe { &mut *this };
        if owns_fpu && fpu::dirty() {
            s.fpu[core].dirty = true;
        }
        if p.exit().is_some() {
            // Killed from the other core while it ran.
            return;
        }
        if from_m {
            // It ran in M-mode since it was last resumed: nothing it did
            // was checked. Stop it, and say so loudly.
            event!(Error, "proc.mmode", pid = p.pid, core = core, entry_pc = Addr(entry_pc as u32), pc = Addr(p.frame.pc as u32), mcause = p.frame.mcause);
            p.state = State::Exited(Exit::Fault { code: p.frame.mcause & cause::CODE_MASK, pc: p.frame.pc, tval: p.frame.mtval });
            return;
        }
        match t {
            Trap::Interrupt { code } if code == Current::TIMER_IRQ => {
                if profiled.is_some() && Current::now() < deadline {
                    if Current::alarm_due() {
                        prof::sample(p.frame.pc, p.frame.pc == entry_pc);
                    } else {
                        s.stats.spurious[core] += 1;
                    }
                    continue;
                }
                if !Current::alarm_due() {
                    s.stats.spurious[core] += 1;
                    continue;
                }
                s.stats.preemptions += 1;
                return;
            }
            // The process goes on; a quiesce was over once the lock was
            // taken above.
            Trap::Interrupt { code } if code == Current::IPI_IRQ => {
                Current::ipi_clear();
                s.stats.ipis[core] += 1;
                // Core 1 queued Wi-Fi frames for this core to send.
                if core == 0 {
                    crate::wlan::flush_tx();
                }
            }
            Trap::Interrupt { code } if Current::handle_irq(code) => {}
            Trap::Interrupt { code } => panic!("unexpected interrupt {code}"),
            Trap::Syscall => {
                // ecall is always 4 bytes long.
                p.frame.pc += 4;
                let next = if profiled.is_some() {
                    let nr = p.frame.regs[crate::arch::riscv32::trap::reg::A7];
                    let c0 = crate::arch::riscv32::cycles();
                    let next = p.syscall();
                    prof::syscall(nr, crate::arch::riscv32::cycles().wrapping_sub(c0));
                    next
                } else {
                    p.syscall()
                };
                kick_after_syscall(s, core);
                match next {
                    Next::Resume => {}
                    Next::Switch => return,
                }
            }
            // The first FP instruction of a process that does not own the
            // FPU. Take it and retry; if the instruction is illegal for
            // another reason it faults again with the FPU enabled. The FPU
            // is free: it is given up at the end of every slice.
            Trap::Exception { code: cause::ILLEGAL_INSN, .. } if !owns_fpu => {
                fpu::restore(&s.fpu_saved[i]);
                s.fpu[core] = CoreFpu { owner: Some(i), dirty: false };
                s.stats.fpu_switches += 1;
            }
            Trap::Exception { code, pc, tval } => {
                p.state = State::Exited(Exit::Fault { code, pc, tval });
                return;
            }
        }
    }
}
