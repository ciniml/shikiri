//! User-mode runtime for tab5-os apps.
//!
//! The kernel has already copied the image, applied relocations, zeroed .bss
//! and set `sp` before jumping to `_start`, so the entry stub only has to call
//! into Rust. Apps define their entry point with [`entry!`].

#![no_std]

pub use t5_abi::log::{self, Addr, Level};
pub use t5_abi::{Error, FOREVER, event, rights, sys, timer};

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

/// Makes syscall `nr` with up to four arguments. Returns a0 (0 or -error)
/// and the values in a1-a3.
#[inline(always)]
unsafe fn syscall4(nr: usize, a: [usize; 4]) -> (isize, [usize; 3]) {
    let ret: isize;
    let v: [usize; 3];
    unsafe {
        let (v1, v2, v3);
        core::arch::asm!(
            "ecall",
            in("a7") nr,
            inlateout("a0") a[0] => ret,
            inlateout("a1") a[1] => v1,
            inlateout("a2") a[2] => v2,
            inlateout("a3") a[3] => v3,
            lateout("a4") _, lateout("a5") _,
            lateout("a6") _, lateout("t0") _, lateout("t1") _, lateout("t2") _,
            lateout("t3") _, lateout("t4") _, lateout("t5") _, lateout("t6") _,
            lateout("ra") _,
            options(nostack),
        );
        v = [v1, v2, v3];
    }
    (ret, v)
}

#[inline(always)]
unsafe fn syscall2(nr: usize, a0: usize, a1: usize) -> (isize, usize, usize) {
    let (ret, v) = unsafe { syscall4(nr, [a0, a1, 0, 0]) };
    (ret, v[0], v[1])
}

fn result(ret: isize, v: [usize; 3]) -> Result<[usize; 3], Error> {
    if ret < 0 { Err(Error::from_code(ret.unsigned_abs())) } else { Ok(v) }
}

/// Raw syscall access for tests and low-level code: returns a0 (0 or
/// -error) and the values in a1/a2.
///
/// # Safety
/// Pointer arguments must follow the syscall's contract.
pub unsafe fn raw_syscall(nr: usize, a0: usize, a1: usize) -> (isize, usize, usize) {
    unsafe { syscall2(nr, a0, a1) }
}

/// A kernel object the process opened (doc/14 §4). Closed when dropped.
#[derive(Debug)]
pub struct Handle(u32);

impl Handle {
    /// Opens the object `name` with `rights` (`rights::*`). The image's
    /// Permissions TLV must name it (elf2t5a `--permit NAME`).
    pub fn open(name: &str, rights: usize) -> Result<Handle, Error> {
        let (ret, v) = unsafe { syscall4(sys::OPEN, [name.as_ptr() as usize, name.len(), rights, 0]) };
        result(ret, v).map(|v| Handle(v[0] as u32))
    }

    pub fn raw(&self) -> u32 {
        self.0
    }

    /// Object-specific operation `op` with a 64-bit argument.
    pub fn ctl(&self, op: usize, arg: u64) -> Result<usize, Error> {
        let (ret, v) = unsafe { syscall4(sys::CTL, [self.0 as usize, op, arg as usize, (arg >> 32) as usize]) };
        result(ret, v).map(|v| v[0])
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = close_raw(self.0);
    }
}

/// The `close` syscall on a raw handle number (normally `Handle` does this).
pub fn close_raw(h: u32) -> Result<(), Error> {
    let (ret, v) = unsafe { syscall4(sys::CLOSE, [h as usize, 0, 0, 0]) };
    result(ret, v).map(|_| ())
}

/// An event `wait` returned: which handle (index into the slice passed),
/// what happened and a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub index: usize,
    pub kind: usize,
    pub value: usize,
}

/// Blocks until one of `handles` has an event, or `timeout_ns` passes
/// (`FOREVER`: no timeout; 0: just check). `Err(Error::TimedOut)` on
/// timeout.
pub fn wait_raw(handles: &[u32], timeout_ns: u64) -> Result<Event, Error> {
    let (ret, v) = unsafe {
        syscall4(sys::WAIT, [handles.as_ptr() as usize, handles.len(), timeout_ns as usize, (timeout_ns >> 32) as usize])
    };
    result(ret, v).map(|v| Event { index: v[0], kind: v[1], value: v[2] })
}

/// `wait_raw` on handles.
pub fn wait(handles: &[&Handle], timeout_ns: u64) -> Result<Event, Error> {
    let mut raw = [0u32; t5_abi::WAIT_MAX];
    let n = handles.len().min(raw.len());
    for (r, h) in raw.iter_mut().zip(handles) {
        *r = h.0;
    }
    wait_raw(&raw[..n], timeout_ns)
}

/// The `timer` object.
pub struct Timer(pub Handle);

impl Timer {
    pub fn open() -> Result<Timer, Error> {
        Handle::open(timer::NAME, rights::CTL | rights::WAIT).map(Timer)
    }

    pub fn set_periodic_ns(&self, ns: u64) -> Result<(), Error> {
        self.0.ctl(timer::SET_PERIODIC, ns).map(|_| ())
    }

    pub fn set_oneshot_ns(&self, ns: u64) -> Result<(), Error> {
        self.0.ctl(timer::SET_ONESHOT, ns).map(|_| ())
    }

    pub fn stop(&self) -> Result<(), Error> {
        self.0.ctl(timer::STOP, 0).map(|_| ())
    }
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
