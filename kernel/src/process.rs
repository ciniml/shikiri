//! A user-mode process and the syscall dispatcher.

use t5_abi::log::{self as wire, Addr};
use t5_abi::{Error, WAIT_MAX, event, sys};

use crate::arch::riscv32::csr::cause;
use crate::arch::riscv32::pmp::{self, Region};
use crate::arch::riscv32::trap::{TrapFrame, reg};
use crate::handle;
use crate::loader::Loaded;
use crate::log;
use crate::platform::{Current, Platform};
use crate::time;

pub struct Process {
    /// Log source id (>= 1; 0 is the kernel).
    pub pid: u32,
    pub name: &'static str,
    pub frame: TrapFrame,
    pub region: Region,
    pub state: State,
    pub handles: handle::Table,
    /// The image's Permissions TLV: the names `open` accepts.
    permissions: &'static [u8],
    /// App store extents (first, count) an XIP process runs from; they must
    /// not be overwritten while it lives (doc/15 §4).
    pub xip_extents: Option<(u16, u16)>,
    /// Set when the process runs an app version that has not been
    /// confirmed yet: when it started (ticks) and the version's first
    /// extent. Its end decides that version (doc/16 §4.2).
    pub pending: Option<(u64, u16)>,
    /// The `shm` handle whose window is mapped, if any.
    shm_mapped: Option<usize>,
    /// Time (ticks) in its slices, the kernel's work on its system calls
    /// included, and the total at the last `ps` (doc/31).
    pub cpu: u64,
    pub cpu_mark: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Ready,
    /// Waiting until the counter reaches `until` (ticks).
    Sleeping { until: u64 },
    /// In `wait` on `handles[..n]` until one has an event or the counter
    /// reaches `until` (`u64::MAX`: no timeout).
    Waiting { handles: [u32; WAIT_MAX], n: usize, until: u64 },
    Exited(Exit),
}

/// How a process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Code(usize),
    Fault { code: usize, pc: usize, tval: usize },
    /// The kernel stopped it (the scheduler's time limit ran out).
    Killed,
}

/// What the scheduler should do after a syscall.
pub enum Next {
    /// Keep running the same process.
    Resume,
    /// Pick another process (yield, sleep or exit).
    Switch,
}

impl Process {
    pub fn new(pid: u32, name: &'static str, loaded: &Loaded, arg0: usize, arg1: usize) -> Process {
        let mut frame = TrapFrame::default();
        frame.pc = loaded.entry;
        frame.regs[reg::SP] = loaded.stack_top;
        frame.regs[reg::A0] = arg0;
        frame.regs[reg::A1] = arg1;
        event!(
            Info,
            "proc.start",
            pid = pid,
            name = name,
            base = Addr(loaded.region.text.start as u32),
            data = Addr(loaded.region.data.start as u32),
            build_id = &loaded.build_id[..]
        );
        Process {
            pid,
            name,
            frame,
            region: loaded.region.clone(),
            state: State::Ready,
            handles: handle::Table::new(),
            permissions: loaded.permissions,
            xip_extents: None,
            pending: None,
            shm_mapped: None,
            cpu: 0,
            cpu_mark: 0,
        }
    }

    pub fn exit(&self) -> Option<Exit> {
        match self.state {
            State::Exited(e) => Some(e),
            _ => None,
        }
    }

    /// When the scheduler must look at a blocked process again (ticks).
    pub fn wake_at(&mut self) -> Option<u64> {
        match self.state {
            State::Sleeping { until } => Some(until),
            State::Waiting { handles, n, until } => {
                Some(self.handles.deadline(&handles[..n]).map_or(until, |d| d.min(until)))
            }
            _ => None,
        }
    }

    /// Makes a blocked process ready if what it waits for has happened by
    /// `now`, completing its syscall. Returns whether it became ready.
    pub fn try_wake(&mut self, now: u64) -> bool {
        match self.state {
            State::Sleeping { until } if until <= now => {}
            State::Waiting { handles, n, until } => match self.handles.poll(&handles[..n], now) {
                Some((i, ev, v)) => self.set_result(Ok([i, ev, v])),
                None if until <= now => self.set_result(Err(Error::TimedOut)),
                None => return false,
            },
            _ => return false,
        }
        self.state = State::Ready;
        true
    }

    /// Handles the ecall the process just made. `pc` already points past it.
    pub fn syscall(&mut self) -> Next {
        let a = &self.frame.regs;
        let (nr, a0, a1, a2, a3) = (a[reg::A7], a[reg::A0], a[reg::A1], a[reg::A2], a[reg::A3]);
        let result = match nr {
            sys::EXIT => {
                self.state = State::Exited(Exit::Code(a0));
                return Next::Switch;
            }
            sys::YIELD => {
                self.set_result(Ok([0, 0, 0]));
                return Next::Switch;
            }
            sys::SLEEP => {
                let ns = ((a1 as u64) << 32) | a0 as u64;
                let until = Current::now().saturating_add(time::ns_to_ticks(ns));
                self.set_result(Ok([0, 0, 0]));
                self.state = State::Sleeping { until };
                return Next::Switch;
            }
            sys::TIME => {
                let ns = time::ticks_to_ns(Current::now());
                Ok([ns as usize, (ns >> 32) as usize, 0])
            }
            sys::LOG_EMIT => self.log_emit(a0, a1).map(|_| [0, 0, 0]),
            sys::LOG_LEVEL => Ok([log::KEEP as usize, 0, 0]),
            sys::OPEN => self.open(a0, a1, a2).map(|h| [h, 0, 0]),
            sys::CLOSE => {
                if self.shm_mapped == Some(a0) {
                    self.shm_mapped = None;
                    self.region.shm = None;
                    pmp::set_process(&self.region);
                }
                let r = self.handles.close(a0);
                if self.region.shared.is_some() && !self.handles.has_display() {
                    self.region.shared = None;
                    pmp::set_process(&self.region);
                }
                r.map(|_| [0, 0, 0])
            }
            sys::CTL if a1 == t5_abi::svc::ACCEPT && self.handles.is_listener(a0) => {
                self.handles.accept(a0, self.pid).map(|h| [h, 0, 0])
            }
            sys::CTL if a1 == t5_abi::shm::GRANT && self.handles.is_shm(a0) => {
                self.handles.grant(a0, ((a3 as u64) << 32) | a2 as u64).map(|v| [v, 0, 0])
            }
            sys::CTL if a1 == t5_abi::chan::TAKE_SHM && self.handles.is_channel(a0) => {
                self.handles.take_shm(a0).map(|h| [h, 0, 0])
            }
            sys::CTL if a1 == t5_abi::shm::MAP && self.handles.is_shm(a0) => self.map_shm(a0).map(|v| [v, 0, 0]),
            sys::CTL => {
                let r = self.handles.ctl(a0, a1, ((a3 as u64) << 32) | a2 as u64);
                if let Ok(fb) = r
                    && a1 == t5_abi::display::MAP
                    && self.handles.is_display(a0)
                    && let Some((_, len)) = Current::display_buffer()
                {
                    // The framebuffer becomes the process's until it closes
                    // the handle (doc/17 §6).
                    self.region.shared = Some(fb..fb + len);
                    pmp::set_process(&self.region);
                }
                r.map(|v| [v, 0, 0])
            }
            sys::WAIT => match self.wait(a0, a1, ((a3 as u64) << 32) | a2 as u64) {
                Ok(Some(r)) => Ok(r),
                Ok(None) => return Next::Switch,
                Err(e) => Err(e),
            },
            sys::READ | sys::WRITE => self.io(a0, a1, a2, nr == sys::WRITE).map(|n| [n, 0, 0]),
            _ => Err(Error::NoSupport),
        };
        self.set_result(result);
        Next::Resume
    }

    /// a0 = 0 and a1-a3 = values on success, a0 = -code on failure.
    fn set_result(&mut self, result: Result<[usize; 3], Error>) {
        let r = &mut self.frame.regs;
        match result {
            Ok([v1, v2, v3]) => {
                r[reg::A0] = 0;
                r[reg::A1] = v1;
                r[reg::A2] = v2;
                r[reg::A3] = v3;
            }
            Err(e) => r[reg::A0] = (e as usize).wrapping_neg(),
        }
    }

    /// Checks that `[ptr, ptr + len)` lies entirely inside memory the process
    /// can read (`write == false`) or write. The range is checked in software
    /// before the kernel touches it (doc/07 §4.1).
    fn check_user_range(&self, ptr: usize, len: usize, write: bool) -> Result<(), Error> {
        if len == 0 {
            return Ok(());
        }
        let end = ptr.checked_add(len).ok_or(Error::BadAddress)?;
        let inside = |r: &core::ops::Range<usize>| ptr >= r.start && end <= r.end;
        if inside(&self.region.data) || (!write && inside(&self.region.text)) {
            Ok(())
        } else {
            Err(Error::BadAddress)
        }
    }

    fn open(&mut self, ptr: usize, len: usize, rights: usize) -> Result<usize, Error> {
        if len > t5_abi::t5a::MAX_NAME {
            return Err(Error::Size);
        }
        self.check_user_range(ptr, len, false)?;
        let mut name = [0u8; t5_abi::t5a::MAX_NAME];
        unsafe { core::ptr::copy_nonoverlapping(ptr as *const u8, name.as_mut_ptr(), len) };
        let name = &name[..len];
        let permitted = t5_abi::t5a::permits(self.permissions, name);
        let r = self.handles.open(name, rights, permitted, self.pid);
        if r == Err(Error::Denied) {
            event!(Warn, "proc.denied", pid = self.pid, name = core::str::from_utf8(name).unwrap_or("?"));
        }
        r
    }

    /// `ctl(shm, MAP)`: one mapped window per process (doc/19 §2).
    fn map_shm(&mut self, h: usize) -> Result<usize, Error> {
        if self.shm_mapped.is_some_and(|m| m != h) {
            return Err(Error::Busy);
        }
        let addr = self.handles.ctl(h, t5_abi::shm::MAP, 0)?;
        let len = self.handles.ctl(h, t5_abi::shm::SIZE, 0)?;
        self.region.shm = Some(addr..addr + len);
        self.shm_mapped = Some(h);
        pmp::set_process(&self.region);
        Ok(addr)
    }

    /// `read`/`write` through a kernel buffer: the user range is checked,
    /// then copied in (write) or out (read).
    fn io(&mut self, h: usize, ptr: usize, len: usize, write: bool) -> Result<usize, Error> {
        // The largest transfer of any object: SD sectors (4 KiB). Channels
        // refuse messages over `chan::MSG_MAX` themselves.
        let mut buf = [0u8; t5_abi::sd::MAX_IO];
        if write && len > buf.len() {
            return Err(Error::Size);
        }
        let n = len.min(buf.len());
        self.check_user_range(ptr, n, !write)?;
        if write {
            unsafe { core::ptr::copy_nonoverlapping(ptr as *const u8, buf.as_mut_ptr(), n) };
            self.handles.write(h, &buf[..n])
        } else {
            let got = self.handles.read(h, &mut buf[..n])?;
            unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), ptr as *mut u8, got) };
            Ok(got)
        }
    }

    /// Starts a `wait`: `Ok(Some(..))` if an event is there already,
    /// `Ok(None)` if the process now blocks (the result is set when it
    /// wakes).
    fn wait(&mut self, ptr: usize, n: usize, timeout_ns: u64) -> Result<Option<[usize; 3]>, Error> {
        if n == 0 || n > WAIT_MAX {
            return Err(Error::Size);
        }
        self.check_user_range(ptr, n * 4, false)?;
        if ptr % 4 != 0 {
            return Err(Error::BadAddress);
        }
        let mut handles = [0u32; WAIT_MAX];
        unsafe { core::ptr::copy_nonoverlapping(ptr as *const u32, handles.as_mut_ptr(), n) };
        self.handles.check_wait(&handles[..n])?;
        let now = Current::now();
        if let Some((i, ev, v)) = self.handles.poll(&handles[..n], now) {
            return Ok(Some([i, ev, v]));
        }
        if timeout_ns == 0 {
            return Err(Error::TimedOut);
        }
        let until = if timeout_ns == t5_abi::FOREVER { u64::MAX } else { now.saturating_add(time::ns_to_ticks(timeout_ns)) };
        self.state = State::Waiting { handles, n, until };
        Ok(None)
    }

    fn log_emit(&self, ptr: usize, len: usize) -> Result<(), Error> {
        if len > wire::MAX_PAYLOAD {
            return Err(Error::Size);
        }
        self.check_user_range(ptr, len, false)?;
        // Copy in before validating, so the bytes cannot change in between
        // (relevant once Core1 runs processes).
        let mut buf = [0u8; wire::MAX_PAYLOAD];
        unsafe { core::ptr::copy_nonoverlapping(ptr as *const u8, buf.as_mut_ptr(), len) };
        if log::record(self.pid, &buf[..len]) {
            Ok(())
        } else {
            event!(Warn, "log.rejected", pid = self.pid, len = len);
            Err(Error::Invalid)
        }
    }
}

pub fn describe(exit: &Exit) -> (&'static str, usize) {
    match exit {
        Exit::Code(c) => ("exit", *c),
        Exit::Fault { code, .. } => (cause::name(*code), *code),
        Exit::Killed => ("killed", 0),
    }
}
