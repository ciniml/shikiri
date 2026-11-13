// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! `update` (doc/67): an app installs apps and kernel packages as the
//! host's control commands do. A request waits in the object until the
//! service loop carries it out (installs write flash, which the service
//! loop does on core 0 with the lock held); the result comes back as a
//! `DONE` event.

use t5_abi::update::{self, op, status};
use t5_abi::{Error, event};

use crate::platform::{Current, Platform};
use crate::{kupdate, sched, store, time};

struct State {
    open: bool,
    pid: u32,
    req: [u8; update::REQUEST_MAX],
    len: usize,
    pending: bool,
    done: Option<usize>,
    /// The holder went away: drop what it had begun.
    cancel: bool,
}

static mut S: State = State { open: false, pid: 0, req: [0; update::REQUEST_MAX], len: 0, pending: false, done: None, cancel: false };

fn st() -> &'static mut State {
    // Under the kernel lock.
    unsafe { &mut *core::ptr::addr_of_mut!(S) }
}

pub struct UpdateHandle;

impl UpdateHandle {
    pub fn open(pid: u32) -> Result<UpdateHandle, Error> {
        let s = st();
        if s.open {
            return Err(Error::Busy);
        }
        *s = State { open: true, pid, req: s.req, len: 0, pending: false, done: None, cancel: s.cancel };
        Ok(UpdateHandle)
    }

    pub fn write(&mut self, req: &[u8]) -> Result<usize, Error> {
        let s = st();
        if s.pending || s.done.is_some() {
            return Err(Error::Busy);
        }
        let ok = match req.first() {
            Some(&op::BEGIN_APP) => req.len() == 5,
            Some(&op::BEGIN_KERNEL) => req.len() == 5 || req.len() == 11,
            Some(&op::DATA) => req.len() > 5 && req.len() <= update::REQUEST_MAX,
            Some(&(op::END | op::REBOOT | op::CANCEL | op::CONFIRM)) => req.len() == 1,
            _ => false,
        };
        if !ok {
            return Err(Error::Invalid);
        }
        s.req[..req.len()].copy_from_slice(req);
        s.len = req.len();
        s.pending = true;
        Ok(req.len())
    }

    pub fn poll(&self) -> Option<(usize, usize)> {
        st().done.take().map(|v| (update::DONE, v))
    }

    /// The status: the server a waiting kernel wants reached.
    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        let o = out.get_mut(..update::STATUS_SIZE).ok_or(Error::Size)?;
        o.fill(0);
        if let Some((ip, port)) = unsafe { *core::ptr::addr_of!(AWAITING) } {
            o[0..4].copy_from_slice(&ip.to_le_bytes());
            o[4..6].copy_from_slice(&port.to_le_bytes());
            o[6..8].copy_from_slice(&update::AWAITING.to_le_bytes());
        }
        Ok(update::STATUS_SIZE)
    }
}

/// The server the running kernel waits for an app to reach (doc/67 §5).
static mut AWAITING: Option<(u32, u16)> = None;

/// From `kupdate`: the kernel waits for `server` to be reached (or not).
pub fn set_awaiting(server: Option<(u32, u16)>) {
    unsafe { *core::ptr::addr_of_mut!(AWAITING) = server };
}

impl Drop for UpdateHandle {
    fn drop(&mut self) {
        let s = st();
        s.open = false;
        s.pending = false;
        s.done = None;
        s.cancel = true;
    }
}

/// From the service loop: carries out the waiting request.
pub fn poll(s: &mut sched::Scheduler, store: &mut store::Store, kup: &mut kupdate::Kupdate) {
    let u = st();
    if core::mem::take(&mut u.cancel) {
        store.install_cancel();
        kup.cancel();
    }
    if !u.pending {
        return;
    }
    let req = &u.req[..u.len];
    let u32_at = |o: usize| u32::from_le_bytes([req[o], req[o + 1], req[o + 2], req[o + 3]]);
    let r: Result<(), &'static str> = match req[0] {
        op::BEGIN_APP => {
            kup.cancel();
            let (busy, n) = crate::xip_busy(s);
            store.install_begin(u32_at(1), &busy[..n]).map(|_| ())
        }
        op::BEGIN_KERNEL => {
            store.install_cancel();
            let net = (req.len() == 11).then(|| (u32_at(5), u16::from_le_bytes([req[9], req[10]])));
            kup.begin(u32_at(1), net).map(|_| ())
        }
        op::CONFIRM => kup.confirm_net(u.pid),
        op::DATA => {
            let mut offset = u32_at(1);
            let mut r = Ok(());
            for chunk in req[5..].chunks(t5_abi::ctl::INSTALL_CHUNK) {
                r = if kup.active() { kup.data(offset, chunk) } else { store.install_data(offset, chunk) };
                if r.is_err() {
                    break;
                }
                offset += chunk.len() as u32;
            }
            r
        }
        op::END if kup.active() => kup.end().map(|_| ()),
        op::END => store.install_end().map(|_| ()),
        op::CANCEL => {
            store.install_cancel();
            kup.cancel();
            Ok(())
        }
        op::REBOOT => {
            event!(Info, "update.reboot", by = u.pid);
            // As the host's `reboot`: a moment for the log to be read.
            let until = Current::now() + time::ns_to_ticks(200_000_000);
            while Current::now() < until {}
            Current::reset()
        }
        _ => Err("bad request"),
    };
    if let Err(why) = r {
        event!(Warn, "update.failed", pid = u.pid, op = req[0], error = why);
    }
    u.pending = false;
    u.done = Some(if r.is_ok() { status::OK } else { status::FAILED });
}
