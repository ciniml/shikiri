//! Per-process handle table and the objects behind it (doc/14 §4,
//! doc/04 §4.2). An app only ever holds small integers; the kernel checks
//! the handle and its rights on every use. Handles are 1..=MAX_HANDLES, so
//! 0 (an uninitialised variable) is never valid.

use t5_abi::{Error, MAX_HANDLES, chan, rights, timer};

use crate::channel;
use crate::platform::{Current, Platform};
use crate::time;

pub enum Object {
    Timer(Timer),
    Channel(channel::End),
}

impl Object {
    /// Opens the object named `name` for process `pid` (the caller has
    /// checked permissions).
    fn open(name: &[u8], pid: u32) -> Result<(Object, usize), Error> {
        match name {
            n if n == timer::NAME.as_bytes() => Ok((Object::Timer(Timer::default()), rights::CTL | rights::WAIT)),
            n if n.starts_with(chan::PREFIX.as_bytes()) => {
                let end = channel::open(&n[chan::PREFIX.len()..], pid)?;
                Ok((Object::Channel(end), rights::READ | rights::WRITE | rights::WAIT))
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
            Object::Channel(_) => Err(Error::NoSupport),
        }
    }

    /// `write`: `buf` is the caller's data, already copied in.
    pub fn write(&mut self, h: usize, buf: &[u8]) -> Result<usize, Error> {
        match &self.get(h, rights::WRITE)?.object {
            Object::Channel(e) => e.write(buf),
            Object::Timer(_) => Err(Error::NoSupport),
        }
    }

    /// `read` into `buf` (kernel memory, copied out by the caller).
    pub fn read(&mut self, h: usize, buf: &mut [u8]) -> Result<usize, Error> {
        match &self.get(h, rights::READ)?.object {
            Object::Channel(e) => e.read(buf),
            Object::Timer(_) => Err(Error::NoSupport),
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
        })
    }

    /// The earliest time one of `hs` can have an event.
    pub fn deadline(&mut self, hs: &[u32]) -> Option<u64> {
        hs.iter()
            .filter_map(|&h| match &self.get(h as usize, rights::WAIT).ok()?.object {
                Object::Timer(t) => t.next,
                // Woken by the writer's process, not by time.
                Object::Channel(_) => None,
            })
            .min()
    }

    pub fn count(&self) -> usize {
        self.slots.iter().flatten().count()
    }
}
