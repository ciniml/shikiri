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

    /// Early hardware setup: watchdogs, console.
    fn init();
    fn console_write(bytes: &[u8]);
    /// Makes code the kernel just wrote to `[addr, addr + len)` visible to
    /// instruction fetch.
    fn sync_code(addr: usize, len: usize);
    /// Ends the run. On QEMU this exits the emulator with `code`.
    fn exit(code: u32) -> !;
}

pub struct Console;

impl core::fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        <Current as Platform>::console_write(s.as_bytes());
        Ok(())
    }
}

#[macro_export]
macro_rules! kprintln {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::platform::Console, $($arg)*);
    }};
}
