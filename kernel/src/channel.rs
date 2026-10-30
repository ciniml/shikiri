//! Channels (M5c, doc/16 §5): named two-way pipes between processes. The
//! first `open("chan:NAME")` takes end A, the next end B; a message written
//! on one end is queued whole for the other. The table is fixed: there is no
//! allocator in the kernel.

use t5_abi::{Error, chan, event};

const MAX_CHANNELS: usize = 8;
const MAX_NAME: usize = t5_abi::t5a::MAX_NAME;

/// The messages queued for one end.
struct Queue {
    len: [u16; chan::QUEUE],
    data: [[u8; chan::MSG_MAX]; chan::QUEUE],
    head: usize,
    count: usize,
}

impl Queue {
    const EMPTY: Queue = Queue { len: [0; chan::QUEUE], data: [[0; chan::MSG_MAX]; chan::QUEUE], head: 0, count: 0 };
}

struct Channel {
    /// The NAME in `chan:NAME`; empty while the entry is unused.
    name: [u8; MAX_NAME],
    name_len: usize,
    /// Who holds each end (pid), if anyone.
    holder: [Option<u32>; 2],
    /// Whether the other end was held at some time since this end was
    /// opened: only then does its closing count as `PEER_CLOSED`.
    had_peer: [bool; 2],
    inbox: [Queue; 2],
}

impl Channel {
    const UNUSED: Channel =
        Channel { name: [0; MAX_NAME], name_len: 0, holder: [None; 2], had_peer: [false; 2], inbox: [Queue::EMPTY, Queue::EMPTY] };

    fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("?")
    }
}

/// Kernel-global; only touched from syscalls and handle drops, which run on
/// one core with interrupts off.
static mut CHANNELS: [Channel; MAX_CHANNELS] = [const { Channel::UNUSED }; MAX_CHANNELS];

fn channels() -> &'static mut [Channel; MAX_CHANNELS] {
    unsafe { &mut *core::ptr::addr_of_mut!(CHANNELS) }
}

/// One end of a channel, held in a handle. Dropping it closes the end.
pub struct End {
    ch: usize,
    end: usize,
}

/// Joins the channel `name` (without the `chan:` prefix) for process `pid`:
/// a free end of an existing channel, or end A of a new one.
pub fn open(name: &[u8], pid: u32) -> Result<End, Error> {
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(Error::Invalid);
    }
    let cs = channels();
    let ch = match cs.iter().position(|c| c.name_len > 0 && &c.name[..c.name_len] == name) {
        Some(i) => i,
        None => {
            let i = cs.iter().position(|c| c.name_len == 0).ok_or(Error::NoMem)?;
            cs[i].name[..name.len()].copy_from_slice(name);
            cs[i].name_len = name.len();
            i
        }
    };
    let c = &mut cs[ch];
    let end = c.holder.iter().position(Option::is_none).ok_or(Error::Busy)?;
    c.holder[end] = Some(pid);
    c.had_peer[end] = c.holder[1 - end].is_some();
    if c.had_peer[end] {
        c.had_peer[1 - end] = true;
    }
    event!(Info, "chan.open", pid = pid, name = c.name(), end = end as u32, peer = c.had_peer[end]);
    Ok(End { ch, end })
}

impl End {
    fn channel(&self) -> &'static mut Channel {
        &mut channels()[self.ch]
    }

    /// Queues `msg` for the other end.
    pub fn write(&self, msg: &[u8]) -> Result<usize, Error> {
        if msg.len() > chan::MSG_MAX {
            return Err(Error::Size);
        }
        let c = self.channel();
        if c.holder[1 - self.end].is_none() {
            return Err(Error::Off);
        }
        let q = &mut c.inbox[1 - self.end];
        if q.count == chan::QUEUE {
            return Err(Error::Busy);
        }
        let i = (q.head + q.count) % chan::QUEUE;
        q.data[i][..msg.len()].copy_from_slice(msg);
        q.len[i] = msg.len() as u16;
        q.count += 1;
        Ok(msg.len())
    }

    /// Takes the oldest message for this end into `out`. A message longer
    /// than `out` stays queued (`Error::Size`).
    pub fn read(&self, out: &mut [u8]) -> Result<usize, Error> {
        let q = &mut self.channel().inbox[self.end];
        if q.count == 0 {
            return Err(Error::Busy);
        }
        let n = q.len[q.head] as usize;
        if n > out.len() {
            return Err(Error::Size);
        }
        out[..n].copy_from_slice(&q.data[q.head][..n]);
        q.head = (q.head + 1) % chan::QUEUE;
        q.count -= 1;
        Ok(n)
    }

    /// The `wait` event this end has now, as (event, value).
    pub fn poll(&self) -> Option<(usize, usize)> {
        let c = self.channel();
        let count = c.inbox[self.end].count;
        if count > 0 {
            Some((chan::READABLE, count))
        } else if c.had_peer[self.end] && c.holder[1 - self.end].is_none() {
            Some((chan::PEER_CLOSED, 0))
        } else {
            None
        }
    }
}

impl Drop for End {
    fn drop(&mut self) {
        let c = self.channel();
        let pid = c.holder[self.end].take().unwrap_or(0);
        let q = &mut c.inbox[self.end];
        let dropped = q.count;
        *q = Queue::EMPTY;
        event!(Info, "chan.closed", pid = pid, name = c.name(), end = self.end as u32, dropped = dropped as u32);
        if c.holder.iter().all(Option::is_none) {
            *c = Channel::UNUSED;
        }
    }
}
