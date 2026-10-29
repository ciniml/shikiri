//! QEMU `virt` machine, used to test the architecture layer (doc/08).
//!
//! qemu-system-riscv32 -M virt -cpu rv32,s=off,h=off,mmu=off,d=off,pmp=on \
//!     -bios none -nographic -kernel t5-kernel

use super::Platform;

const UART0: usize = 0x1000_0000;
const UART_THR: usize = 0;
const UART_LSR: usize = 5;
const UART_LSR_THRE: u8 = 1 << 5;
const SIFIVE_TEST: usize = 0x0010_0000;
/// Stand-in for the flash on virt: RAM at 0x8400_0000 (within QEMU's
/// default 128 MiB) holds an image of the flash layout.
const FLASH_IMAGE: usize = 0x8400_0000;
// CLINT (hw/riscv/virt.c, hw/intc/riscv_aclint.c): 10 MHz mtime.
const CLINT_MTIMECMP: usize = 0x0200_4000;
const CLINT_MTIME: usize = 0x0200_BFF8;

pub struct Virt;

impl Platform for Virt {
    const NAME: &'static str = "qemu-virt";
    const MTVEC_MODE: usize = 0;
    // The kernel's 1 MiB is at 0x8000_0000 (link-virt.ld).
    const APP_POOL: core::ops::Range<usize> = 0x8010_0000..0x8020_0000;
    const PROTECTED_MMIO: usize = UART0;
    const TIMER_HZ: u64 = 10_000_000;
    const TIMER_IRQ: usize = 7; // machine timer interrupt

    fn init() {
        Self::clear_alarm();
        crate::arch::riscv32::csr::write_csr!("mie", crate::arch::riscv32::csr::mie::MTIE);
    }

    fn now() -> u64 {
        // On RV32 the halves are read separately; retry if hi changed.
        loop {
            let hi = unsafe { core::ptr::read_volatile((CLINT_MTIME + 4) as *const u32) };
            let lo = unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u32) };
            if hi == unsafe { core::ptr::read_volatile((CLINT_MTIME + 4) as *const u32) } {
                return ((hi as u64) << 32) | lo as u64;
            }
        }
    }

    fn set_alarm(at: u64) {
        // Raise hi first so no intermediate value is in the past.
        unsafe {
            core::ptr::write_volatile((CLINT_MTIMECMP + 4) as *mut u32, u32::MAX);
            core::ptr::write_volatile(CLINT_MTIMECMP as *mut u32, at as u32);
            core::ptr::write_volatile((CLINT_MTIMECMP + 4) as *mut u32, (at >> 32) as u32);
        }
    }

    fn clear_alarm() {
        // mtip follows mtime >= mtimecmp, so a far-future compare clears it.
        Self::set_alarm(u64::MAX);
    }

    fn console_write(bytes: &[u8]) -> bool {
        for &b in bytes {
            unsafe {
                while core::ptr::read_volatile((UART0 + UART_LSR) as *const u8) & UART_LSR_THRE == 0 {}
                core::ptr::write_volatile((UART0 + UART_THR) as *mut u8, b);
            }
        }
        true
    }

    fn map_flash() -> Option<usize> {
        // `make test-virt` loads the app store images here with QEMU's
        // generic loader, at FLASH_IMAGE + their flash offsets.
        Some(FLASH_IMAGE)
    }

    fn sync_code(_addr: usize, _len: usize) {
        unsafe { core::arch::asm!("fence.i") };
    }

    fn exit(code: u32) -> ! {
        // sifive_test: 0x5555 = pass, (code << 16) | 0x3333 = fail with code.
        let v = if code == 0 { 0x5555 } else { (code << 16) | 0x3333 };
        unsafe { core::ptr::write_volatile(SIFIVE_TEST as *mut u32, v) };
        loop {}
    }
}
