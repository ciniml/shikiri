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

pub struct Virt;

impl Platform for Virt {
    const NAME: &'static str = "qemu-virt";
    const MTVEC_MODE: usize = 0;
    // The kernel's 1 MiB is at 0x8000_0000 (link-virt.ld).
    const APP_POOL: core::ops::Range<usize> = 0x8010_0000..0x8020_0000;
    const PROTECTED_MMIO: usize = UART0;

    fn init() {}

    fn console_write(bytes: &[u8]) {
        for &b in bytes {
            unsafe {
                while core::ptr::read_volatile((UART0 + UART_LSR) as *const u8) & UART_LSR_THRE == 0 {}
                core::ptr::write_volatile((UART0 + UART_THR) as *mut u8, b);
            }
        }
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
