//! Channels (M5c, doc/16 §5): named two-way pipes between processes. The
//! first `open("chan:NAME")` takes end A, the next end B; a message written
//! on one end is queued whole for the other. The table is fixed: there is no
//! allocator in the kernel.
//!
//! Service channels (N0a, doc/19): a server's listener (`listen:NAME`)
//! collects connections; each `open("svc:NAME")` makes a new, unnamed
//! channel whose end B is the client's and whose end A waits in the
//! listener until the server accepts it.

use t5_abi::{Error, chan, event, svc};

const MAX_CHANNELS: usize = 32;
/// Services (`listen:`): 8 were all in use on the Tab5 once `cfg` came
/// (doc/50). QEMU runs fewer services, and has less RAM.
#[cfg(not(feature = "platform-virt"))]
const MAX_LISTENERS: usize = 16;
#[cfg(feature = "platform-virt")]
const MAX_LISTENERS: usize = 8;
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
    /// A service connection: not joinable by name.
    service: bool,
    /// End A is waiting in a listener (counts as held: the client can write).
    pending_accept: bool,
    /// Shared memory granted to each end and not yet taken.
    offer: [Option<crate::shm::Window>; 2],
}

impl Channel {
    const UNUSED: Channel = Channel {
        name: [0; MAX_NAME],
        name_len: 0,
        holder: [None; 2],
        had_peer: [false; 2],
        inbox: [Queue::EMPTY, Queue::EMPTY],
        service: false,
        pending_accept: false,
        offer: [None; 2],
    };

    /// Whether end `e` is there to write to.
    fn present(&self, e: usize) -> bool {
        self.holder[e].is_some() || (e == 0 && self.pending_accept)
    }

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
    let ch = match cs.iter().position(|c| c.name_len > 0 && !c.service && &c.name[..c.name_len] == name) {
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
        if !c.present(1 - self.end) {
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

    /// Grants `w` to the other end (one pending grant at a time).
    pub fn offer(&self, w: crate::shm::Window) -> Result<(), Error> {
        let c = self.channel();
        if c.holder[1 - self.end].is_none() {
            w.release();
            return Err(Error::Off);
        }
        if c.offer[1 - self.end].is_some() {
            w.release();
            return Err(Error::Busy);
        }
        c.offer[1 - self.end] = Some(w);
        Ok(())
    }

    /// What the other end granted to this one.
    pub fn take(&self) -> Option<crate::shm::Window> {
        self.channel().offer[self.end].take()
    }

    /// The `wait` event this end has now, as (event, value).
    pub fn poll(&self) -> Option<(usize, usize)> {
        let c = self.channel();
        let count = c.inbox[self.end].count;
        if c.offer[self.end].is_some() {
            Some((chan::SHM, 1))
        } else if count > 0 {
            Some((chan::READABLE, count))
        } else if c.had_peer[self.end] && !c.present(1 - self.end) {
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
        if let Some(w) = c.offer[self.end].take() {
            w.release();
        }
        let q = &mut c.inbox[self.end];
        let dropped = q.count;
        *q = Queue::EMPTY;
        event!(Info, "chan.closed", pid = pid, name = c.name(), end = self.end as u32, dropped = dropped as u32);
        free_if_unused(c);
    }
}

fn free_if_unused(c: &mut Channel) {
    if c.holder.iter().all(Option::is_none) && !c.pending_accept {
        *c = Channel::UNUSED;
    }
}

// ---- service channels ----

struct ListenerEntry {
    name: [u8; MAX_NAME],
    name_len: usize,
    owner: u32,
    /// Channels whose end A waits for `accept`, oldest first.
    pending: [Option<usize>; svc::BACKLOG],
}

static mut LISTENERS: [Option<ListenerEntry>; MAX_LISTENERS] = [const { None }; MAX_LISTENERS];

fn listeners() -> &'static mut [Option<ListenerEntry>; MAX_LISTENERS] {
    unsafe { &mut *core::ptr::addr_of_mut!(LISTENERS) }
}

/// A server's listener on `svc:NAME`, held in a handle.
pub struct Listener {
    index: usize,
}

/// Whether a service listens on `svc:NAME` (doc/51 §3).
pub fn is_listening(name: &[u8]) -> bool {
    listeners().iter().flatten().any(|l| &l.name[..l.name_len] == name)
}

/// `open("listen:NAME")`: one listener per name.
pub fn listen(name: &[u8], pid: u32) -> Result<Listener, Error> {
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(Error::Invalid);
    }
    let ls = listeners();
    if ls.iter().flatten().any(|l| &l.name[..l.name_len] == name) {
        return Err(Error::Busy);
    }
    let index = ls.iter().position(Option::is_none).ok_or(Error::NoMem)?;
    let mut e = ListenerEntry { name: [0; MAX_NAME], name_len: name.len(), owner: pid, pending: [None; svc::BACKLOG] };
    e.name[..name.len()].copy_from_slice(name);
    ls[index] = Some(e);
    event!(Info, "svc.listen", pid = pid, name = core::str::from_utf8(name).unwrap_or("?"));
    Ok(Listener { index })
}

/// `open("svc:NAME")`: a new channel to the listener; the caller gets end B.
pub fn connect(name: &[u8], pid: u32) -> Result<End, Error> {
    let ls = listeners();
    let l = ls.iter_mut().flatten().find(|l| &l.name[..l.name_len] == name).ok_or(Error::Off)?;
    let slot = l.pending.iter().position(Option::is_none).ok_or(Error::Busy)?;
    let cs = channels();
    let ch = cs.iter().position(|c| c.name_len == 0).ok_or(Error::NoMem)?;
    let c = &mut cs[ch];
    c.name[..name.len()].copy_from_slice(name);
    c.name_len = name.len();
    c.service = true;
    c.pending_accept = true;
    c.holder[1] = Some(pid);
    c.had_peer[1] = true;
    l.pending[slot] = Some(ch);
    event!(Info, "svc.connect", pid = pid, name = c.name(), server = l.owner);
    Ok(End { ch, end: 1 })
}

impl Listener {
    fn entry(&self) -> &'static mut ListenerEntry {
        listeners()[self.index].as_mut().unwrap()
    }

    /// End A of the oldest waiting connection, now held by `pid`.
    pub fn accept(&mut self, pid: u32) -> Result<End, Error> {
        let l = self.entry();
        let ch = l.pending[0].ok_or(Error::Busy)?;
        l.pending.rotate_left(1);
        l.pending[svc::BACKLOG - 1] = None;
        let c = &mut channels()[ch];
        c.pending_accept = false;
        c.holder[0] = Some(pid);
        // A client that already left reads as PEER_CLOSED right away.
        c.had_peer[0] = true;
        Ok(End { ch, end: 0 })
    }

    /// The `wait` event: PENDING with the number of waiting connections.
    pub fn poll(&self) -> Option<(usize, usize)> {
        let n = self.entry().pending.iter().flatten().count();
        (n > 0).then_some((svc::PENDING, n))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let l = listeners()[self.index].take().unwrap();
        // Connections never accepted: their clients see PEER_CLOSED.
        for ch in l.pending.into_iter().flatten() {
            let c = &mut channels()[ch];
            c.pending_accept = false;
            free_if_unused(c);
        }
        event!(Info, "svc.closed", pid = l.owner, name = core::str::from_utf8(&l.name[..l.name_len]).unwrap_or("?"));
    }
}
