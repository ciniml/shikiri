// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! `kbd` (doc/39): the keyboard's events, read by the kernel every
//! service round while a process has it open. While processes hold seats
//! (doc/60), the events reach the holder only when its seat is in front.
//! Keys typed from the host (`keys`, doc/54 §7) are fed in as the reader
//! takes them, each a press and a release.

use t5_abi::{Error, event, kbd};

use crate::platform::{Current, Platform};

struct Queue {
    open: bool,
    /// The process that has it open.
    pid: u32,
    events: [[u8; kbd::EVENT_SIZE]; kbd::QUEUE],
    head: usize,
    count: usize,
    dropped: u32,
}

static mut Q: Queue = Queue { open: false, pid: 0, events: [[0; kbd::EVENT_SIZE]; kbd::QUEUE], head: 0, count: 0, dropped: 0 };

/// Characters from the host's `keys` not yet in the queue.
struct Typed {
    text: [u8; t5_abi::ctl::KEYS_MAX],
    head: usize,
    len: usize,
}

static mut TYPED: Typed = Typed { text: [0; t5_abi::ctl::KEYS_MAX], head: 0, len: 0 };

fn typed() -> &'static mut Typed {
    // Under the kernel lock.
    unsafe { &mut *core::ptr::addr_of_mut!(TYPED) }
}

/// `keys TEXT` (escapes as `ctl::keys_text` reads them) for the process
/// that reads the keyboard; the characters taken.
pub fn inject(text: &[u8]) -> Result<u32, &'static str> {
    if !q().open {
        return Err("nobody reads the keyboard");
    }
    let t = typed();
    if t.len > 0 {
        return Err("still typing");
    }
    let mut n = 0;
    for b in t5_abi::ctl::keys_text(text) {
        if t5_abi::kbd::key_of(b).is_none() {
            return Err("a character no key types");
        }
        t.text[n] = b;
        n += 1;
    }
    (t.head, t.len) = (0, n);
    event!(Info, "kbd.typed", chars = n as u32);
    Ok(n as u32)
}

fn q() -> &'static mut Queue {
    // Under the kernel lock.
    unsafe { &mut *core::ptr::addr_of_mut!(Q) }
}

/// The switch key (doc/60 §8): left Ctrl + left Alt + Tab.
const SWITCH_MODIFIER: u8 = 1 << 0 | 1 << 2;
const SWITCH_USAGE: u8 = 0x2B;

/// From the service loop: while a process has it open, or two seats are
/// held (for the switch key).
pub fn poll() {
    let q = q();
    if !q.open && crate::screen::taken() < 2 {
        return;
    }
    // Typed keys, as room frees (a press and a release each).
    let t = typed();
    while q.open && t.len > 0 && q.count + 2 <= kbd::QUEUE {
        if let Some((modifier, usage)) = kbd::key_of(t.text[t.head]) {
            push(q, [modifier, usage]);
            push(q, [0, 0]);
        }
        t.head += 1;
        t.len -= 1;
    }
    if !q.open {
        t.len = 0;
    }
    let _ = Current::kbd_poll(&mut |modifier, usage| {
        if modifier & 0x0F == SWITCH_MODIFIER && usage == SWITCH_USAGE && crate::screen::taken() >= 2 {
            crate::screen::switch("key");
            return;
        }
        // Read either way: another seat's keys are not kept for later.
        if q.open && crate::screen::input_for(q.pid) {
            push(q, [modifier, usage]);
        }
    });
}

fn push(q: &mut Queue, ev: [u8; kbd::EVENT_SIZE]) {
    if q.count == kbd::QUEUE {
        q.head = (q.head + 1) % kbd::QUEUE;
        q.count -= 1;
        q.dropped += 1;
    }
    q.events[(q.head + q.count) % kbd::QUEUE] = ev;
    q.count += 1;
}

/// `pid`'s seat left the front (doc/60 §4): every key is up for it, so that
/// no key repeats while it is behind.
pub fn released(pid: u32) {
    let q = q();
    if q.open && q.pid == pid {
        push(q, [0, 0]);
    }
}

pub struct KbdHandle;

impl KbdHandle {
    pub fn open(pid: u32) -> Result<KbdHandle, Error> {
        let q = q();
        if q.open {
            return Err(Error::Busy);
        }
        *q = Queue { open: true, pid, events: q.events, head: 0, count: 0, dropped: 0 };
        Ok(KbdHandle)
    }

    /// The oldest events, `EVENT_SIZE` bytes each; `Busy` if none.
    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        let q = q();
        if q.count == 0 {
            return Err(Error::Busy);
        }
        let mut len = 0;
        while q.count > 0 && len + kbd::EVENT_SIZE <= out.len() {
            out[len..len + kbd::EVENT_SIZE].copy_from_slice(&q.events[q.head]);
            q.head = (q.head + 1) % kbd::QUEUE;
            q.count -= 1;
            len += kbd::EVENT_SIZE;
        }
        if len == 0 { Err(Error::Size) } else { Ok(len) }
    }

    pub fn poll(&self) -> Option<(usize, usize)> {
        let q = q();
        (q.count > 0).then_some((kbd::READABLE, q.count))
    }
}

impl Drop for KbdHandle {
    fn drop(&mut self) {
        let q = q();
        if q.dropped > 0 {
            event!(Warn, "kbd.dropped", events = q.dropped);
        }
        q.open = false;
    }
}
