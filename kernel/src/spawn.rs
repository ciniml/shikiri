//! `proc:spawn` (doc/38): requests to start store apps. A system call
//! runs while the scheduler holds the calling process, so the requests are
//! queued here and the service loop, which has the scheduler and the
//! store, starts them in order.

use t5_abi::{Error, spawn};

pub struct Request {
    /// The pid that asked.
    pub by: u32,
    name: [u8; spawn::NAME_MAX],
    len: u8,
    pub arg0: u32,
    pub arg1: u32,
}

impl Request {
    pub fn name(&self) -> &str {
        // Checked when queued.
        core::str::from_utf8(&self.name[..self.len as usize]).unwrap_or("")
    }
}

const QUEUE: usize = 8;

struct Queue {
    slots: [Option<Request>; QUEUE],
    head: usize,
    count: usize,
}

static mut Q: Queue = Queue { slots: [const { None }; QUEUE], head: 0, count: 0 };

fn q() -> &'static mut Queue {
    // Under the kernel lock.
    unsafe { &mut *core::ptr::addr_of_mut!(Q) }
}

/// The oldest request.
pub fn take() -> Option<Request> {
    let q = q();
    if q.count == 0 {
        return None;
    }
    let r = q.slots[q.head].take();
    q.head = (q.head + 1) % QUEUE;
    q.count -= 1;
    r
}

/// A handle on `proc:spawn`.
pub struct SpawnHandle {
    pub pid: u32,
}

impl SpawnHandle {
    pub fn write(&mut self, req: &[u8]) -> Result<usize, Error> {
        let name = req.get(spawn::ARGS..).ok_or(Error::Size)?;
        if name.is_empty() || name.len() > spawn::NAME_MAX {
            return Err(Error::Size);
        }
        if core::str::from_utf8(name).is_err() {
            return Err(Error::Invalid);
        }
        let q = q();
        if q.count == QUEUE {
            return Err(Error::Busy);
        }
        let word = |i: usize| u32::from_le_bytes(req[i..i + 4].try_into().unwrap());
        let mut r = Request { by: self.pid, name: [0; spawn::NAME_MAX], len: name.len() as u8, arg0: word(0), arg1: word(4) };
        r.name[..name.len()].copy_from_slice(name);
        q.slots[(q.head + q.count) % QUEUE] = Some(r);
        q.count += 1;
        Ok(req.len())
    }
}
