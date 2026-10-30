//! SoC/board layer. The architecture layer (`arch::riscv32`) is shared; each
//! platform provides the console, exit path and memory layout (doc/08 §3.3).

#[cfg(feature = "platform-virt")]
mod virt;
#[cfg(feature = "platform-virt")]
pub use virt::Virt as Current;

#[cfg(feature = "platform-esp32p4v1")]
mod esp32p4v1;
#[cfg(feature = "platform-esp32p4v1")]
pub use esp32p4v1::Esp32p4v1 as Current;

pub trait Platform {
    const NAME: &'static str;
    /// mtvec mode bits (0 = direct, 3 = CLIC).
    const MTVEC_MODE: usize;
    /// Memory the kernel may hand to processes.
    const APP_POOL: core::ops::Range<usize>;
    /// An MMIO address U-mode must never reach (used by the self-test).
    const PROTECTED_MMIO: usize;
    /// After the self-test: keep running and take control commands
    /// (doc/14 §3), or end the run (QEMU in CI).
    const SERVE: bool;

    /// Frequency of the monotonic counter returned by `now`.
    const TIMER_HZ: u64;
    /// mcause exception code of the alarm interrupt.
    const TIMER_IRQ: usize;

    /// Early hardware setup: watchdogs, console, the timer and its interrupt.
    /// Interrupts stay globally disabled in M-mode (MIE=0).
    fn init();
    /// Raw byte output for log frames (doc/12 §2.4). Never blocks for long.
    /// Returns false if some bytes were dropped (no reader).
    fn console_write(bytes: &[u8]) -> bool;
    /// Makes code the kernel just wrote to `[addr, addr + len)` visible to
    /// instruction fetch.
    fn sync_code(addr: usize, len: usize);
    /// Monotonic time in counter ticks.
    fn now() -> u64;
    /// Arms the one-shot alarm at `at` ticks, replacing any earlier one. An
    /// `at` in the past fires immediately.
    fn set_alarm(at: u64);
    /// Disarms the alarm and clears its pending interrupt.
    fn clear_alarm();
    /// Makes the whole flash readable and returns the address of flash
    /// offset 0, or `None` without flash (doc/14 §5.1). Called once.
    fn map_flash() -> Option<usize> {
        None
    }
    /// Erases the 4 KiB flash sector at `offset` and drops it from the
    /// caches, so the window shows the new contents. Callers go through
    /// `crate::flash`, which checks the range (doc/14 §5.2).
    fn flash_erase_sector(_offset: u32) -> Result<(), &'static str> {
        Err("no flash")
    }
    /// Writes `data` (at most one 256-byte page, not crossing a page
    /// boundary, `offset` 4-byte aligned) to erased flash at `offset`.
    fn flash_write(_offset: u32, _data: &[u8]) -> Result<(), &'static str> {
        Err("no flash")
    }
    /// Powers up, tunes and maps the external PSRAM (M4a); returns the
    /// window address and size.
    fn psram_init() -> Result<(usize, usize), &'static str> {
        Err("no psram")
    }
    /// Tests the whole PSRAM (destroys its contents); returns its size.
    fn psram_test() -> Result<u32, &'static str> {
        Err("no psram")
    }
    /// Flash chip details the boot code left behind, for the log:
    /// (JEDEC device id, size in bytes).
    fn flash_chip() -> Option<(u32, u32)> {
        None
    }
    /// Logs platform details worth keeping (called once the log is up).
    fn log_info() {}
    /// Restarts the system watchdog (no-op where there is none).
    fn feed_watchdog() {}
    /// Why the chip last reset (ROM RESET_REASON on the ESP32-P4; 0 = unknown).
    fn reset_cause() -> u32 {
        0
    }
    /// Waits until an interrupt is pending (MIE stays 0, so no trap is taken).
    fn wait_for_interrupt() {
        unsafe { core::arch::asm!("wfi") };
    }
    /// Ends the run. On QEMU this exits the emulator with `code`.
    fn exit(code: u32) -> !;
}
