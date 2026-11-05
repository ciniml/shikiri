//! User-mode runtime for tab5-os apps.
//!
//! The kernel has already copied the image, applied relocations, zeroed .bss
//! and set `sp` before jumping to `_start`, so the entry stub only has to call
//! into Rust. Apps define their entry point with [`entry!`].

#![no_std]

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub use t5_abi::log::{self, Addr, Level};
pub use t5_abi::{Error, FOREVER, ble, bt, cfg, cfgflash, chan, display, event, font, fs, kbd, matter, net, netdev, proc, random, ready, rights, sd, shm, spawn, store, svc, sys, t5a, timer, touch, wifi, wlan};

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

    /// Reads into `buf`; returns how many bytes came (a channel: one whole
    /// message).
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, Error> {
        let (ret, v) = unsafe { syscall4(sys::READ, [self.0 as usize, buf.as_mut_ptr() as usize, buf.len(), 0]) };
        result(ret, v).map(|v| v[0])
    }

    /// `read`, and how many frames are still queued after it (devices that
    /// queue frames; 0 for the rest).
    pub fn read_queued(&self, buf: &mut [u8]) -> Result<(usize, usize), Error> {
        let (ret, v) = unsafe { syscall4(sys::READ, [self.0 as usize, buf.as_mut_ptr() as usize, buf.len(), 0]) };
        result(ret, v).map(|v| (v[0], v[1]))
    }

    /// Writes `buf` (a channel: as one message).
    pub fn write(&self, buf: &[u8]) -> Result<usize, Error> {
        let (ret, v) = unsafe { syscall4(sys::WRITE, [self.0 as usize, buf.as_ptr() as usize, buf.len(), 0]) };
        result(ret, v).map(|v| v[0])
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

/// One end of the channel `chan:NAME` (doc/16 §5).
pub struct Channel(pub Handle);

/// A channel's rights: messages, waiting, and taking granted memory (CTL).
const CHANNEL_RIGHTS: usize = rights::READ | rights::WRITE | rights::WAIT | rights::CTL;

/// Opens `prefix` + `name` with `rights`.
fn open_prefixed(prefix: &str, name: &str, rights: usize) -> Result<Handle, Error> {
    let mut full = [0u8; t5_abi::t5a::MAX_NAME];
    let n = prefix.len() + name.len();
    if n > full.len() {
        return Err(Error::Size);
    }
    full[..prefix.len()].copy_from_slice(prefix.as_bytes());
    full[prefix.len()..n].copy_from_slice(name.as_bytes());
    let full = core::str::from_utf8(&full[..n]).map_err(|_| Error::Invalid)?;
    Handle::open(full, rights)
}

impl Channel {
    /// Joins the channel `name` (without the `chan:` prefix). The image
    /// must be built with `--permit chan:NAME`.
    pub fn open(name: &str) -> Result<Channel, Error> {
        open_prefixed(chan::PREFIX, name, CHANNEL_RIGHTS).map(Channel)
    }

    /// Connects to the service `name` (`--permit svc:NAME`): a channel of
    /// its own to the server.
    pub fn connect(name: &str) -> Result<Channel, Error> {
        open_prefixed(svc::CONNECT_PREFIX, name, CHANNEL_RIGHTS).map(Channel)
    }

    /// Sends `msg`, yielding while the other end's queue is full.
    pub fn send(&self, msg: &[u8]) -> Result<(), Error> {
        loop {
            match self.0.write(msg) {
                Ok(_) => return Ok(()),
                Err(Error::Busy) => yield_now(),
                Err(e) => return Err(e),
            }
        }
    }

    /// Waits for the next message and reads it into `buf`.
    /// `Err(Error::Off)` once nothing is queued and the other end closed.
    pub fn recv(&self, buf: &mut [u8]) -> Result<usize, Error> {
        loop {
            match self.0.read(buf) {
                Err(Error::Busy) => {}
                r => return r,
            }
            let ev = wait(&[&self.0], FOREVER)?;
            if ev.kind == chan::PEER_CLOSED {
                return Err(Error::Off);
            }
        }
    }
}

/// The display (doc/17 §6): the panel's 720 x 1280 RGB565 framebuffer,
/// mapped into the process. `put`, `fill` and `rect` take coordinates as the
/// user sees the screen (after `orientation`); `flush` makes rows visible.
pub struct Display {
    pub handle: Handle,
    fb: &'static mut [u16],
    /// Framebuffer width and height (the panel's).
    pub fb_w: usize,
    pub fb_h: usize,
    /// Quarter turns (display::ORIENTATION), read at `open`.
    pub quarter: usize,
}

impl Display {
    /// Opens and maps the display. Needs `--permit display`.
    pub fn open() -> Result<Display, Error> {
        let handle = Handle::open(display::NAME, rights::CTL | rights::WAIT | rights::WRITE)?;
        let info = handle.ctl(display::INFO, 0)?;
        let (fb_w, fb_h) = (info & 0xFFFF, info >> 16);
        let quarter = handle.ctl(display::ORIENTATION, 0)? & 3;
        let addr = handle.ctl(display::MAP, 0)?;
        let fb = unsafe { core::slice::from_raw_parts_mut(addr as *mut u16, fb_w * fb_h) };
        Ok(Display { handle, fb, fb_w, fb_h, quarter })
    }

    /// Width and height as the user sees the screen.
    pub fn size(&self) -> (usize, usize) {
        if self.quarter & 1 == 1 { (self.fb_h, self.fb_w) } else { (self.fb_w, self.fb_h) }
    }

    /// The framebuffer index of user point (x, y) (inverse of the kernel's
    /// touch mapping).
    pub fn index(&self, x: usize, y: usize) -> usize {
        let (w, h) = (self.fb_w, self.fb_h);
        let (px, py) = match self.quarter {
            1 => (w - 1 - y, x),
            2 => (w - 1 - x, h - 1 - y),
            3 => (y, h - 1 - x),
            _ => (x, y),
        };
        py * w + px
    }

    /// How the framebuffer index changes for user x + 1 and for y + 1. One
    /// of them is ±1: drawing along that axis writes adjacent pixels, which
    /// is much faster in PSRAM than the other (doc/42).
    pub fn steps(&self) -> (isize, isize) {
        let w = self.fb_w as isize;
        match self.quarter {
            1 => (w, -1),
            2 => (-1, -w),
            3 => (-w, 1),
            _ => (1, w),
        }
    }

    /// The framebuffer itself (index it with `index` and `steps`).
    pub fn fb_mut(&mut self) -> &mut [u16] {
        self.fb
    }

    /// The framebuffer row that user point (x, y) lands on.
    pub fn fb_row(&self, x: usize, y: usize) -> usize {
        self.index(x, y) / self.fb_w
    }

    pub fn put(&mut self, x: usize, y: usize, color: u16) {
        let (w, h) = self.size();
        if x < w && y < h {
            let i = self.index(x, y);
            self.fb[i] = color;
        }
    }

    /// Fills a rectangle (user coordinates, clipped).
    pub fn rect(&mut self, x: usize, y: usize, rw: usize, rh: usize, color: u16) {
        let (w, h) = self.size();
        for yy in y..(y + rh).min(h) {
            for xx in x..(x + rw).min(w) {
                let i = self.index(xx, yy);
                self.fb[i] = color;
            }
        }
    }

    pub fn fill(&mut self, color: u16) {
        self.fb.fill(color);
    }

    /// Makes framebuffer rows `[y0, y1)` visible.
    pub fn flush(&self, y0: usize, y1: usize) -> Result<(), Error> {
        self.handle.ctl(display::FLUSH, (y0 | y1 << 16) as u64).map(|_| ())
    }

    pub fn flush_all(&self) -> Result<(), Error> {
        self.flush(0, self.fb_h)
    }

    /// The framebuffer rectangle (x, y, w, h) that user rectangle covers.
    pub fn fb_rect(&self, x: usize, y: usize, w: usize, h: usize) -> (usize, usize, usize, usize) {
        let (a, b) = (self.index(x, y), self.index(x + w - 1, y + h - 1));
        let (ax, ay, bx, by) = (a % self.fb_w, a / self.fb_w, b % self.fb_w, b / self.fb_w);
        (ax.min(bx), ay.min(by), ax.abs_diff(bx) + 1, ay.abs_diff(by) + 1)
    }

    /// Starts filling a user rectangle with the 2D hardware (doc/44). The
    /// caller must not touch those framebuffer rows until `wait_done`.
    pub fn start_fill(&self, x: usize, y: usize, w: usize, h: usize, color: u16) -> Result<(), Error> {
        let (fx, fy, fw, fh) = self.fb_rect(x, y, w, h);
        self.request(&[display::op::FILL, fx as u16, fy as u16, fw as u16, fh as u16, color])
    }

    /// Starts copying a user rectangle of `w` x `h` from (sx, sy) to
    /// (dx, dy) with the 2D hardware. `Invalid` if the kernel cannot (a
    /// small shift over a large overlapping area: then copy with the CPU).
    pub fn start_copy(&self, sx: usize, sy: usize, dx: usize, dy: usize, w: usize, h: usize) -> Result<(), Error> {
        let (fsx, fsy, fw, fh) = self.fb_rect(sx, sy, w, h);
        let (fdx, fdy, _, _) = self.fb_rect(dx, dy, w, h);
        self.request(&[display::op::COPY, fsx as u16, fsy as u16, fdx as u16, fdy as u16, fw as u16, fh as u16])
    }

    fn request(&self, words: &[u16]) -> Result<(), Error> {
        let mut b = [0u8; 14];
        for (i, w) in words.iter().enumerate() {
            b[2 * i..2 * i + 2].copy_from_slice(&w.to_le_bytes());
        }
        self.handle.write(&b[..2 * words.len()]).map(|_| ())
    }

    /// Waits until the 2D request started last has finished; `Fail` if it
    /// failed (the rows may be partly written).
    pub fn wait_done(&self) -> Result<(), Error> {
        loop {
            match wait(&[&self.handle], FOREVER)?.kind {
                display::DONE => return Ok(()),
                display::FAILED => return Err(Error::Fail),
                _ => {}
            }
        }
    }
}

/// The glyphs of the font service (doc/45). Needs `--permit svc:font`.
pub struct Font {
    _conn: Channel,
    _shm: Shm,
    mem: &'static [u8],
    narrow: (usize, usize, usize),
    wide: (usize, usize, usize),
}

impl Font {
    /// Connects to `svc:font` and maps the glyphs it grants (waits up to
    /// `ms` for the grant).
    pub fn connect(ms: u64) -> Result<Font, Error> {
        let conn = Channel::connect(font::SVC)?;
        loop {
            let ev = wait(&[&conn.0], ms * 1_000_000)?;
            if ev.kind == chan::SHM {
                break;
            }
            if ev.kind == chan::PEER_CLOSED {
                return Err(Error::Off);
            }
        }
        let shm = conn.take_shm()?;
        let mem: &'static [u8] = shm.map()?;
        let word = |i: usize| mem.get(4 * i..4 * i + 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
        if mem.len() < font::HEADER || mem[..4] != font::MAGIC {
            return Err(Error::Invalid);
        }
        let narrow = (word(1), word(2), word(3));
        let wide = (word(4), word(5), word(6));
        let fits = |(n, codes, bits): (usize, usize, usize), size: usize| codes + 4 * n <= mem.len() && bits + size * n <= mem.len();
        if !fits(narrow, font::NARROW_BYTES) || !fits(wide, font::WIDE_BYTES) {
            return Err(Error::Invalid);
        }
        Ok(Font { _conn: conn, _shm: shm, mem, narrow, wide })
    }

    fn find(&self, (n, codes, bits): (usize, usize, usize), size: usize, cp: char) -> Option<&[u8]> {
        let code = |i: usize| {
            let b = &self.mem[codes + 4 * i..codes + 4 * i + 4];
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        };
        let (mut lo, mut hi) = (0, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match code(mid).cmp(&(cp as u32)) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(&self.mem[bits + size * mid..bits + size * (mid + 1)]),
            }
        }
        None
    }

    /// The 16 x 32 glyph of `cp`: rows, bit 15 the leftmost pixel.
    pub fn narrow(&self, cp: char) -> Option<[u16; font::HEIGHT]> {
        let g = self.find(self.narrow, font::NARROW_BYTES, cp)?;
        Some(core::array::from_fn(|y| u16::from_le_bytes([g[2 * y], g[2 * y + 1]])))
    }

    /// The 32 x 32 glyph of `cp`: rows, bit 31 the leftmost pixel.
    pub fn wide(&self, cp: char) -> Option<[u32; font::HEIGHT]> {
        let g = self.find(self.wide, font::WIDE_BYTES, cp)?;
        Some(core::array::from_fn(|y| u32::from_le_bytes([g[4 * y], g[4 * y + 1], g[4 * y + 2], g[4 * y + 3]])))
    }
}

/// Fills `buf` from the kernel's `random` (doc/46 §4; see `random` for how
/// random it is). Needs `--permit random`.
pub fn random_fill(buf: &mut [u8]) -> Result<(), Error> {
    let h = Handle::open(random::NAME, rights::READ)?;
    for chunk in buf.chunks_mut(random::MAX) {
        h.read(chunk)?;
    }
    Ok(())
}

/// 64 random bits (`random_fill`).
pub fn random_u64() -> Result<u64, Error> {
    let mut b = [0u8; 8];
    random_fill(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// RGB565 from 8-bit components.
pub const fn rgb(r: u32, g: u32, b: u32) -> u16 {
    (((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3)) as u16
}

/// A touch point in user coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Point {
    pub x: u16,
    pub y: u16,
    pub id: u16,
    pub size: u16,
}

/// Touch input. Needs `--permit touch`.
pub struct Touch(pub Handle);

impl Touch {
    pub fn open() -> Result<Touch, Error> {
        Handle::open(touch::NAME, rights::READ | rights::WAIT).map(Touch)
    }

    /// The current points into `out`; returns how many.
    pub fn points(&self, out: &mut [Point; touch::MAX_POINTS]) -> Result<usize, Error> {
        let mut buf = [0u8; touch::MAX_POINTS * touch::POINT_SIZE];
        let n = self.0.read(&mut buf)? / touch::POINT_SIZE;
        for (o, b) in out.iter_mut().zip(buf.chunks(touch::POINT_SIZE)).take(n) {
            let u = |k: usize| u16::from_le_bytes([b[2 * k], b[2 * k + 1]]);
            *o = Point { x: u(0), y: u(1), id: u(2), size: u(3) };
        }
        Ok(n)
    }
}

/// Shared memory (doc/19 §2). Needs `--permit shm` to allocate.
pub struct Shm(pub Handle);

impl Shm {
    /// A new region of at least `bytes` (64 KiB units, zeroed).
    pub fn alloc(bytes: usize) -> Result<Shm, Error> {
        let h = Handle::open(shm::NAME, rights::CTL)?;
        h.ctl(shm::ALLOC, bytes as u64)?;
        Ok(Shm(h))
    }

    pub fn size(&self) -> Result<usize, Error> {
        self.0.ctl(shm::SIZE, 0)
    }

    /// Maps the window (one per process at a time).
    pub fn map(&self) -> Result<&'static mut [u8], Error> {
        let len = self.size()?;
        let addr = self.0.ctl(shm::MAP, 0)?;
        Ok(unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, len) })
    }

    /// Grants `[offset, offset + len)` (multiples of `shm::GRAIN`) to the
    /// other end of `to`.
    pub fn grant(&self, to: &Channel, offset: usize, len: usize) -> Result<(), Error> {
        self.0.ctl(shm::GRANT, shm::grant_arg(to.0.raw(), offset, len)).map(|_| ())
    }
}

impl Channel {
    /// On a service connection (doc/52 §3.2): the client's pid, 0 if gone.
    pub fn peer_pid(&self) -> u32 {
        self.0.ctl(chan::PEER_PID, 0).unwrap_or(0) as u32
    }

    /// The client's app name (from its signed image) into `buf`; its length.
    pub fn peer_name(&self, buf: &mut [u8; t5_abi::t5a::MAX_NAME]) -> usize {
        self.0.ctl(chan::PEER_NAME, buf.as_mut_ptr() as u64 | (buf.len() as u64) << 32).unwrap_or(0)
    }

    /// The client's grants on this service (doc/52 §3.1).
    pub fn peer_grants(&self) -> Grants {
        let mut g = Grants { buf: [0; GRANTS], len: 0 };
        // Longer than a server keeps: none (and the server denies).
        g.len = self.0.ctl(chan::PEER_GRANTS, g.buf.as_mut_ptr() as u64 | (GRANTS as u64) << 32).unwrap_or(0);
        g
    }
}

/// The bytes of grants a server keeps per client.
pub const GRANTS: usize = 256;

/// A client's grants on a service: the GRANT of each `svc:NAME/GRANT` in
/// its signed permissions (doc/52 §3.1). What a grant allows is the
/// service's to say; these are the common forms.
#[derive(Clone)]
pub struct Grants {
    buf: [u8; GRANTS],
    len: usize,
}

impl Grants {
    pub const NONE: Grants = Grants { buf: [0; GRANTS], len: 0 };

    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        self.buf[..self.len].split(|&b| b == 0).filter(|g| !g.is_empty())
    }

    /// Whether `grant` is there exactly (`scan`, `ping`).
    pub fn has(&self, grant: &[u8]) -> bool {
        self.iter().any(|g| g == grant)
    }

    /// The values of `kind:VALUE` grants (`udp:5540` → `5540`).
    pub fn values<'a>(&'a self, kind: &'a [u8]) -> impl Iterator<Item = &'a [u8]> + 'a {
        self.iter().filter_map(move |g| (g.len() > kind.len() && g.starts_with(kind) && g[kind.len()] == b':').then(|| &g[kind.len() + 1..]))
    }

    /// Whether a `kind:PREFIX` grant's PREFIX starts `s` (`r:wifi.` and
    /// `wifi.ssid`; an empty PREFIX allows all).
    pub fn prefix_of(&self, kind: &[u8], s: &[u8]) -> bool {
        self.values(kind).any(|p| s.starts_with(p)) || self.iter().any(|g| g.len() == kind.len() + 1 && g.starts_with(kind) && g[kind.len()] == b':')
    }
}

impl Channel {
    /// The shared memory the other end granted (`Error::Busy` if none).
    pub fn take_shm(&self) -> Result<Shm, Error> {
        self.0.ctl(chan::TAKE_SHM, 0).map(|h| Shm(Handle(h as u32)))
    }
}

/// A connection to the settings service (doc/50, `t5_abi::cfg`). Needs
/// `--permit svc:cfg/…` grants for the keys it uses (doc/52).
pub struct Config(pub Channel);

/// A failed request: the service's `cfg::err` code, or a channel error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CfgError {
    Code(u8),
    Ipc(Error),
}

/// A value: its flags (`cfg::SECRET`) and length (0 for a secret read over
/// `svc:cfg`).
#[derive(Clone, Copy, Debug)]
pub struct CfgValue {
    pub flags: u8,
    pub len: usize,
}

impl Config {
    pub fn connect() -> Result<Config, Error> {
        Channel::connect(cfg::SVC).map(Config)
    }

    /// Like `connect`, trying again for up to `ms` (the service may not be
    /// up yet at boot).
    pub fn connect_within(ms: u64) -> Result<Config, Error> {
        let end = time_ns() + ms * 1_000_000;
        loop {
            match Config::connect() {
                Ok(c) => return Ok(c),
                Err(e) if time_ns() >= end => return Err(e),
                Err(_) => sleep_ms(100),
            }
        }
    }

    fn call(&self, req: &[u8], answer: &mut [u8; cfg::MSG_MAX]) -> Result<usize, CfgError> {
        self.0.send(req).map_err(CfgError::Ipc)?;
        let n = self.0.recv(answer).map_err(CfgError::Ipc)?;
        match answer[..n] {
            [cfg::ERR, code] => Err(CfgError::Code(code)),
            [] => Err(CfgError::Ipc(Error::Size)),
            _ => Ok(n),
        }
    }

    /// `[op, key_len, key…, tail…]`.
    fn request<'a>(op: u8, key: &str, tail: &[u8], m: &'a mut [u8; cfg::MSG_MAX]) -> Result<&'a [u8], CfgError> {
        let (k, n) = (key.len(), 2 + key.len() + tail.len());
        if k > cfg::KEY_MAX || n > m.len() {
            return Err(CfgError::Code(cfg::err::TOO_LONG));
        }
        m[0] = op;
        m[1] = k as u8;
        m[2..2 + k].copy_from_slice(key.as_bytes());
        m[2 + k..n].copy_from_slice(tail);
        Ok(&m[..n])
    }

    /// The value of `key` into `out` (cut to its length), if there is one.
    pub fn get(&self, key: &str, out: &mut [u8]) -> Result<Option<CfgValue>, CfgError> {
        let mut m = [0u8; cfg::MSG_MAX];
        let mut a = [0u8; cfg::MSG_MAX];
        let n = self.call(Self::request(cfg::GET, key, &[], &mut m)?, &mut a)?;
        match a[0] {
            cfg::VALUE if n >= 2 => {
                let len = (n - 2).min(out.len());
                out[..len].copy_from_slice(&a[2..2 + len]);
                Ok(Some(CfgValue { flags: a[1], len }))
            }
            cfg::NONE => Ok(None),
            _ => Err(CfgError::Ipc(Error::Invalid)),
        }
    }

    /// Sets `key` (a secret if `secret`; a secret stays one).
    pub fn set(&self, key: &str, value: &[u8], secret: bool) -> Result<(), CfgError> {
        let mut a = [0u8; cfg::MSG_MAX];
        let (k, v) = (key.len(), value.len());
        if k > cfg::KEY_MAX || v > cfg::VALUE_MAX {
            return Err(CfgError::Code(cfg::err::TOO_LONG));
        }
        // `[SET, flags, key_len, key…, value…]`.
        let mut m = [0u8; cfg::MSG_MAX];
        m[0] = cfg::SET;
        m[1] = if secret { cfg::SECRET } else { 0 };
        m[2] = k as u8;
        m[3..3 + k].copy_from_slice(key.as_bytes());
        m[3 + k..3 + k + v].copy_from_slice(value);
        self.call(&m[..3 + k + v], &mut a).map(|_| ())
    }

    /// Deletes `key` (fine if there is none).
    pub fn remove(&self, key: &str) -> Result<(), CfgError> {
        let mut m = [0u8; cfg::MSG_MAX];
        let mut a = [0u8; cfg::MSG_MAX];
        self.call(Self::request(cfg::DEL, key, &[], &mut m)?, &mut a).map(|_| ())
    }

    /// Entry `index`: its key length (into `key`) and value (into
    /// `value`), or `None` past the last.
    pub fn entry(&self, index: u8, key: &mut [u8; cfg::KEY_MAX], value: &mut [u8]) -> Result<Option<(usize, CfgValue)>, CfgError> {
        let mut a = [0u8; cfg::MSG_MAX];
        let n = self.call(&[cfg::LIST, index], &mut a)?;
        match a[0] {
            cfg::ENTRY if n >= 3 && n >= 3 + a[2] as usize => {
                let k = (a[2] as usize).min(cfg::KEY_MAX);
                key[..k].copy_from_slice(&a[3..3 + k]);
                let v = &a[3 + a[2] as usize..n];
                let len = v.len().min(value.len());
                value[..len].copy_from_slice(&v[..len]);
                Ok(Some((k, CfgValue { flags: a[1], len })))
            }
            cfg::END => Ok(None),
            _ => Err(CfgError::Ipc(Error::Invalid)),
        }
    }
}

/// A connection to the file service (doc/18 §5). Needs `--permit svc:fs`.
/// Data moves through the 64 KiB slice the service grants (`slice`).
pub struct Files {
    chan: Channel,
    _shm: Shm,
    pub slice: &'static mut [u8],
}

/// A file the service opened for us.
#[derive(Clone, Copy, Debug)]
pub struct FileId {
    pub id: u32,
    pub size: u32,
}

/// A failed request: the service's `fs::status`, or a channel error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    Status(u32),
    Ipc(Error),
}

impl Files {
    pub fn connect() -> Result<Files, FsError> {
        let chan = Channel::connect(fs::SERVICE).map_err(FsError::Ipc)?;
        let ev = wait(&[&chan.0], FOREVER).map_err(FsError::Ipc)?;
        if ev.kind != chan::SHM {
            return Err(FsError::Ipc(Error::Off));
        }
        let shm = chan.take_shm().map_err(FsError::Ipc)?;
        let slice = shm.map().map_err(FsError::Ipc)?;
        Ok(Files { chan, _shm: shm, slice })
    }

    fn call(&self, op: u8, flags: u8, a: u32, b: u32, path: &str) -> Result<(u32, u32), FsError> {
        let mut m = [0u8; chan::MSG_MAX];
        let n = fs::request(op, flags, a, b, path, &mut m).ok_or(FsError::Status(fs::status::BAD_NAME))?;
        self.chan.send(&m[..n]).map_err(FsError::Ipc)?;
        let mut r = [0u8; chan::MSG_MAX];
        let k = self.chan.recv(&mut r).map_err(FsError::Ipc)?;
        if k < fs::REPLY_SIZE {
            return Err(FsError::Ipc(Error::Size));
        }
        match fs::u32_at(&r, 0) {
            fs::status::OK => Ok((fs::u32_at(&r, 4), fs::u32_at(&r, 8))),
            s => Err(FsError::Status(s)),
        }
    }

    pub fn open(&self, path: &str, flags: u8) -> Result<FileId, FsError> {
        self.call(fs::op::OPEN, flags, 0, 0, path).map(|(id, size)| FileId { id, size })
    }

    /// Reads up to `buf.len()` bytes; 0 at the end.
    pub fn read(&mut self, f: FileId, buf: &mut [u8]) -> Result<usize, FsError> {
        let mut done = 0;
        while done < buf.len() {
            let want = (buf.len() - done).min(fs::SLICE);
            let (n, _) = self.call(fs::op::READ, 0, f.id, want as u32, "")?;
            let n = n as usize;
            buf[done..done + n].copy_from_slice(&self.slice[..n]);
            done += n;
            if n < want {
                break;
            }
        }
        Ok(done)
    }

    pub fn write(&mut self, f: FileId, data: &[u8]) -> Result<(), FsError> {
        for chunk in data.chunks(fs::SLICE) {
            self.slice[..chunk.len()].copy_from_slice(chunk);
            self.call(fs::op::WRITE, 0, f.id, chunk.len() as u32, "")?;
        }
        Ok(())
    }

    pub fn seek(&self, f: FileId, offset: u32) -> Result<(), FsError> {
        self.call(fs::op::SEEK, 0, f.id, offset, "").map(|_| ())
    }

    pub fn close(&self, f: FileId) -> Result<(), FsError> {
        self.call(fs::op::CLOSE, 0, f.id, 0, "").map(|_| ())
    }

    /// Calls `each(name, size, is_dir)` for the entries of directory `path`
    /// ("" = root); returns how many there are.
    pub fn list(&self, path: &str, mut each: impl FnMut(&str, u32, bool)) -> Result<u32, FsError> {
        let (n, total) = self.call(fs::op::LIST, 0, 0, 0, path)?;
        for k in 0..n as usize {
            let rec = &self.slice[k * fs::ENTRY_SIZE..(k + 1) * fs::ENTRY_SIZE];
            let name_len = rec[..fs::ENTRY_NAME].iter().position(|&b| b == 0).unwrap_or(fs::ENTRY_NAME);
            let name = core::str::from_utf8(&rec[..name_len]).unwrap_or("?");
            each(name, fs::u32_at(rec, fs::ENTRY_NAME), rec[fs::ENTRY_NAME + 4] & fs::ATTR_DIR != 0);
        }
        Ok(total)
    }

    pub fn delete(&self, path: &str) -> Result<(), FsError> {
        self.call(fs::op::DELETE, 0, 0, 0, path).map(|_| ())
    }

    pub fn mkdir(&self, path: &str) -> Result<(), FsError> {
        self.call(fs::op::MKDIR, 0, 0, 0, path).map(|_| ())
    }
}

/// A connection to the network service (N1). Needs `--permit svc:net`.
/// Socket data moves through the granted slice; calls never block in the
/// service, and the blocking helpers here retry after 1 ms.
pub struct Net {
    chan: Channel,
    _shm: Shm,
    pub slice: &'static mut [u8],
    /// A UDP_WAIT is out: its reply comes before the next one.
    armed: core::cell::Cell<bool>,
}

/// A failed network request: the service's `net::status`, or IPC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    Status(u32),
    Ipc(Error),
    TimedOut,
}

/// What `recv_now` found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Received {
    pub len: usize,
    /// The peer closed and nothing is left.
    pub ended: bool,
}

impl Net {
    pub fn connect() -> Result<Net, NetError> {
        let chan = Channel::connect(net::SERVICE).map_err(NetError::Ipc)?;
        // The service grants the slice at once, unless all its client
        // slots are taken: then this would wait forever.
        let ev = wait(&[&chan.0], 5_000_000_000).map_err(NetError::Ipc)?;
        if ev.kind != chan::SHM {
            return Err(NetError::Ipc(Error::Off));
        }
        let shm = chan.take_shm().map_err(NetError::Ipc)?;
        let slice = shm.map().map_err(NetError::Ipc)?;
        Ok(Net { chan, _shm: shm, slice, armed: core::cell::Cell::new(false) })
    }

    fn call(&self, op: u8, sock: u16, a: u32, b: u32) -> Result<(u32, u32), NetError> {
        self.call_ip(op, sock, IpAddr::V4(Ipv4Addr::from_bits(a)), b).map(|(v0, v1, _)| (v0, v1))
    }

    /// A request with an address of either family (doc/26 §4); the reply's
    /// values and, if it has one, its IPv6 address.
    fn call_ip(&self, op: u8, sock: u16, addr: IpAddr, b: u32) -> Result<(u32, u32, Option<Ipv6Addr>), NetError> {
        let sent = match addr {
            IpAddr::V4(a) => self.chan.send(&net::request(op, sock, a.to_bits(), b)),
            IpAddr::V6(a) => self.chan.send(&net::request6(op, sock, a.octets(), b)),
        };
        sent.map_err(NetError::Ipc)?;
        let mut r = [0u8; chan::MSG_MAX];
        if self.armed.replace(false) {
            // UDP_WAIT's reply (now, or from earlier).
            self.chan.recv(&mut r).map_err(NetError::Ipc)?;
        }
        let k = self.chan.recv(&mut r).map_err(NetError::Ipc)?;
        if k < net::REPLY_SIZE {
            return Err(NetError::Ipc(Error::Size));
        }
        let u = |o: usize| u32::from_le_bytes([r[o], r[o + 1], r[o + 2], r[o + 3]]);
        let v6 = (k >= net::REPLY_SIZE_V6).then(|| Ipv6Addr::from(<[u8; 16]>::try_from(&r[net::REPLY_SIZE..net::REPLY_SIZE_V6]).unwrap()));
        match u(0) {
            net::status::OK => Ok((u(4), u(8), v6)),
            s => Err(NetError::Status(s)),
        }
    }

    /// Asks the service to answer once a datagram waits on one of this
    /// client's UDP sockets (UDP_WAIT, doc/49 §18); then `wait` on
    /// `handle()` (with any other handles) wakes when one does. The next
    /// call takes the answer. Once until then.
    pub fn udp_arm(&self) -> Result<(), NetError> {
        if !self.armed.get() {
            self.chan.send(&net::request(net::op::UDP_WAIT, 0, 0, 0)).map_err(NetError::Ipc)?;
            self.armed.set(true);
        }
        Ok(())
    }

    /// DHCP starts over on `link` (WLAN0): the Wi-Fi service joined
    /// another network (doc/49 §20).
    /// `link` associated again (doc/49 §20, §22); `changed`: with another
    /// network (DHCP starts over).
    pub fn link_renew(&self, link: u8, changed: bool) -> Result<(), NetError> {
        self.call(net::op::LINK_RENEW, 0, changed as u32, link as u32).map(|_| ())
    }

    /// The channel to the service, to `wait` on after `udp_arm`.
    pub fn handle(&self) -> &Handle {
        &self.chan.0
    }

    /// TCP_CONNECT to an address of either family.
    pub fn tcp_connect_ip(&self, ip: IpAddr, port: u16) -> Result<u16, NetError> {
        self.call_ip(net::op::TCP_CONNECT, 0, ip, port as u32).map(|(s, ..)| s as u16)
    }

    /// Listens on `port` at `addr` (unspecified: any, of that family's
    /// links as for `tcp_listen_at`).
    pub fn tcp_listen_ip(&self, addr: IpAddr, link: u8, port: u16) -> Result<u16, NetError> {
        self.call_ip(net::op::TCP_LISTEN, 0, addr, (link as u32) << 16 | port as u32).map(|(s, ..)| s as u16)
    }

    /// One echo request to `ip` (either family); the round trip in µs.
    pub fn ping_ip(&mut self, ip: IpAddr, ms: u64) -> Result<u32, NetError> {
        let data = b"t5 ping";
        self.slice[..data.len()].copy_from_slice(data);
        let (s, ..) = self.call_ip(net::op::PING, 0, ip, data.len() as u32)?;
        let r = self.poll_result(net::op::PING_RESULT, s as u16, ms).map(|(v, _)| v);
        let _ = self.close(s as u16);
        r
    }

    /// Looks up `name`'s IPv6 address (AAAA), waiting up to `ms`.
    pub fn resolve6(&mut self, name: &str, ms: u64) -> Result<Ipv6Addr, NetError> {
        let n = name.len().min(net::NAME_MAX);
        self.slice[..n].copy_from_slice(&name.as_bytes()[..n]);
        let (s, ..) = self.call_ip(net::op::RESOLVE, 0, IpAddr::V6(Ipv6Addr::UNSPECIFIED), n as u32)?;
        let r = self.poll_result(net::op::RESOLVE_RESULT, s as u16, ms);
        let _ = self.close(s as u16);
        r?.1.ok_or(NetError::Status(net::status::UNREACHABLE))
    }

    /// Listens on `port` on every link.
    pub fn tcp_listen(&self, port: u16) -> Result<u16, NetError> {
        self.tcp_listen_at(0, net::link::ANY, port)
    }

    /// Listens on `port` at local address `addr` (0: any) on `link`
    /// (`net::link`; ANY with an address: the link that has it).
    pub fn tcp_listen_at(&self, addr: u32, link: u8, port: u16) -> Result<u16, NetError> {
        self.call(net::op::TCP_LISTEN, 0, addr, (link as u32) << 16 | port as u32).map(|(s, _)| s as u16)
    }

    pub fn tcp_connect(&self, ip: u32, port: u16) -> Result<u16, NetError> {
        self.call(net::op::TCP_CONNECT, 0, ip, port as u32).map(|(s, _)| s as u16)
    }

    /// (`net::state`, bytes waiting).
    pub fn tcp_state(&self, s: u16) -> Result<(u32, u32), NetError> {
        self.call(net::op::TCP_STATE, s, 0, 0)
    }

    /// Waits up to `ms` until the socket is established.
    pub fn wait_established(&self, s: u16, ms: u64) -> Result<(), NetError> {
        let end = time_ns() + ms * 1_000_000;
        loop {
            match self.tcp_state(s)?.0 {
                net::state::ESTABLISHED => return Ok(()),
                net::state::CLOSED => return Err(NetError::Status(net::status::NOT_CONNECTED)),
                _ if time_ns() > end => return Err(NetError::TimedOut),
                _ => sleep_ms(1),
            }
        }
    }

    /// Queues what fits of `data` (up to the slice); returns how much.
    pub fn send(&mut self, s: u16, data: &[u8]) -> Result<usize, NetError> {
        let n = data.len().min(self.slice.len());
        self.slice[..n].copy_from_slice(&data[..n]);
        self.call(net::op::SEND, s, 0, n as u32).map(|(k, _)| k as usize)
    }

    /// Sends all of `data`, waiting for room.
    pub fn send_all(&mut self, s: u16, mut data: &[u8]) -> Result<(), NetError> {
        while !data.is_empty() {
            let k = self.send(s, data)?;
            data = &data[k..];
            if k == 0 {
                sleep_ms(1);
            }
        }
        Ok(())
    }

    /// Takes what has arrived, without waiting.
    pub fn recv_now(&mut self, s: u16, buf: &mut [u8]) -> Result<Received, NetError> {
        let max = buf.len().min(self.slice.len());
        let (n, ended) = self.call(net::op::RECV, s, 0, max as u32)?;
        buf[..n as usize].copy_from_slice(&self.slice[..n as usize]);
        Ok(Received { len: n as usize, ended: ended != 0 })
    }

    /// Waits for data; 0 means the peer closed.
    pub fn recv(&mut self, s: u16, buf: &mut [u8]) -> Result<usize, NetError> {
        loop {
            let r = self.recv_now(s, buf)?;
            if r.len > 0 || r.ended {
                return Ok(r.len);
            }
            sleep_ms(1);
        }
    }

    pub fn close(&self, s: u16) -> Result<(), NetError> {
        self.call(net::op::CLOSE, s, 0, 0).map(|_| ())
    }

    /// Binds `port` on every link.
    pub fn udp_bind(&self, port: u16) -> Result<u16, NetError> {
        self.udp_bind_at(0, net::link::ANY, port)
    }

    /// Binds `port` at local address `addr` (0: any) on `link`.
    pub fn udp_bind_at(&self, addr: u32, link: u8, port: u16) -> Result<u16, NetError> {
        self.call(net::op::UDP_BIND, 0, addr, (link as u32) << 16 | port as u32).map(|(s, _)| s as u16)
    }

    /// Sends one datagram, waiting for room in the queue.
    pub fn udp_send(&mut self, s: u16, ip: u32, port: u16, data: &[u8]) -> Result<(), NetError> {
        let n = data.len().min(0xFFFF).min(self.slice.len());
        self.slice[..n].copy_from_slice(&data[..n]);
        loop {
            let (k, _) = self.call(net::op::UDP_SEND, s, ip, (port as u32) << 16 | n as u32)?;
            if k > 0 {
                return Ok(());
            }
            sleep_ms(1);
        }
    }

    /// Sends one echo request to `ip` and waits up to `ms` for the reply;
    /// returns the round trip in µs.
    pub fn ping(&mut self, ip: u32, ms: u64) -> Result<u32, NetError> {
        let data = b"t5 ping";
        self.slice[..data.len()].copy_from_slice(data);
        let (s, _) = self.call(net::op::PING, 0, ip, data.len() as u32)?;
        let r = self.poll_result(net::op::PING_RESULT, s as u16, ms).map(|(v, _)| v);
        let _ = self.close(s as u16);
        r
    }

    /// Looks up the IPv4 address of `name` (DNS, A record), waiting up to
    /// `ms`.
    pub fn resolve(&mut self, name: &str, ms: u64) -> Result<u32, NetError> {
        let n = name.len().min(net::NAME_MAX);
        self.slice[..n].copy_from_slice(&name.as_bytes()[..n]);
        let (s, _) = self.call(net::op::RESOLVE, 0, 0, n as u32)?;
        let r = self.poll_result(net::op::RESOLVE_RESULT, s as u16, ms).map(|(v, _)| v);
        let _ = self.close(s as u16);
        r
    }

    /// Interface `k`'s record (`net::iface`) at the start of the slice:
    /// its length, or None if the interface is not up now. `NO_ADDRESS`
    /// past the last interface.
    pub fn if_info(&self, k: u32) -> Result<Option<usize>, NetError> {
        let (up, len) = self.call(net::op::IF_INFO, 0, k, 0)?;
        Ok((up != 0).then_some(len as usize))
    }

    /// Asks `op` on `s` every millisecond until it answers (1, value[,
    /// IPv6 address]).
    fn poll_result(&self, op: u8, s: u16, ms: u64) -> Result<(u32, Option<Ipv6Addr>), NetError> {
        let end = time_ns() + ms * 1_000_000;
        loop {
            let (done, v, a) = self.call_ip(op, s, IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)?;
            if done != 0 {
                return Ok((v, a));
            }
            if time_ns() > end {
                return Err(NetError::TimedOut);
            }
            sleep_ms(1);
        }
    }

    /// Sends one datagram to an address of either family, waiting for room.
    /// Joins multicast group `group` on the links UDP socket `s` is bound
    /// on (doc/49).
    pub fn udp_join(&self, s: u16, group: IpAddr) -> Result<(), NetError> {
        self.call_ip(net::op::UDP_JOIN, s, group, 0).map(|_| ())
    }

    pub fn udp_send_ip(&mut self, s: u16, ip: IpAddr, port: u16, data: &[u8]) -> Result<(), NetError> {
        let n = data.len().min(0xFFFF).min(self.slice.len());
        self.slice[..n].copy_from_slice(&data[..n]);
        loop {
            let (k, ..) = self.call_ip(net::op::UDP_SEND, s, ip, (port as u32) << 16 | n as u32)?;
            if k > 0 {
                return Ok(());
            }
            sleep_ms(1);
        }
    }

    /// Waits up to `ms` for a datagram from either family: (length, source,
    /// source port).
    pub fn udp_recv_ip(&mut self, s: u16, buf: &mut [u8], ms: u64) -> Result<(usize, IpAddr, u16), NetError> {
        let end = time_ns() + ms * 1_000_000;
        loop {
            let (n, ip, v6) = self.call_ip(net::op::UDP_RECV, s, IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)?;
            if n > 0 {
                let n = (n as usize).min(buf.len());
                buf[..n].copy_from_slice(&self.slice[net::UDP_HEADER..net::UDP_HEADER + n]);
                let port = u16::from_le_bytes([self.slice[0], self.slice[1]]);
                let from = v6.map_or(IpAddr::V4(Ipv4Addr::from_bits(ip)), IpAddr::V6);
                return Ok((n, from, port));
            }
            if time_ns() > end {
                return Err(NetError::TimedOut);
            }
            sleep_ms(1);
        }
    }

    /// Waits up to `ms` for a datagram: (length, source IPv4, source port).
    pub fn udp_recv(&mut self, s: u16, buf: &mut [u8], ms: u64) -> Result<(usize, u32, u16), NetError> {
        let end = time_ns() + ms * 1_000_000;
        loop {
            let (n, ip) = self.call(net::op::UDP_RECV, s, 0, 0)?;
            if n > 0 {
                let n = (n as usize).min(buf.len());
                buf[..n].copy_from_slice(&self.slice[net::UDP_HEADER..net::UDP_HEADER + n]);
                let port = u16::from_le_bytes([self.slice[0], self.slice[1]]);
                return Ok((n, ip, port));
            }
            if time_ns() > end {
                return Err(NetError::TimedOut);
            }
            sleep_ms(1);
        }
    }
}

/// A server's listener on the service `name` (`--permit listen:NAME`).
pub struct Listener(pub Handle);

impl Listener {
    pub fn open(name: &str) -> Result<Listener, Error> {
        open_prefixed(svc::LISTEN_PREFIX, name, rights::CTL | rights::WAIT).map(Listener)
    }

    /// The oldest waiting connection (`Error::Busy` if none).
    pub fn accept(&self) -> Result<Channel, Error> {
        self.0.ctl(svc::ACCEPT, 0).map(|h| Channel(Handle(h as u32)))
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

/// Gives up `entry` and every `entry/GRANT` of this process's permissions
/// for good (doc/56 §4); how many entries went. What is open stays open.
pub fn permit_drop(entry: &str) -> Result<usize, Error> {
    let (ret, v) = unsafe { syscall4(sys::PERMIT_DROP, [entry.as_ptr() as usize, entry.len(), 0, 0]) };
    result(ret, v).map(|v| v[0])
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
