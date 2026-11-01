//! The second core (SMP, doc/32, doc/33).
//!
//! - S1: core 0 starts core 1, which sets itself up and reports.
//! - S2: one big kernel lock (`lock`/`unlock`): a core holds it whenever it
//!   runs kernel code, and lets go only while it runs a process (U-mode) or
//!   sleeps. Core 1 then runs processes of the scheduler core 0 has
//!   published (`sched::ACTIVE`); devices, control commands and the
//!   watchdog stay on core 0.
//! - Waiting for the lock, and sleeping, happen in RAM (`.ram_text`) with
//!   `WAITING` set: before a flash or MMU operation suspends the cache that
//!   kernel text and process memory are read through (doc/14 §5), core 0,
//!   which holds the lock then, waits until core 1 is there (`quiesce`).
//! - S3 (doc/33 §10): an interrupt from the other core (IPI) wakes a
//!   sleeping core when a process is ready for it (`kick`), and brings a
//!   core running a process into the lock wait before a flash or MMU
//!   operation (`quiesce`). After new code is loaded, each core runs
//!   `fence.i` before its next process (`sync_fetch`).

use core::sync::atomic::{AtomicU32, Ordering};

use t5_abi::event;
use t5_abi::log::Addr;

use crate::arch::riscv32::csr::read_csr;
use crate::arch::riscv32::{self as rv, pmp};
use crate::platform::{Current, Platform};
use crate::time;

/// Other harts wait in `_start` until this is not 0 (QEMU virt starts all
/// harts there; on the ESP32-P4 Core1 starts where core 0 says).
pub static SECONDARY_GO: AtomicU32 = AtomicU32::new(0);
/// 1: core 1 is up and joins. 2: core 0 gave up waiting; core 1, if it
/// comes later, stays parked (it is not counted, so `quiesce` would not
/// wait for it).
static CORE_UP: AtomicU32 = AtomicU32::new(0);
/// The big kernel lock, a ticket lock: a core takes the next ticket and
/// waits until it is served, so the cores take turns. (A plain
/// compare-and-swap lock starved core 1: core 0 takes it again a few
/// instructions after a process's system call, doc/33 §5.)
static NEXT: AtomicU32 = AtomicU32::new(0);
static SERVING: AtomicU32 = AtomicU32::new(0);
/// Per hart: it waits for the lock or sleeps, in RAM.
static WAITING: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
/// Whether core 1 runs (and so takes part in locking).
static TWO_CORES: AtomicU32 = AtomicU32::new(0);
/// Per hart: it sleeps in `idle` (set and cleared with the lock held, so
/// a core holding the lock sees it exactly).
static ASLEEP: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
/// Bumped when code was loaded; per hart, the value at its last `fence.i`.
static CODE_GEN: AtomicU32 = AtomicU32::new(0);
static FETCH_GEN: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

unsafe extern "C" {
    fn _start_secondary();
}

/// This core's number (mhartid). Inlined: the RAM functions use it.
#[inline(always)]
pub fn hartid() -> usize {
    read_csr!("mhartid")
}

/// Starts core 1 and waits up to 100 ms for it to report. The number of
/// cores running.
pub fn start() -> usize {
    if !Current::start_secondary(_start_secondary as *const () as usize) {
        event!(Info, "smp.single", reason = "no second core on this platform");
        return 1;
    }
    let end = Current::now() + time::ms_to_ticks(100);
    while CORE_UP.load(Ordering::Acquire) == 0 && Current::now() < end {}
    let cores = match CORE_UP.compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => 1,
        Err(_) => 2,
    };
    event!(Info, "smp.started", cores = cores as u32);
    if cores == 2 {
        TWO_CORES.store(1, Ordering::Release);
    }
    // From here core 0 holds the lock except while it runs a process or
    // sleeps.
    lock();
    cores
}

/// Takes the big kernel lock, waiting in RAM.
#[unsafe(link_section = ".ram_text")]
#[inline(never)]
pub fn lock() {
    let me = hartid() & 1;
    WAITING[me].store(1, Ordering::Release);
    take();
    WAITING[me].store(0, Ordering::Relaxed);
}

#[unsafe(link_section = ".ram_text")]
#[inline(never)]
pub fn unlock() {
    SERVING.store(SERVING.load(Ordering::Relaxed).wrapping_add(1), Ordering::Release);
}

/// `pause` (Zihintpause; a FENCE with no successor set, so an ordinary
/// fence where the hint is unknown). t5emu gives the other core its turn.
#[inline(always)]
fn spin_hint() {
    unsafe { core::arch::asm!(".insn i 0x0F, 0, x0, x0, 0x010") };
}

#[inline(always)]
fn take() {
    let ticket = NEXT.fetch_add(1, Ordering::Relaxed);
    while SERVING.load(Ordering::Acquire) != ticket {
        spin_hint();
    }
}

/// Lets go of the lock, sleeps until an interrupt is pending (the alarm the
/// caller has set, or `kick` from the other core), and takes the lock
/// again; all in RAM. A kick sent before the `wfi` stays pending, so it is
/// not lost.
#[unsafe(link_section = ".ram_text")]
#[inline(never)]
pub fn idle() {
    let me = hartid() & 1;
    WAITING[me].store(1, Ordering::Release);
    ASLEEP[me].store(1, Ordering::Relaxed);
    SERVING.store(SERVING.load(Ordering::Relaxed).wrapping_add(1), Ordering::Release);
    unsafe { core::arch::asm!("wfi") };
    take();
    ASLEEP[me].store(0, Ordering::Relaxed);
    WAITING[me].store(0, Ordering::Relaxed);
    // The lock is held, so no flash operation runs: flash code is fine.
    // Cleared before the caller looks for work, so a later kick is seen.
    Current::ipi_clear();
}

/// Lets the other core have the lock if it waits for it (a ticket lock
/// serves it first).
pub fn pause() {
    unlock();
    lock();
}

/// Whether the other core sleeps in `idle` (lock held).
pub fn other_asleep() -> bool {
    TWO_CORES.load(Ordering::Acquire) != 0 && ASLEEP[1 - (hartid() & 1)].load(Ordering::Relaxed) != 0
}

/// Wakes the other core if it sleeps (lock held): it has a process to run.
/// Whether an interrupt was sent.
pub fn kick() -> bool {
    if !other_asleep() {
        return false;
    }
    Current::ipi_send(1 - (hartid() & 1));
    true
}

/// Called with the lock held, before the cache is suspended (flash and MMU
/// operations): brings the other core into RAM, waiting for the lock or
/// asleep. A core running a process is interrupted (the IPI); it then waits
/// for the lock, which it cannot get while this core holds it. False if it
/// did not get there in 100 ms (logged).
#[allow(dead_code)] // Only the P4 suspends its cache.
pub fn quiesce() -> bool {
    if TWO_CORES.load(Ordering::Acquire) == 0 {
        return true;
    }
    let other = 1 - (hartid() & 1);
    if WAITING[other].load(Ordering::Acquire) == 0 {
        Current::ipi_send(other);
    }
    let end = Current::now() + time::ms_to_ticks(100);
    while WAITING[other].load(Ordering::Acquire) == 0 {
        if Current::now() > end {
            event!(Error, "smp.quiesce_timeout", hart = other as u32);
            return false;
        }
    }
    true
}

/// New code was loaded (or mapped): every core runs `fence.i` before it
/// next runs a process. The I-caches of both cores are invalidated by the
/// platform (`sync_code`, `map_xip`); `fence.i` covers what this core's
/// pipeline fetched already.
pub fn code_changed() {
    CODE_GEN.fetch_add(1, Ordering::Release);
}

/// Before a process runs on this core: `fence.i` if code changed since.
#[inline]
pub fn sync_fetch() {
    let me = hartid() & 1;
    let latest = CODE_GEN.load(Ordering::Acquire);
    if FETCH_GEN[me].load(Ordering::Relaxed) != latest {
        FETCH_GEN[me].store(latest, Ordering::Relaxed);
        unsafe { core::arch::asm!("fence.i") };
    }
}

/// Whether core 1 is running.
pub fn two_cores() -> bool {
    TWO_CORES.load(Ordering::Acquire) != 0
}

/// Core 1's kernel entry (from `_start_secondary`, on its own stack).
pub extern "C" fn kmain_secondary() -> ! {
    Current::init_secondary();
    rv::init_trap_vector(Current::MTVEC_MODE);
    pmp::reset();
    let sp: usize;
    unsafe { core::arch::asm!("mv {}, sp", out(reg) sp) };
    event!(Info, "smp.core_up", hart = hartid() as u32, mtvec = Addr(read_csr!("mtvec") as u32), sp = Addr(sp as u32));
    if CORE_UP.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
        park();
    }
    // Core 0 takes the lock once it has seen CORE_UP; this core waits for
    // it in RAM and then runs processes for good.
    lock();
    crate::sched::run_secondary()
}

/// Core 1 when it came too late: sleeps in RAM for good (nothing is routed
/// to it).
#[unsafe(link_section = ".ram_text")]
#[inline(never)]
fn park() -> ! {
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}
