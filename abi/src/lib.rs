//! Definitions shared between the kernel, the user runtime (`t5-rt`) and host
//! tools. See doc/07-architecture-proposal.md §4 for the syscall ABI and
//! doc/04-dynamic-loading-and-abi.md and doc/14-m3-design.md for the T5A image format.

#![no_std]

pub mod log;
pub mod store;
pub mod t5a;

/// ABI version carried by every app image. The kernel accepts an image when
/// the major numbers match and the image's minor is not newer than its own.
pub const ABI_MAJOR: u16 = 0;
/// Minor 2 (M2): yield, sleep, time. Minor 3 (L1, doc/12): log_emit and
/// log_level replace dbg_write. Minor 4 (M3a, doc/14): signed T5A v1 images
/// (pre-1.0, so no compatibility is kept).
pub const ABI_MINOR: u16 = 4;

/// Syscall numbers (passed in a7). Numbers are only ever added, never reused.
pub mod sys {
    /// a0 = exit code. Does not return.
    pub const EXIT: usize = 0x000;
    /// Gives up the rest of the time slice.
    pub const YIELD: usize = 0x001;
    /// a0/a1 = nanoseconds (lo/hi). Sleeps at least that long.
    pub const SLEEP: usize = 0x002;
    pub const SYS_INFO: usize = 0x003;
    /// Returns monotonic nanoseconds since boot in a1/a2 (lo/hi).
    pub const TIME: usize = 0x004;
    /// a0/a1 = payload (see `log`), at most `log::MAX_PAYLOAD` bytes. The
    /// kernel adds the sequence number, timestamp and source.
    pub const LOG_EMIT: usize = 0x005;
    /// Returns the lowest level (a1) the kernel currently keeps, so apps can
    /// skip building events that would be dropped.
    pub const LOG_LEVEL: usize = 0x006;
}

/// Error codes. A syscall returns `-(code as isize)` in a0 on failure.
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Fail = 1,
    Busy = 2,
    Already = 3,
    Off = 4,
    Reserve = 5,
    Invalid = 6,
    Size = 7,
    Cancel = 8,
    NoMem = 9,
    NoSupport = 10,
    NoDevice = 11,
    Uninstalled = 12,
    NoAck = 13,
    /// A user pointer was outside the calling process's memory.
    BadAddress = 14,
}

impl Error {
    pub fn from_code(code: usize) -> Error {
        match code {
            2 => Error::Busy,
            3 => Error::Already,
            4 => Error::Off,
            5 => Error::Reserve,
            6 => Error::Invalid,
            7 => Error::Size,
            8 => Error::Cancel,
            9 => Error::NoMem,
            10 => Error::NoSupport,
            11 => Error::NoDevice,
            12 => Error::Uninstalled,
            13 => Error::NoAck,
            14 => Error::BadAddress,
            _ => Error::Fail,
        }
    }
}

/// Exit code a process reports when it panics.
pub const EXIT_PANIC: usize = 0xDEAD;
