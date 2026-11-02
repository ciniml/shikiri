//! `proc:list`, `proc:apps` and `proc:kill` (doc/40). A system call runs
//! while the scheduler holds the calling process, so, as for `proc:spawn`,
//! the service loop does the work: it takes the snapshots that open
//! handles wait for, and the kills that were queued.

use t5_abi::store::State as StoreState;
use t5_abi::{Error, event, proc};

use crate::{process, sched, store};

struct Snapshot<const N: usize> {
    /// Handles open on it.
    open: u32,
    /// Taken since the last open.
    fresh: bool,
    len: usize,
    bytes: [u8; N],
}

impl<const N: usize> Snapshot<N> {
    fn read(&self, out: &mut [u8], record: usize) -> Result<usize, Error> {
        if !self.fresh {
            return Err(Error::Busy);
        }
        if out.len() < record {
            return Err(Error::Size);
        }
        let len = self.len.min(out.len() / record * record);
        out[..len].copy_from_slice(&self.bytes[..len]);
        Ok(len)
    }
}

const PROC_BYTES: usize = proc::PROC_RECORD * proc::MAX_PROCS;
const APP_BYTES: usize = proc::APP_RECORD * proc::MAX_APPS;
const KILLS: usize = 8;

struct State {
    procs: Snapshot<PROC_BYTES>,
    apps: Snapshot<APP_BYTES>,
    /// (by, pid)
    kills: [(u32, u32); KILLS],
    n_kills: usize,
}

static mut S: State = State {
    procs: Snapshot { open: 0, fresh: false, len: 0, bytes: [0; PROC_BYTES] },
    apps: Snapshot { open: 0, fresh: false, len: 0, bytes: [0; APP_BYTES] },
    kills: [(0, 0); KILLS],
    n_kills: 0,
};

fn st() -> &'static mut State {
    // Under the kernel lock.
    unsafe { &mut *core::ptr::addr_of_mut!(S) }
}

/// From the service loop.
pub fn poll(s: &mut sched::Scheduler, store: &store::Store) {
    let st = st();
    for &(by, pid) in &st.kills[..st.n_kills] {
        if s.kill(pid) {
            event!(Info, "proc.killed", by = by, pid = pid);
        } else {
            event!(Warn, "proc.kill_failed", by = by, pid = pid);
        }
    }
    st.n_kills = 0;
    if st.procs.open > 0 && !st.procs.fresh {
        let mut len = 0;
        for p in s.slots.iter().flatten().take(proc::MAX_PROCS) {
            let r = &mut st.procs.bytes[len..len + proc::PROC_RECORD];
            r.fill(0);
            r[0..4].copy_from_slice(&p.pid.to_le_bytes());
            r[4] = match p.state {
                process::State::Ready => proc::STATE_READY,
                process::State::Sleeping { .. } => proc::STATE_SLEEPING,
                process::State::Waiting { .. } => proc::STATE_WAITING,
                process::State::Exited(_) => proc::STATE_EXITED,
            };
            r[5] = p.handles.count() as u8;
            let cpu_ms = crate::time::ticks_to_ns(p.cpu) / 1_000_000;
            r[8..12].copy_from_slice(&(cpu_ms as u32).to_le_bytes());
            r[12..16].copy_from_slice(&(p.region.data.len() as u32).to_le_bytes());
            let name = p.name.as_bytes();
            let n = name.len().min(16);
            r[16..16 + n].copy_from_slice(&name[..n]);
            len += proc::PROC_RECORD;
        }
        st.procs.len = len;
        st.procs.fresh = true;
    }
    if st.apps.open > 0 && !st.apps.fresh {
        let mut len = 0;
        for e in store.entries().take(proc::MAX_APPS) {
            let r = &mut st.apps.bytes[len..len + proc::APP_RECORD];
            r.fill(0);
            r[0..32].copy_from_slice(&e.name);
            r[32..36].copy_from_slice(&e.length.to_le_bytes());
            for (i, v) in e.version.iter().enumerate() {
                r[36 + 4 * i..40 + 4 * i].copy_from_slice(&v.to_le_bytes());
            }
            r[48] = match e.state {
                StoreState::Previous => proc::APP_PREVIOUS,
                StoreState::PendingVerify => proc::APP_UNCONFIRMED,
                _ => proc::APP_CURRENT,
            };
            r[49] = (e.flags & t5_abi::t5a::tlv::FLAG_AUTOSTART as u8 != 0) as u8 * proc::APP_AUTOSTART;
            len += proc::APP_RECORD;
        }
        st.apps.len = len;
        st.apps.fresh = true;
    }
}

/// A handle on `proc:list` or `proc:apps`.
pub struct ListHandle {
    apps: bool,
}

impl ListHandle {
    pub fn open(apps: bool) -> ListHandle {
        let st = st();
        if apps {
            st.apps.open += 1;
            st.apps.fresh = false;
        } else {
            st.procs.open += 1;
            st.procs.fresh = false;
        }
        ListHandle { apps }
    }

    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        let st = st();
        if self.apps { st.apps.read(out, proc::APP_RECORD) } else { st.procs.read(out, proc::PROC_RECORD) }
    }
}

impl Drop for ListHandle {
    fn drop(&mut self) {
        let st = st();
        let s = if self.apps { &mut st.apps.open } else { &mut st.procs.open };
        *s -= 1;
    }
}

/// A handle on `proc:kill`.
pub struct KillHandle {
    pub pid: u32,
}

impl KillHandle {
    pub fn write(&mut self, req: &[u8]) -> Result<usize, Error> {
        let pid = u32::from_le_bytes(req.try_into().map_err(|_| Error::Size)?);
        let st = st();
        if st.n_kills == KILLS {
            return Err(Error::Busy);
        }
        st.kills[st.n_kills] = (self.pid, pid);
        st.n_kills += 1;
        Ok(req.len())
    }
}
