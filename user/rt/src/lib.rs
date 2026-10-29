//! User-mode runtime for tab5-os apps.
//!
//! The kernel has already copied the image, applied relocations, zeroed .bss
//! and set `sp` before jumping to `_start`, so the entry stub only has to call
//! into Rust. Apps define their entry point with [`entry!`].

#![no_std]

use core::fmt;

pub use t5_abi::{Error, sys};

core::arch::global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    "_start:",
    // a0/a1 carry arguments from the kernel. gp is deliberately left unset:
    // apps are linked without gp relaxation.
    "    call {start}",
    "1:  j 1b",
    start = sym t5_rt_start,
);

unsafe extern "Rust" {
    fn t5_main(arg0: usize, arg1: usize) -> usize;
}

extern "C" fn t5_rt_start(arg0: usize, arg1: usize) -> ! {
    let code = unsafe { t5_main(arg0, arg1) };
    exit(code)
}

/// Declares the app's entry point: `fn(arg0: usize, arg1: usize) -> usize`.
/// The return value becomes the process exit code.
#[macro_export]
macro_rules! entry {
    ($f:path) => {
        #[unsafe(no_mangle)]
        fn t5_main(arg0: usize, arg1: usize) -> usize {
            let f: fn(usize, usize) -> usize = $f;
            f(arg0, arg1)
        }
    };
}

#[inline(always)]
unsafe fn syscall2(nr: usize, a0: usize, a1: usize) -> (isize, usize) {
    let ret: isize;
    let val: usize;
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") nr,
            inlateout("a0") a0 => ret,
            inlateout("a1") a1 => val,
            lateout("a2") _, lateout("a3") _, lateout("a4") _, lateout("a5") _,
            lateout("a6") _, lateout("t0") _, lateout("t1") _, lateout("t2") _,
            lateout("t3") _, lateout("t4") _, lateout("t5") _, lateout("t6") _,
            lateout("ra") _,
            options(nostack),
        );
    }
    (ret, val)
}

fn result(ret: isize, val: usize) -> Result<usize, Error> {
    if ret < 0 {
        Err(Error::from_code(ret.unsigned_abs()))
    } else {
        Ok(val)
    }
}

/// Writes bytes to the kernel debug console. Returns the number written.
pub fn dbg_write(buf: &[u8]) -> Result<usize, Error> {
    let (ret, val) = unsafe { syscall2(sys::DBG_WRITE, buf.as_ptr() as usize, buf.len()) };
    result(ret, val)
}

/// Terminates the calling process.
pub fn exit(code: usize) -> ! {
    unsafe {
        syscall2(sys::EXIT, code, 0);
    }
    // The kernel never returns from exit.
    loop {}
}

/// `core::fmt` adapter over [`dbg_write`].
pub struct DebugConsole;

impl fmt::Write for DebugConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        dbg_write(s.as_bytes()).map(|_| ()).map_err(|_| fmt::Error)
    }
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::DebugConsole, $($arg)*);
    }};
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write as _;
    let _ = writeln!(DebugConsole, "app panic: {}", info);
    exit(t5_abi::EXIT_PANIC)
}
