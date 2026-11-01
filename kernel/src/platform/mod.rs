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
    /// mcause code of the interrupt from the other core (doc/33 §10).
    const IPI_IRQ: usize = usize::MAX;

    /// Early hardware setup: watchdogs, console, the timer and its interrupt.
    /// Interrupts stay globally disabled in M-mode (MIE=0).
    fn init();
    /// Starts the second core at `entry` (doc/33); false if there is none.
    fn start_secondary(_entry: usize) -> bool {
        false
    }
    /// The second core's own setup (its interrupt controller), on it.
    fn init_secondary() {}
    /// Raises the interrupt from the other core (`IPI_IRQ`) on `hart`.
    fn ipi_send(_hart: usize) {}
    /// Clears this core's interrupt from the other core.
    fn ipi_clear() {}
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
    /// Whether this core's alarm time has come (a timer interrupt before it
    /// is spurious, doc/33 §8).
    fn alarm_due() -> bool {
        true
    }
    /// Makes the whole flash readable and returns the address of flash
    /// offset 0, or `None` without flash (doc/14 §5.1). Called once.
    fn map_flash() -> Option<usize> {
        None
    }
    /// The flash offset of `addr` in the window `map_flash` returned.
    fn flash_offset(_addr: usize) -> Option<u32> {
        None
    }
    /// XIP (doc/15): maps `pages` 64 KiB flash pages from `flash_offset`
    /// as the text of process slot `slot`; returns the address
    /// `flash_offset` now appears at. Its data goes `t5a::XIP_DELTA` above.
    fn map_xip(_base: usize, _capacity: usize, _flash_offset: u32, _pages: usize) -> Option<usize> {
        None
    }
    /// Before `[addr, addr + len)` is used: writes back and drops the same
    /// memory under any other address it is mapped at (doc/15 §2).
    fn retire_alias(_addr: usize, _len: usize) {}
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
    /// Final clock and voltage setup, after the PSRAM is up (M4c).
    fn clock_init() {}
    /// Flash offset of the kernel image the bootloader started (doc/16
    /// §3.1); `None` where kernels are not updated in place.
    fn kernel_slot() -> Option<u32> {
        None
    }
    /// The chip revision, major * 100 + minor.
    fn chip_revision() -> u16 {
        0
    }
    /// Restarts the whole chip (the bootloader runs again).
    fn reset() -> ! {
        Self::exit(0)
    }
    /// Sets the CPU clock in MHz (the `clock` command); returns the new one.
    fn set_cpu_mhz(_mhz: u32) -> Result<u32, &'static str> {
        Err("not supported")
    }
    /// Turns the automatic voltage adjustment on or off (the `pvt` command).
    fn set_pvt(_on: bool) -> Result<u32, &'static str> {
        Err("not supported")
    }
    /// Logs the core voltage settings (`stage` names the moment).
    fn log_power(_stage: &'static str) {}
    /// Board devices around the display (M6a, doc/17 §5): releases the
    /// panel and touch resets and identifies the panel.
    fn board_init() -> Panel {
        Panel::None
    }
    /// Reads the touch points into `out`; returns how many (0 = no touch).
    fn touch_read(_out: &mut [TouchPoint; 5]) -> Result<usize, &'static str> {
        Err("no touch")
    }
    /// Brings the display up with its framebuffer at `fb` (M6b, doc/17).
    fn display_init(_panel: Panel, _fb: usize) -> Result<usize, &'static str> {
        Err("no display")
    }
    /// Keeps the display refreshing (restarts the frame DMA when a frame is
    /// done). Called from the scheduler loop.
    fn display_service() {}
    /// The framebuffer (address, bytes) once the display runs.
    fn display_buffer() -> Option<(usize, usize)> {
        None
    }
    /// The framebuffer's width and height in pixels.
    fn display_size() -> Option<(u16, u16)> {
        None
    }
    /// Makes `len` bytes of the framebuffer from `addr` visible to the
    /// display (writes them back from the cache).
    fn display_flush(_addr: usize, _len: usize) {}
    /// Frames sent to the panel so far.
    fn display_frames() -> u32 {
        0
    }
    /// Logs the display's frame count and rate.
    fn display_stats() -> Result<u32, &'static str> {
        Err("no display")
    }
    /// Handles an interrupt other than the timer; false if unknown.
    fn handle_irq(_code: usize) -> bool {
        false
    }
    /// Powers the Wi-Fi chip and identifies it on SDIO (N2b-1, doc/22).
    fn wlan_probe() -> Result<u32, &'static str> {
        Err("no Wi-Fi")
    }
    /// Starts the Wi-Fi chip's transport (about 2 s, doc/22).
    fn wlan_up() -> Result<(), &'static str> {
        Err("no Wi-Fi")
    }
    /// Passes what the Wi-Fi chip has sent to `sink` (interface type,
    /// payload).
    fn wlan_poll(_sink: &mut dyn FnMut(u8, &[u8])) -> Result<(), &'static str> {
        Ok(())
    }
    /// Whether the Wi-Fi chip has signalled something not yet read (the
    /// scheduler does not sleep then).
    fn wlan_pending() -> bool {
        false
    }
    /// (card interrupts taken, re-arms that found the line still low, ms
    /// in bus commands, bus commands).
    fn wlan_irq_stats() -> (u32, u32, u32, u32) {
        (0, 0, 0, 0)
    }
    /// Sends one payload to the Wi-Fi chip; `Err("busy")` when it has no room.
    fn wlan_send(_if_type: u8, _payload: &[u8]) -> Result<(), &'static str> {
        Err("no Wi-Fi")
    }
    /// Initialises the SD card if needed and logs what is on it (M7a).
    fn sd_info() -> Result<u32, &'static str> {
        Err("no SD card slot")
    }
    /// Writes, reads back and times the SD card's test sectors.
    fn sd_test() -> Result<u32, &'static str> {
        Err("no SD card slot")
    }
    /// The SD card's size in sectors (initialises it if needed).
    fn sd_sectors() -> Result<u32, &'static str> {
        Err("no SD card slot")
    }
    /// Reads `buf.len() / 512` sectors from `lba` (at most 4 KiB).
    fn sd_read(_lba: u32, _buf: &mut [u8]) -> Result<(), &'static str> {
        Err("no SD card slot")
    }
    /// Writes `data.len() / 512` sectors at `lba` (at most 4 KiB).
    fn sd_write(_lba: u32, _data: &[u8]) -> Result<(), &'static str> {
        Err("no SD card slot")
    }
    /// Logs sector `lba` of the SD card.
    fn sd_dump(_lba: u32) -> Result<u32, &'static str> {
        Err("no SD card slot")
    }
    /// Logs every I2C address on the board bus that acknowledges.
    fn i2c_scan() -> Result<u32, &'static str> {
        Err("no I2C")
    }
    /// Reads what the console line has received (non-blocking); 0 if
    /// nothing or no input.
    fn console_read(_buf: &mut [u8]) -> usize {
        0
    }
    /// Whether the console line has received bytes not read yet.
    fn console_pending() -> bool {
        false
    }
    /// Lets bytes arriving on the console line wake `wait_for_interrupt`
    /// (while the host link is open, doc/21 §6).
    fn console_wake(_on: bool) {}
    /// Restarts the system watchdog (no-op where there is none).
    fn feed_watchdog() {}
    /// Turns the system watchdog on again (a debug probe turns it off when
    /// it attaches); no-op where there is none.
    fn arm_watchdog() {}
    /// Why the chip last reset (ROM RESET_REASON on the ESP32-P4; 0 = unknown).
    fn reset_cause() -> u32 {
        0
    }
    /// Ends the run. On QEMU this exits the emulator with `code`.
    fn exit(code: u32) -> !;
}

/// The display panel (Tab5 lots, doc/06 §7).
#[cfg_attr(not(feature = "platform-esp32p4v1"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    /// No board support, or the IO expander did not answer.
    None,
    /// Nothing identified on the touch addresses.
    Unknown,
    Ili9881c { touch_addr: u8 },
    St7123,
    St7121,
}

/// One touch point in panel pixels (720 x 1280, portrait).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TouchPoint {
    pub x: u16,
    pub y: u16,
    pub id: u16,
    pub size: u16,
}
