// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! Starting again the apps that fail (doc/56 §3). An app started through a
//! `proc:spawn` handle with `spawn::RESTART` set (init's list) is watched:
//! when it faults or ends with a code other than 0, the service loop starts
//! it again with the same arguments, after 0.5 s, then 1, 2 and 4 s; a
//! fifth failure within a minute of running gives up. A normal end (0) or
//! a `kill` ends the watch.

use t5_abi::{event, spawn, t5a};

use crate::process::Exit;
use crate::time;

const ENTRIES: usize = crate::sched::MAX_PROCS;
/// A run this long forgets the restarts before it.
const WINDOW_MS: u64 = 60_000;
const FIRST_DELAY_MS: u64 = 500;

pub struct Entry {
    name: [u8; t5a::MAX_NAME],
    len: usize,
    pub arg0: u32,
    pub arg1: u32,
    /// The pid that asked first (the parent of every run).
    pub by: u32,
    /// The process running it; `None` while a restart waits for `due` (or
    /// for its needs: `u64::MAX`).
    pid: Option<u32>,
    due: u64,
    started: u64,
    restarts: u32,
}

impl Entry {
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.len]).unwrap_or("")
    }
}

static mut TABLE: [Option<Entry>; ENTRIES] = [const { None }; ENTRIES];

fn table() -> &'static mut [Option<Entry>; ENTRIES] {
    // Under the kernel lock (the service loop).
    unsafe { &mut *core::ptr::addr_of_mut!(TABLE) }
}

/// Watches `pid`, a run of `name` asked for by `by`. A restart that was
/// held for its needs keeps its count.
pub fn started(name: &str, arg0: u32, arg1: u32, by: u32, pid: u32, now: u64) {
    let t = table();
    if let Some(e) = t.iter_mut().flatten().find(|e| e.pid.is_none() && e.due == u64::MAX && e.name() == name && e.by == by) {
        e.pid = Some(pid);
        e.started = now;
        return;
    }
    let Some(slot) = t.iter_mut().find(|e| e.is_none()) else {
        event!(Warn, "proc.supervise_full", name = name, pid = pid);
        return;
    };
    if name.len() > t5a::MAX_NAME {
        return;
    }
    let mut e = Entry { name: [0; t5a::MAX_NAME], len: name.len(), arg0, arg1, by, pid: Some(pid), due: 0, started: now, restarts: 0 };
    e.name[..name.len()].copy_from_slice(name.as_bytes());
    *slot = Some(e);
}

/// The watched process `pid` ended: plans its restart, or ends the watch.
pub fn exited(pid: u32, exit: Exit, now: u64) {
    let Some(slot) = table().iter_mut().find(|e| e.as_ref().is_some_and(|e| e.pid == Some(pid))) else { return };
    if matches!(exit, Exit::Code(0) | Exit::Killed) {
        *slot = None;
        return;
    }
    failed(slot, now);
}

/// A run failed (or could not start): the next restart, or none.
fn failed(slot: &mut Option<Entry>, now: u64) {
    let e = slot.as_mut().unwrap();
    if now.saturating_sub(e.started) >= time::ms_to_ticks(WINDOW_MS) {
        e.restarts = 0;
    }
    if e.restarts >= spawn::RESTARTS {
        event!(Error, "proc.restart_gave_up", name = e.name(), restarts = e.restarts);
        *slot = None;
        return;
    }
    let delay = FIRST_DELAY_MS << e.restarts;
    e.restarts += 1;
    e.pid = None;
    e.due = now + time::ms_to_ticks(delay);
    event!(Warn, "proc.restart_planned", name = e.name(), attempt = e.restarts, ms = delay as u32);
}

/// The next restart that is due, as (name, arg0, arg1, by); the caller
/// starts it and reports with `restarted`.
pub fn due(now: u64) -> Option<(&'static str, u32, u32, u32)> {
    table().iter().flatten().find(|e| e.pid.is_none() && e.due <= now).map(|e| (e.name(), e.arg0, e.arg1, e.by))
}

/// How the restart of `name` (from `due`) went: `Some(Some(pid))`
/// started, `Some(None)` waits for its needs, `None` failed.
pub fn restarted(name: &str, result: Option<Option<u32>>, now: u64) {
    let Some(slot) = table().iter_mut().find(|e| e.as_ref().is_some_and(|e| e.pid.is_none() && e.due <= now && e.name() == name)) else {
        return;
    };
    let e = slot.as_mut().unwrap();
    match result {
        Some(Some(pid)) => {
            e.pid = Some(pid);
            e.started = now;
            event!(Info, "proc.restarted", name = e.name(), pid = pid, attempt = e.restarts);
        }
        Some(None) => e.due = u64::MAX,
        None => failed(slot, now),
    }
}

/// A restart held for its needs timed out: the watch ends.
pub fn forget(name: &str, by: u32) {
    for slot in table().iter_mut() {
        if slot.as_ref().is_some_and(|e| e.pid.is_none() && e.due == u64::MAX && e.name() == name && e.by == by) {
            event!(Error, "proc.restart_gave_up", name = name, restarts = slot.as_ref().unwrap().restarts);
            *slot = None;
        }
    }
}
