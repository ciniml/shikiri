//! User-mode runtime for tab5-os apps.
//!
//! The kernel has already copied the image, applied relocations, zeroed .bss
//! and set `sp` before jumping to `_start`, so the entry stub only has to call
//! into Rust. Apps define their entry point with [`entry!`].

#![no_std]

pub use t5_abi::log::{self, Addr, Level};
pub use t5_abi::{Error, event, sys};

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

/// Makes syscall `nr`. Returns a0 (0 or -error) and the values in a1/a2.
#[inline(always)]
unsafe fn syscall2(nr: usize, a0: usize, a1: usize) -> (isize, usize, usize) {
    let ret: isize;
    let val: usize;
    let val2: usize;
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") nr,
            inlateout("a0") a0 => ret,
            inlateout("a1") a1 => val,
            lateout("a2") val2, lateout("a3") _, lateout("a4") _, lateout("a5") _,
            lateout("a6") _, lateout("t0") _, lateout("t1") _, lateout("t2") _,
            lateout("t3") _, lateout("t4") _, lateout("t5") _, lateout("t6") _,
            lateout("ra") _,
            options(nostack),
        );
    }
    (ret, val, val2)
}

/// Raw syscall access for tests and low-level code: returns a0 (0 or
/// -error) and the values in a1/a2.
///
/// # Safety
/// Pointer arguments must follow the syscall's contract.
pub unsafe fn raw_syscall(nr: usize, a0: usize, a1: usize) -> (isize, usize, usize) {
    unsafe { syscall2(nr, a0, a1) }
}

/// The `log_emit` syscall: the sink behind [`event!`] in apps. Logging never
/// fails from the app's point of view; a rejected payload is a bug in the
/// encoder and the kernel reports it as its own event.
#[unsafe(no_mangle)]
pub fn t5log_emit(payload: &[u8]) {
    unsafe { syscall2(sys::LOG_EMIT, payload.as_ptr() as usize, payload.len()) };
}

/// The lowest level the kernel currently keeps.
pub fn log_level() -> log::Level {
    let (_, v, _) = unsafe { syscall2(sys::LOG_LEVEL, 0, 0) };
    log::Level::from_u8(v as u8).unwrap_or(log::Level::Trace)
}

/// Gives up the rest of the time slice.
pub fn yield_now() {
    unsafe { syscall2(sys::YIELD, 0, 0) };
}

/// Sleeps for at least `ns` nanoseconds.
pub fn sleep_ns(ns: u64) {
    unsafe { syscall2(sys::SLEEP, ns as usize, (ns >> 32) as usize) };
}

pub fn sleep_ms(ms: u64) {
    sleep_ns(ms * 1_000_000)
}

/// Monotonic time since boot in nanoseconds.
pub fn time_ns() -> u64 {
    let (_, lo, hi) = unsafe { syscall2(sys::TIME, 0, 0) };
    ((hi as u64) << 32) | lo as u64
}

/// Terminates the calling process.
pub fn exit(code: usize) -> ! {
    unsafe {
        syscall2(sys::EXIT, code, 0);
    }
    // The kernel never returns from exit.
    loop {}
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write as _;
    let mut msg = log::FmtBuf::<{ log::MAX_STR }>::new();
    let _ = write!(msg, "{}", info.message());
    let (file, line) = info.location().map_or(("?", 0), |l| (l.file(), l.line()));
    event!(Error, "app.panic", file = file, line = line, msg = msg.as_str());
    exit(t5_abi::EXIT_PANIC)
}
