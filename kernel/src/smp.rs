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
/// Set by core 1 once it is up.
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
    let cores = 1 + CORE_UP.load(Ordering::Acquire) as usize;
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
/// caller has set), and takes the lock again; all in RAM.
#[unsafe(link_section = ".ram_text")]
#[inline(never)]
pub fn idle() {
    let me = hartid() & 1;
    WAITING[me].store(1, Ordering::Release);
    SERVING.store(SERVING.load(Ordering::Relaxed).wrapping_add(1), Ordering::Release);
    unsafe { core::arch::asm!("wfi") };
    take();
    WAITING[me].store(0, Ordering::Relaxed);
}

/// Lets the other core have the lock if it waits for it (a ticket lock
/// serves it first).
pub fn pause() {
    unlock();
    lock();
}

/// Called with the lock held, before the cache is suspended (flash and MMU
/// operations): waits until the other core is in RAM, waiting for the lock
/// or asleep. It cannot leave while the lock is held. False if it did not
/// get there in 100 ms (logged).
#[allow(dead_code)] // Only the P4 suspends its cache.
pub fn quiesce() -> bool {
    if TWO_CORES.load(Ordering::Acquire) == 0 {
        return true;
    }
    let other = 1 - (hartid() & 1);
    let end = Current::now() + time::ms_to_ticks(100);
    while WAITING[other].load(Ordering::Acquire) == 0 {
        if Current::now() > end {
            event!(Error, "smp.quiesce_timeout", hart = other as u32);
            return false;
        }
    }
    true
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
    CORE_UP.store(1, Ordering::Release);
    // Core 0 takes the lock once it has seen CORE_UP; this core waits for
    // it in RAM and then runs processes for good.
    lock();
    crate::sched::run_secondary()
}
