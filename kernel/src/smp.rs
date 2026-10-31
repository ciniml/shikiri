//! The second core (SMP, doc/32, doc/33). S1: core 0 starts core 1, which
//! sets itself up, reports and waits in RAM. Core 0 waits until it has
//! reported, so the two never use the kernel (its log, its tables) at the
//! same time yet.

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

unsafe extern "C" {
    fn _start_secondary();
}

/// This core's number (mhartid).
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
    cores
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
    park()
}

/// Core 1 waits here, in RAM: flash operations on core 0 suspend the cache
/// that kernel text is read through (doc/14 §5). Nothing is routed to it,
/// so it sleeps.
#[unsafe(link_section = ".ram_text")]
#[inline(never)]
fn park() -> ! {
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}
