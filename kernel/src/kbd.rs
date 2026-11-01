//! `kbd` (doc/39): the keyboard's events, read by the kernel every
//! service round while a process has it open.

use t5_abi::{Error, event, kbd};

use crate::platform::{Current, Platform};

struct Queue {
    open: bool,
    events: [[u8; kbd::EVENT_SIZE]; kbd::QUEUE],
    head: usize,
    count: usize,
    dropped: u32,
}

static mut Q: Queue = Queue { open: false, events: [[0; kbd::EVENT_SIZE]; kbd::QUEUE], head: 0, count: 0, dropped: 0 };

fn q() -> &'static mut Queue {
    // Under the kernel lock.
    unsafe { &mut *core::ptr::addr_of_mut!(Q) }
}

/// From the service loop.
pub fn poll() {
    let q = q();
    if !q.open {
        return;
    }
    let _ = Current::kbd_poll(&mut |modifier, usage| {
        if q.count == kbd::QUEUE {
            q.head = (q.head + 1) % kbd::QUEUE;
            q.count -= 1;
            q.dropped += 1;
        }
        q.events[(q.head + q.count) % kbd::QUEUE] = [modifier, usage];
        q.count += 1;
    });
}

pub struct KbdHandle;

impl KbdHandle {
    pub fn open() -> Result<KbdHandle, Error> {
        let q = q();
        if q.open {
            return Err(Error::Busy);
        }
        *q = Queue { open: true, events: q.events, head: 0, count: 0, dropped: 0 };
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
