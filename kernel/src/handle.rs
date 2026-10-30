//! Per-process handle table and the objects behind it (doc/14 §4,
//! doc/04 §4.2). An app only ever holds small integers; the kernel checks
//! the handle and its rights on every use. Handles are 1..=MAX_HANDLES, so
//! 0 (an uninitialised variable) is never valid.

use t5_abi::{Error, MAX_HANDLES, chan, display, rights, sd, shm, svc, timer, touch};

use crate::channel;
use crate::screen::{DisplayHandle, TouchHandle};
use crate::shm::ShmHandle;
use crate::platform::{Current, Platform};
use crate::time;

/// Channels: messages, waiting, and taking granted shared memory (CTL).
const CHANNEL_RIGHTS: usize = rights::READ | rights::WRITE | rights::WAIT | rights::CTL;

pub enum Object {
    Timer(Timer),
    Channel(channel::End),
    Display(DisplayHandle),
    Touch(TouchHandle),
    Listener(channel::Listener),
    Shm(ShmHandle),
    Sd(SdHandle),
}

/// The SD card's sectors (doc/18 §3): one handle at a time.
pub struct SdHandle {
    pos: u32,
    sectors: u32,
}

static mut SD_OPEN: bool = false;

impl SdHandle {
    fn open() -> Result<SdHandle, Error> {
        if unsafe { SD_OPEN } {
            return Err(Error::Busy);
        }
        let sectors = Current::sd_sectors().map_err(|why| {
            t5_abi::event!(Warn, "sd.absent", error = why);
            Error::NoDevice
        })?;
        unsafe { SD_OPEN = true };
        Ok(SdHandle { pos: 0, sectors })
    }

    /// Checks a transfer of `len` bytes at the position; returns sectors.
    fn span(&self, len: usize) -> Result<u32, Error> {
        if len == 0 || len % sd::SECTOR != 0 || len > sd::MAX_IO {
            return Err(Error::Size);
        }
        let n = (len / sd::SECTOR) as u32;
        if self.pos as u64 + n as u64 > self.sectors as u64 {
            return Err(Error::Invalid);
        }
        Ok(n)
    }

    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        let n = self.span(buf.len())?;
        Current::sd_read(self.pos, buf).map_err(|_| Error::Fail)?;
        self.pos += n;
        Ok(buf.len())
    }

    pub fn write(&mut self, data: &[u8]) -> Result<usize, Error> {
        let n = self.span(data.len())?;
        Current::sd_write(self.pos, data).map_err(|_| Error::Fail)?;
        self.pos += n;
        Ok(data.len())
    }
}

impl Drop for SdHandle {
    fn drop(&mut self) {
        unsafe { SD_OPEN = false };
    }
}

impl Object {
    /// Opens the object named `name` for process `pid` (the caller has
    /// checked permissions).
    fn open(name: &[u8], pid: u32) -> Result<(Object, usize), Error> {
        match name {
            n if n == timer::NAME.as_bytes() => Ok((Object::Timer(Timer::default()), rights::CTL | rights::WAIT)),
            n if n == display::NAME.as_bytes() => Ok((Object::Display(DisplayHandle::open()?), rights::CTL | rights::WAIT)),
            n if n == touch::NAME.as_bytes() => Ok((Object::Touch(TouchHandle::open()?), rights::READ | rights::WAIT)),
            n if n == sd::NAME.as_bytes() => Ok((Object::Sd(SdHandle::open()?), rights::READ | rights::WRITE | rights::CTL)),
            n if n == shm::NAME.as_bytes() => {
                if !crate::shm::available() {
                    return Err(Error::NoDevice);
                }
                Ok((Object::Shm(ShmHandle { window: None }), rights::CTL))
            }
            n if n.starts_with(svc::LISTEN_PREFIX.as_bytes()) => {
                let l = channel::listen(&n[svc::LISTEN_PREFIX.len()..], pid)?;
                Ok((Object::Listener(l), rights::CTL | rights::WAIT))
            }
            n if n.starts_with(svc::CONNECT_PREFIX.as_bytes()) => {
                let end = channel::connect(&n[svc::CONNECT_PREFIX.len()..], pid)?;
                Ok((Object::Channel(end), CHANNEL_RIGHTS))
            }
            n if n.starts_with(chan::PREFIX.as_bytes()) => {
                let end = channel::open(&n[chan::PREFIX.len()..], pid)?;
                Ok((Object::Channel(end), CHANNEL_RIGHTS))
            }
            _ => Err(Error::NoDevice),
        }
    }
}

pub struct Handle {
    pub object: Object,
    pub rights: usize,
}

/// A periodic or one-shot timer, evaluated lazily against the counter: an
/// expiry is counted when someone looks at or after its deadline.
#[derive(Default)]
pub struct Timer {
    /// Ticks; 0 for one-shot.
    period: u64,
    next: Option<u64>,
    pending: u32,
}

impl Timer {
    fn update(&mut self, now: u64) {
        let Some(t) = self.next else { return };
        if now < t {
            return;
        }
        if self.period == 0 {
            self.pending = self.pending.saturating_add(1);
            self.next = None;
        } else {
            let k = (now - t) / self.period + 1;
            self.pending = self.pending.saturating_add(k.min(u32::MAX as u64) as u32);
            self.next = Some(t + k * self.period);
        }
    }

    fn ctl(&mut self, op: usize, ns: u64) -> Result<usize, Error> {
        let now = Current::now();
        match op {
            timer::SET_PERIODIC | timer::SET_ONESHOT => {
                if ns < timer::MIN_NS {
                    return Err(Error::Invalid);
                }
                let ticks = time::ns_to_ticks(ns);
                self.period = if op == timer::SET_PERIODIC { ticks } else { 0 };
                self.next = Some(now.saturating_add(ticks));
                self.pending = 0;
            }
            timer::STOP => {
                self.next = None;
                self.pending = 0;
            }
            _ => return Err(Error::NoSupport),
        }
        Ok(0)
    }
}

pub struct Table {
    slots: [Option<Handle>; MAX_HANDLES],
}

impl Table {
    pub const fn new() -> Table {
        Table { slots: [const { None }; MAX_HANDLES] }
    }

    /// Opens `name` with `want` rights for process `pid` if `permitted`;
    /// returns the handle.
    pub fn open(&mut self, name: &[u8], want: usize, permitted: bool, pid: u32) -> Result<usize, Error> {
        if !permitted {
            return Err(Error::Denied);
        }
        // Checked before the object exists: a channel end is taken on open.
        let i = self.slots.iter().position(Option::is_none).ok_or(Error::NoMem)?;
        let (object, supported) = Object::open(name, pid)?;
        if want == 0 || want & !supported != 0 {
            return Err(Error::Invalid);
        }
        self.slots[i] = Some(Handle { object, rights: want });
        Ok(i + 1)
    }

    pub fn close(&mut self, h: usize) -> Result<(), Error> {
        self.slot(h)?.take().map(|_| ()).ok_or(Error::Invalid)
    }

    fn slot(&mut self, h: usize) -> Result<&mut Option<Handle>, Error> {
        h.checked_sub(1).and_then(|i| self.slots.get_mut(i)).ok_or(Error::Invalid)
    }

    /// The handle `h` if it is open with all of `need`.
    fn get(&mut self, h: usize, need: usize) -> Result<&mut Handle, Error> {
        let handle = self.slot(h)?.as_mut().ok_or(Error::Invalid)?;
        if handle.rights & need != need {
            return Err(Error::Denied);
        }
        Ok(handle)
    }

    pub fn ctl(&mut self, h: usize, op: usize, arg: u64) -> Result<usize, Error> {
        match &mut self.get(h, rights::CTL)?.object {
            Object::Timer(t) => t.ctl(op, arg),
            Object::Display(d) => d.ctl(op, arg),
            Object::Listener(_) => Err(Error::NoSupport), // ACCEPT: `accept`
            Object::Sd(s) => match op {
                sd::SECTORS => Ok(s.sectors as usize),
                sd::SEEK if arg < s.sectors as u64 => {
                    s.pos = arg as u32;
                    Ok(0)
                }
                sd::SEEK => Err(Error::Invalid),
                _ => Err(Error::NoSupport),
            },
            Object::Shm(s) => match op {
                shm::ALLOC if s.window.is_none() => {
                    s.window = Some(crate::shm::alloc(arg as usize)?);
                    Ok(s.window.unwrap().len)
                }
                shm::ALLOC => Err(Error::Already),
                shm::SIZE => s.window.map(|w| w.len).ok_or(Error::Invalid),
                shm::MAP => s.window.map(|w| w.addr()).ok_or(Error::Invalid),
                _ => Err(Error::NoSupport), // GRANT: `grant`
            },
            Object::Channel(_) | Object::Touch(_) => Err(Error::NoSupport),
        }
    }

    /// `ctl(listener, ACCEPT)`: the next connection as a new handle in this
    /// table, for process `pid`.
    pub fn accept(&mut self, h: usize, pid: u32) -> Result<usize, Error> {
        // A free slot first: an accepted connection must not be lost.
        let free = self.slots.iter().position(Option::is_none).ok_or(Error::NoMem)?;
        let Object::Listener(l) = &mut self.get(h, rights::CTL)?.object else {
            return Err(Error::NoSupport);
        };
        let end = l.accept(pid)?;
        self.slots[free] = Some(Handle { object: Object::Channel(end), rights: CHANNEL_RIGHTS });
        Ok(free + 1)
    }

    /// `ctl(shm, GRANT, arg)`: a part of `h`'s window to the other end of
    /// the channel named in `arg`.
    pub fn grant(&mut self, h: usize, arg: u64) -> Result<usize, Error> {
        let chan_h = (arg & 0xFF) as usize;
        let (offset, len) = (((arg >> 8) & 0xFF_FFFF) as usize * shm::GRAIN, ((arg >> 32) & 0xFF_FFFF) as usize * shm::GRAIN);
        let Object::Shm(s) = &self.get(h, rights::CTL)?.object else { return Err(Error::NoSupport) };
        let w = s.window.ok_or(Error::Invalid)?.part(offset, len)?;
        match self.get(chan_h, rights::WRITE) {
            Ok(Handle { object: Object::Channel(end), .. }) => end.offer(w).map(|_| 0),
            _ => {
                w.release();
                Err(Error::Invalid)
            }
        }
    }

    /// `ctl(channel, TAKE_SHM)`: the granted memory as a new `shm` handle.
    pub fn take_shm(&mut self, h: usize) -> Result<usize, Error> {
        let free = self.slots.iter().position(Option::is_none).ok_or(Error::NoMem)?;
        let Object::Channel(end) = &self.get(h, rights::CTL)?.object else { return Err(Error::NoSupport) };
        let w = end.take().ok_or(Error::Busy)?;
        self.slots[free] = Some(Handle { object: Object::Shm(ShmHandle { window: Some(w) }), rights: rights::CTL });
        Ok(free + 1)
    }

    /// Whether `h` is a channel / a shared-memory handle.
    pub fn is_channel(&self, h: usize) -> bool {
        h.checked_sub(1)
            .and_then(|i| self.slots.get(i))
            .is_some_and(|s| matches!(s, Some(Handle { object: Object::Channel(_), .. })))
    }

    pub fn is_shm(&self, h: usize) -> bool {
        h.checked_sub(1).and_then(|i| self.slots.get(i)).is_some_and(|s| matches!(s, Some(Handle { object: Object::Shm(_), .. })))
    }

    /// Whether `h` is a listener.
    pub fn is_listener(&self, h: usize) -> bool {
        h.checked_sub(1)
            .and_then(|i| self.slots.get(i))
            .is_some_and(|s| matches!(s, Some(Handle { object: Object::Listener(_), .. })))
    }

    /// Whether `h` is the display handle.
    pub fn is_display(&self, h: usize) -> bool {
        h.checked_sub(1)
            .and_then(|i| self.slots.get(i))
            .is_some_and(|s| matches!(s, Some(Handle { object: Object::Display(_), .. })))
    }

    /// Whether any handle is the display.
    pub fn has_display(&self) -> bool {
        self.slots.iter().flatten().any(|s| matches!(s.object, Object::Display(_)))
    }

    /// `write`: `buf` is the caller's data, already copied in.
    pub fn write(&mut self, h: usize, buf: &[u8]) -> Result<usize, Error> {
        match &mut self.get(h, rights::WRITE)?.object {
            Object::Channel(e) => e.write(buf),
            Object::Sd(s) => s.write(buf),
            _ => Err(Error::NoSupport),
        }
    }

    /// `read` into `buf` (kernel memory, copied out by the caller).
    pub fn read(&mut self, h: usize, buf: &mut [u8]) -> Result<usize, Error> {
        match &mut self.get(h, rights::READ)?.object {
            Object::Channel(e) => e.read(buf),
            Object::Touch(t) => t.read(buf),
            Object::Sd(s) => s.read(buf),
            _ => Err(Error::NoSupport),
        }
    }

    /// Closes every handle (the process ended).
    pub fn close_all(&mut self) {
        self.slots = [const { None }; MAX_HANDLES];
    }

    /// Checks that every handle in `hs` can be waited on.
    pub fn check_wait(&mut self, hs: &[u32]) -> Result<(), Error> {
        hs.iter().try_for_each(|&h| self.get(h as usize, rights::WAIT).map(|_| ()))
    }

    /// The first of `hs` (all checked) with an event at `now`, as
    /// (index in `hs`, event, value); the event is consumed.
    pub fn poll(&mut self, hs: &[u32], now: u64) -> Option<(usize, usize, usize)> {
        hs.iter().enumerate().find_map(|(i, &h)| match &mut self.get(h as usize, rights::WAIT).ok()?.object {
            Object::Timer(t) => {
                t.update(now);
                (t.pending > 0).then(|| (i, timer::EXPIRED, core::mem::take(&mut t.pending) as usize))
            }
            Object::Channel(e) => e.poll().map(|(ev, v)| (i, ev, v)),
            Object::Display(d) => d.poll().map(|(ev, v)| (i, ev, v)),
            Object::Touch(t) => t.poll().map(|(ev, v)| (i, ev, v)),
            Object::Listener(l) => l.poll().map(|(ev, v)| (i, ev, v)),
            Object::Shm(_) | Object::Sd(_) => None,
        })
    }

    /// The earliest time one of `hs` can have an event.
    pub fn deadline(&mut self, hs: &[u32]) -> Option<u64> {
        hs.iter()
            .filter_map(|&h| match &self.get(h as usize, rights::WAIT).ok()?.object {
                Object::Timer(t) => t.next,
                // Woken by the writer's process, a frame or the touch
                // poll, not by time.
                _ => None,
            })
            .min()
    }

    pub fn count(&self) -> usize {
        self.slots.iter().flatten().count()
    }
}
