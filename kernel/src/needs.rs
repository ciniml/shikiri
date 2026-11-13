// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! What an app needs before it starts (doc/51 §3): its image's Needs TLV
//! names services (`svc:NAME`, someone is listening) and marks
//! (`ready:NAME`, a process holds a handle on it). A start whose needs are
//! not all there waits here, for `run` from the host and `proc:spawn`
//! alike; the service loop starts it once they are, or drops it after
//! `WAIT_MS`.
//!
//! The Needs TLV is read before the image's signature is checked (the
//! loader checks it when the app starts): it only orders starts, it grants
//! nothing.

use t5_abi::{Error, event, ready, svc, t5a};

use crate::channel;
use crate::time;

/// Marks raised at once, and starts waiting at once (fewer on QEMU, which
/// has less RAM and runs few services).
#[cfg(not(feature = "platform-virt"))]
const MARKS: usize = 16;
#[cfg(feature = "platform-virt")]
const MARKS: usize = 2;
#[cfg(not(feature = "platform-virt"))]
const WAITING: usize = 8;
#[cfg(feature = "platform-virt")]
const WAITING: usize = 2;
/// How long a start waits for its needs.
pub const WAIT_MS: u64 = 10_000;

struct Mark {
    name: [u8; t5a::MAX_NAME],
    len: usize,
}

static mut MARK_TABLE: [Option<Mark>; MARKS] = [const { None }; MARKS];

fn marks() -> &'static mut [Option<Mark>; MARKS] {
    // Under the kernel lock.
    unsafe { &mut *core::ptr::addr_of_mut!(MARK_TABLE) }
}

/// A raised mark, held in a handle: it goes down when the handle is closed
/// (or the process ends).
pub struct ReadyMark {
    index: usize,
}

/// `open("ready:NAME")`: raises the mark (`Busy` if it is up already).
pub fn raise(name: &[u8], pid: u32) -> Result<ReadyMark, Error> {
    if name.is_empty() || name.len() > t5a::MAX_NAME {
        return Err(Error::Invalid);
    }
    if is_ready(name) {
        return Err(Error::Busy);
    }
    let m = marks();
    let index = m.iter().position(Option::is_none).ok_or(Error::NoMem)?;
    let mut mark = Mark { name: [0; t5a::MAX_NAME], len: name.len() };
    mark.name[..name.len()].copy_from_slice(name);
    m[index] = Some(mark);
    event!(Info, "ready.raised", pid = pid, name = core::str::from_utf8(name).unwrap_or("?"));
    Ok(ReadyMark { index })
}

impl Drop for ReadyMark {
    fn drop(&mut self) {
        if let Some(m) = marks()[self.index].take() {
            event!(Info, "ready.lowered", name = core::str::from_utf8(&m.name[..m.len]).unwrap_or("?"));
        }
    }
}

fn is_ready(name: &[u8]) -> bool {
    marks().iter().flatten().any(|m| &m.name[..m.len] == name)
}

/// The first of the NUL-separated `needs` that is not there.
pub fn missing(needs: &[u8]) -> Option<&[u8]> {
    needs.split(|&b| b == 0).filter(|n| !n.is_empty()).find(|n| {
        if let Some(s) = n.strip_prefix(svc::CONNECT_PREFIX.as_bytes()) {
            !channel::is_listening(s)
        } else if let Some(r) = n.strip_prefix(ready::PREFIX.as_bytes()) {
            !is_ready(r)
        } else {
            false
        }
    })
}

/// The Needs TLV of a store image, unverified (see the module's comment).
pub fn of_image(image: &[u8]) -> &[u8] {
    t5a::split_signed(image).ok().and_then(|s| t5a::parse_any_abi(s.message).ok()).map_or(&[], |i| i.info.needs)
}

/// A start that waits.
pub struct Waiting {
    /// The pid that asked (`proc:spawn`), or `None` for the host's `run`.
    pub by: Option<u32>,
    name: [u8; t5a::MAX_NAME],
    len: usize,
    pub arg0: u32,
    pub arg1: u32,
    /// Watched once started (doc/56 §3).
    pub restart: bool,
    since: u64,
}

impl Waiting {
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.len]).unwrap_or("")
    }
}

static mut WAIT_TABLE: [Option<Waiting>; WAITING] = [const { None }; WAITING];

fn waiting() -> &'static mut [Option<Waiting>; WAITING] {
    unsafe { &mut *core::ptr::addr_of_mut!(WAIT_TABLE) }
}

/// Holds a start until `need` (the first missing) and the rest are there.
pub fn defer(by: Option<u32>, name: &str, arg0: u32, arg1: u32, restart: bool, need: &[u8], now: u64) -> Result<(), &'static str> {
    if name.len() > t5a::MAX_NAME {
        return Err("name too long");
    }
    let w = waiting();
    let slot = w.iter_mut().find(|s| s.is_none()).ok_or("too many starts waiting")?;
    let mut e = Waiting { by, name: [0; t5a::MAX_NAME], len: name.len(), arg0, arg1, restart, since: now };
    e.name[..name.len()].copy_from_slice(name.as_bytes());
    *slot = Some(e);
    event!(Info, "proc.waiting", name = name, needs = core::str::from_utf8(need).unwrap_or("?"));
    Ok(())
}

/// The waiting starts whose needs are there now (`ready` true) or that
/// waited too long (false), taken off the list, one at a time.
pub fn next_due(now: u64, image_needs: impl Fn(&str) -> Option<&'static [u8]>) -> Option<(Waiting, Option<&'static [u8]>)> {
    let w = waiting();
    for slot in w.iter_mut() {
        let Some(e) = slot else { continue };
        let needs = image_needs(e.name()).unwrap_or(&[]);
        let miss = missing(needs);
        if miss.is_none() || now.saturating_sub(e.since) >= time::ms_to_ticks(WAIT_MS) {
            return slot.take().map(|e| (e, miss));
        }
    }
    None
}
