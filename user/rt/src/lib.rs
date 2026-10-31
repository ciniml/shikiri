//! User-mode runtime for tab5-os apps.
//!
//! The kernel has already copied the image, applied relocations, zeroed .bss
//! and set `sp` before jumping to `_start`, so the entry stub only has to call
//! into Rust. Apps define their entry point with [`entry!`].

#![no_std]

pub use t5_abi::log::{self, Addr, Level};
pub use t5_abi::{Error, FOREVER, chan, display, event, fs, net, netdev, rights, sd, shm, svc, sys, timer, touch, wlan};

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
        let handle = Handle::open(display::NAME, rights::CTL | rights::WAIT)?;
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
    fn index(&self, x: usize, y: usize) -> usize {
        let (w, h) = (self.fb_w, self.fb_h);
        let (px, py) = match self.quarter {
            1 => (w - 1 - y, x),
            2 => (w - 1 - x, h - 1 - y),
            3 => (y, h - 1 - x),
            _ => (x, y),
        };
        py * w + px
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
    /// The shared memory the other end granted (`Error::Busy` if none).
    pub fn take_shm(&self) -> Result<Shm, Error> {
        self.0.ctl(chan::TAKE_SHM, 0).map(|h| Shm(Handle(h as u32)))
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
        let ev = wait(&[&chan.0], FOREVER).map_err(NetError::Ipc)?;
        if ev.kind != chan::SHM {
            return Err(NetError::Ipc(Error::Off));
        }
        let shm = chan.take_shm().map_err(NetError::Ipc)?;
        let slice = shm.map().map_err(NetError::Ipc)?;
        Ok(Net { chan, _shm: shm, slice })
    }

    fn call(&self, op: u8, sock: u16, a: u32, b: u32) -> Result<(u32, u32), NetError> {
        self.chan.send(&net::request(op, sock, a, b)).map_err(NetError::Ipc)?;
        let mut r = [0u8; chan::MSG_MAX];
        let k = self.chan.recv(&mut r).map_err(NetError::Ipc)?;
        if k < net::REPLY_SIZE {
            return Err(NetError::Ipc(Error::Size));
        }
        let u = |o: usize| u32::from_le_bytes([r[o], r[o + 1], r[o + 2], r[o + 3]]);
        match u(0) {
            net::status::OK => Ok((u(4), u(8))),
            s => Err(NetError::Status(s)),
        }
    }

    pub fn tcp_listen(&self, port: u16) -> Result<u16, NetError> {
        self.call(net::op::TCP_LISTEN, 0, 0, port as u32).map(|(s, _)| s as u16)
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

    pub fn udp_bind(&self, port: u16) -> Result<u16, NetError> {
        self.call(net::op::UDP_BIND, 0, 0, port as u32).map(|(s, _)| s as u16)
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
        let r = self.poll_result(net::op::PING_RESULT, s as u16, ms);
        let _ = self.close(s as u16);
        r
    }

    /// Looks up the IPv4 address of `name` (DNS, A record), waiting up to
    /// `ms`.
    pub fn resolve(&mut self, name: &str, ms: u64) -> Result<u32, NetError> {
        let n = name.len().min(net::NAME_MAX);
        self.slice[..n].copy_from_slice(&name.as_bytes()[..n]);
        let (s, _) = self.call(net::op::RESOLVE, 0, 0, n as u32)?;
        let r = self.poll_result(net::op::RESOLVE_RESULT, s as u16, ms);
        let _ = self.close(s as u16);
        r
    }

    /// Asks `op` on `s` every millisecond until it answers (1, value).
    fn poll_result(&self, op: u8, s: u16, ms: u64) -> Result<u32, NetError> {
        let end = time_ns() + ms * 1_000_000;
        loop {
            let (done, v) = self.call(op, s, 0, 0)?;
            if done != 0 {
                return Ok(v);
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
