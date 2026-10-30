//! Definitions shared between the kernel, the user runtime (`t5-rt`) and host
//! tools. See doc/07-architecture-proposal.md §4 for the syscall ABI and
//! doc/04-dynamic-loading-and-abi.md and doc/14-m3-design.md for the T5A image format.

#![no_std]

pub mod ctl;
pub mod fs;
pub mod log;
pub mod otadata;
pub mod store;
pub mod t5a;
pub mod t5k;

/// ABI version carried by every app image. The kernel accepts an image when
/// the major numbers match and the image's minor is not newer than its own.
pub const ABI_MAJOR: u16 = 0;
/// Minor 2 (M2): yield, sleep, time. Minor 3 (L1, doc/12): log_emit and
/// log_level replace dbg_write. Minor 4 (M3a, doc/14): signed T5A v1 images
/// (pre-1.0, so no compatibility is kept). Minor 5 (M3d, doc/14 §4): handles,
/// open/ctl/wait/close and the timer object. Minor 6 (M5c, doc/16 §5):
/// read/write and the channel object. Minor 7 (M6c, doc/17 §6): the
/// display and touch objects. Minor 8 (N0a, doc/19): service channels
/// (`listen:`/`svc:`). Minor 9 (N0b, doc/19 §2): shared memory. Minor 10
/// (M7c, doc/18 §3): the `sd` object.
pub const ABI_MINOR: u16 = 10;

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
    /// a0 = handle. Frees the handle.
    pub const CLOSE: usize = 0x080;
    /// a0/a1 = array of `u32` handles and its length (at most
    /// `WAIT_MAX`), a2/a3 = timeout in ns (lo/hi; `FOREVER` = none, 0 =
    /// just poll). Returns the index of the handle in the array (a1), the
    /// event (a2) and its value (a3) of the first handle that has one;
    /// `Error::TimedOut` if none came in time.
    pub const WAIT: usize = 0x082;
    /// a0/a1 = object name (UTF-8), a2 = rights (`rights`). Returns the
    /// handle in a1. Only names in the image's Permissions TLV can be
    /// opened (`Error::Denied` otherwise).
    pub const OPEN: usize = 0x100;
    /// a0 = handle, a1/a2 = buffer. Returns the bytes read in a1
    /// (channels: one whole message).
    pub const READ: usize = 0x101;
    /// a0 = handle, a1/a2 = buffer. Returns the bytes written in a1
    /// (channels: one whole message).
    pub const WRITE: usize = 0x102;
    /// a0 = handle, a1 = operation (per object), a2/a3 = arguments.
    /// Returns a value in a1.
    pub const CTL: usize = 0x103;
}

/// Handles per process.
pub const MAX_HANDLES: usize = 16;
/// Handles one `wait` can watch.
pub const WAIT_MAX: usize = MAX_HANDLES;
/// `wait` timeout meaning "no timeout".
pub const FOREVER: u64 = u64::MAX;

/// Rights asked for in `open`, checked on each use of the handle.
pub mod rights {
    pub const READ: usize = 1 << 0;
    pub const WRITE: usize = 1 << 1;
    pub const CTL: usize = 1 << 2;
    pub const WAIT: usize = 1 << 3;
}

/// The `timer` object: a periodic or one-shot timer whose expiries `wait`
/// reports. Expiries that come before the app waits are added up, not
/// queued: the event's value is how many there were.
pub mod timer {
    pub const NAME: &str = "timer";
    /// ctl: a2/a3 = period in ns (lo/hi). The first expiry is one period
    /// from now.
    pub const SET_PERIODIC: usize = 1;
    /// ctl: a2/a3 = delay in ns (lo/hi).
    pub const SET_ONESHOT: usize = 2;
    /// ctl: stops the timer and drops expiries not yet waited for.
    pub const STOP: usize = 3;
    /// Shortest period or delay the kernel accepts (ns).
    pub const MIN_NS: u64 = 100_000;
    /// `wait` event: the timer expired (value = how many times).
    pub const EXPIRED: usize = 1;
}
/// Channels (doc/16 §5): `open("chan:NAME")` joins one of the two ends of
/// the channel NAME; a message `write` on one end is `read` whole on the
/// other. The image's Permissions TLV must name `chan:NAME`.
pub mod chan {
    pub const PREFIX: &str = "chan:";
    /// Longest message (`write` refuses more with `Error::Size`).
    pub const MSG_MAX: usize = 256;
    /// Messages each end holds before `write` gets `Error::Busy`.
    pub const QUEUE: usize = 4;
    /// `wait` event: messages are queued (value = how many).
    pub const READABLE: usize = 1;
    /// `wait` event: nothing is queued and the other end was closed.
    pub const PEER_CLOSED: usize = 2;
    /// `wait` event: the other end granted shared memory; take it with
    /// `ctl(TAKE_SHM)`.
    pub const SHM: usize = 3;
    /// ctl: returns a new `shm` handle for the granted memory.
    pub const TAKE_SHM: usize = 1;
}

/// Shared memory (doc/19 §2): regions of PSRAM in 64 KiB units. One
/// `open("shm")` handle holds one window; a process maps at most one at a
/// time (besides the framebuffer).
pub mod shm {
    pub const NAME: &str = "shm";
    /// ctl: a2 = bytes; gives the (empty) handle a new zeroed region.
    pub const ALLOC: usize = 1;
    /// ctl: maps the window read/write into the caller; returns its address.
    pub const MAP: usize = 2;
    /// ctl: returns the window's length in bytes.
    pub const SIZE: usize = 3;
    /// ctl: a2/a3 = channel handle | (offset / GRAIN) << 8 | (length / GRAIN)
    /// << 32: grants that part of the window to the channel's other end.
    pub const GRANT: usize = 4;
    /// Offsets and lengths of grants are multiples of this (PMP granule).
    pub const GRAIN: usize = 128;

    /// The `GRANT` argument.
    pub const fn grant_arg(chan: u32, offset: usize, len: usize) -> u64 {
        chan as u64 | ((offset / GRAIN) as u64) << 8 | ((len / GRAIN) as u64) << 32
    }
}

/// The SD card as sectors (doc/18 §3), for the file service: one opener at
/// a time. `read`/`write` move whole sectors (at most `MAX_IO` bytes) at
/// the position and advance it.
pub mod sd {
    pub const NAME: &str = "sd";
    pub const SECTOR: usize = 512;
    pub const MAX_IO: usize = 4096;
    /// ctl: returns the number of sectors.
    pub const SECTORS: usize = 1;
    /// ctl: a2 = sector number: the position of the next read or write.
    pub const SEEK: usize = 2;
}

/// Service channels (doc/19): many clients, one server. The server opens
/// `listen:NAME` (a listener); each client that opens `svc:NAME` gets a new
/// channel of its own (as `chan:`), whose other end the server takes with
/// `ctl(ACCEPT)`. Both names must be in the image's Permissions TLV.
pub mod svc {
    pub const LISTEN_PREFIX: &str = "listen:";
    pub const CONNECT_PREFIX: &str = "svc:";
    /// ctl on a listener: returns the handle of the oldest waiting
    /// connection (`Error::Busy` if none).
    pub const ACCEPT: usize = 1;
    /// `wait` event on a listener: connections are waiting (value = how many).
    pub const PENDING: usize = 1;
    /// Connections a listener holds before `open("svc:…")` gets `Busy`.
    pub const BACKLOG: usize = 4;
}

/// The display (doc/17 §6): one process at a time gets the framebuffer
/// mapped into its memory. The framebuffer is the panel's: 720 x 1280
/// RGB565, rows of 720 pixels, top row first as the panel scans it.
/// `ORIENTATION` says how the Tab5 is held; apps rotate what they draw.
pub mod display {
    pub const NAME: &str = "display";
    /// ctl: returns width | height << 16 of the framebuffer (pixels).
    pub const INFO: usize = 1;
    /// ctl: maps the framebuffer into the caller (read/write); returns its
    /// address. Stays mapped until the handle is closed.
    pub const MAP: usize = 2;
    /// ctl: a2 = first row | (end row << 16): makes those rows visible
    /// (writes them back from the cache).
    pub const FLUSH: usize = 3;
    /// ctl: returns the orientation in quarter turns clockwise (0-3): how
    /// far the panel's top is turned from the top the user sees.
    pub const ORIENTATION: usize = 4;
    /// `wait` event: frames were sent to the panel (value = how many).
    pub const FRAME: usize = 1;
}

/// Touch input (doc/17 §6). Points are in the orientation's coordinates
/// (`display::ORIENTATION`): x to the right, y down as the user sees it.
pub mod touch {
    pub const NAME: &str = "touch";
    /// Points `read` returns at most.
    pub const MAX_POINTS: usize = 5;
    /// Bytes per point: u16 x, y, id, size (little-endian).
    pub const POINT_SIZE: usize = 8;
    /// `wait` event: the touch changed (value = points now, 0 = released).
    pub const CHANGED: usize = 1;
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
    /// The image's Permissions TLV does not allow this (`open`).
    Denied = 15,
    /// `wait` saw no event before its timeout.
    TimedOut = 16,
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
            15 => Error::Denied,
            16 => Error::TimedOut,
            _ => Error::Fail,
        }
    }
}

/// Exit code a process reports when it panics.
pub const EXIT_PANIC: usize = 0xDEAD;
