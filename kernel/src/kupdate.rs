// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! Kernel updates (M5a, doc/16 §3): a T5K package received over the
//! control channel goes into the kernel slot that is not running; once its
//! signature and image check out, a new otadata entry selects it for the
//! next boot. The new kernel confirms itself when its self-test passes; if
//! it does not, the ESP-IDF bootloader starts the old one again.
//!
//! A package an app fetched over the network (doc/67 §5) names the server
//! in its otadata entry's label (`T5NC`, IPv4, port): after its self-test
//! the new kernel waits up to `update::NET_CONFIRM_MS` for an app to say
//! it reached that server (`update` `CONFIRM`), and otherwise marks itself
//! invalid and restarts, so the old kernel comes back.

use core::sync::atomic::{AtomicBool, Ordering};

use t5_abi::otadata::{self, Entry, OTADATA_OFFSET, SLOT_SIZE, SLOTS, state};
use t5_abi::{event, t5k};

use crate::flash;
use crate::loader;
use crate::platform::{Current, Platform};
use crate::time;

/// Set while the running kernel is an update that has not confirmed itself
/// yet (`pending_verify`). Such a kernel must never stop: a panic resets
/// the chip instead, so the bootloader goes back to the old kernel even
/// when a debug probe has turned the watchdog off (doc/16 §6.10).
static PENDING: AtomicBool = AtomicBool::new(false);

/// Whether the running kernel is still unconfirmed (see [`PENDING`]).
pub fn pending() -> bool {
    PENDING.load(Ordering::Relaxed)
}

/// The label of an otadata entry whose kernel waits for the network.
const NET_MAGIC: &[u8; 4] = b"T5NC";

/// A package being received.
struct Session {
    /// The server the new kernel must reach before it confirms itself.
    net: Option<(u32, u16)>,
    slot: usize,
    length: u32,
    next: u32,
    stats: flash::Stats,
    started: u64,
}

pub struct Kupdate {
    /// Address of flash offset 0.
    window: usize,
    /// The slot (0 or 1) the running kernel came from.
    running: Option<usize>,
    session: Option<Session>,
    /// This kernel waits for the network: the server, and the deadline
    /// (ticks; set after the self-test).
    net: Option<((u32, u16), Option<u64>)>,
}

fn state_name(s: u32) -> &'static str {
    match s {
        state::NEW => "new",
        state::PENDING_VERIFY => "pending_verify",
        state::VALID => "valid",
        state::INVALID => "invalid",
        state::ABORTED => "aborted",
        state::UNDEFINED => "undefined",
        _ => "?",
    }
}

impl Kupdate {
    pub fn new(window: usize) -> Kupdate {
        let running = Current::kernel_slot().and_then(|o| SLOTS.iter().position(|&s| s == o));
        let mut k = Kupdate { window, running, session: None, net: None };
        if let Some((_, e)) = k.own_entry().filter(|(_, e)| e.state == state::PENDING_VERIFY)
            && e.label[..4] == *NET_MAGIC
        {
            let ip = u32::from_le_bytes([e.label[4], e.label[5], e.label[6], e.label[7]]);
            let port = u16::from_le_bytes([e.label[8], e.label[9]]);
            k.net = Some(((ip, port), None));
        }
        if k.own_entry().is_some_and(|(_, e)| e.state == state::PENDING_VERIFY) {
            // Until the self-test confirms this kernel, a hang must end in
            // a reset as well: arm the watchdog again in case a probe
            // turned it off.
            PENDING.store(true, Ordering::Relaxed);
            Current::arm_watchdog();
            event!(Info, "kupdate.pending", watchdog = true);
        }
        k
    }

    fn bytes(&self, offset: u32, len: usize) -> &'static [u8] {
        unsafe { core::slice::from_raw_parts((self.window + offset as usize) as *const u8, len) }
    }

    fn slot_bytes(&self, slot: usize) -> &'static [u8] {
        self.bytes(SLOTS[slot], SLOT_SIZE as usize)
    }

    fn entries(&self) -> [Entry; 2] {
        let e = |i: u32| Entry::from_bytes(self.bytes(OTADATA_OFFSET + i * otadata::SECTOR_SIZE, otadata::ENTRY_SIZE));
        [e(0), e(1)]
    }

    /// The otadata entry the bootloader chose for the running kernel.
    fn own_entry(&self) -> Option<(usize, Entry)> {
        let e = self.entries();
        let i = otadata::active(&e)?;
        (Some(e[i].slot()) == self.running).then_some((i, e[i]))
    }

    /// Logs the slots and otadata (`kinfo`, and once at boot). After a
    /// rollback the newest entry is aborted or invalid and names the other
    /// slot: that is reported as `kupdate.rolled_back`.
    pub fn log_state(&self) {
        let e = self.entries();
        for (i, x) in e.iter().enumerate() {
            event!(Info, "kupdate.otadata", sector = i as u32, ota_seq = x.seq, state = state_name(x.state), crc_ok = x.crc_ok(), valid = x.is_valid());
        }
        let Some(running) = self.running else {
            event!(Info, "kupdate.no_slot");
            return;
        };
        for slot in 0..2 {
            match t5k::esp_image(self.slot_bytes(slot)) {
                Ok(i) => event!(Info, "kupdate.slot", slot = slot as u32, running = slot == running, version = i.version, secure_version = i.secure_version, len = i.len as u32),
                Err(_) => event!(Info, "kupdate.slot_empty", slot = slot as u32, running = slot == running),
            }
        }
        let newest = e.iter().filter(|x| !x.is_initial() && x.crc_ok()).max_by_key(|x| x.seq);
        if let Some(n) = newest.filter(|n| !n.is_valid() && n.slot() != running) {
            event!(Warn, "kupdate.rolled_back", ota_seq = n.seq, state = state_name(n.state), slot = n.slot() as u32);
        }
    }

    pub fn active(&self) -> bool {
        self.session.is_some()
    }

    pub fn cancel(&mut self) {
        if let Some(s) = self.session.take() {
            event!(Warn, "kupdate.dropped", received = s.next, length = s.length);
        }
    }

    /// Starts receiving a package of `length` bytes; returns the slot it
    /// goes to.
    pub fn begin(&mut self, length: u32, net: Option<(u32, u16)>) -> Result<u32, &'static str> {
        self.cancel();
        let slot = flash::spare_slot().ok_or("kernel slot unknown")?;
        if (length as usize) < t5k::HEADER_SIZE || length > SLOT_SIZE {
            return Err("bad length");
        }
        event!(Info, "kupdate.begin", length = length, slot = slot as u32);
        self.session = Some(Session { net, slot, length, next: 0, stats: flash::Stats::default(), started: Current::now() });
        Ok(slot as u32)
    }

    /// Writes the next piece, erasing each sector as the data reaches it.
    /// Any error drops the session.
    pub fn data(&mut self, offset: u32, data: &[u8]) -> Result<(), &'static str> {
        let mut s = self.session.take().ok_or("no update in progress")?;
        let end = offset as usize + data.len();
        let full = data.len() == t5_abi::ctl::INSTALL_CHUNK || end == s.length as usize;
        if offset != s.next || end > s.length as usize || !full {
            return Err("data out of order");
        }
        let at = SLOTS[s.slot] + offset;
        if at as usize % otadata::SECTOR_SIZE as usize == 0 {
            flash::erase_sector(at, &mut s.stats)?;
        }
        flash::write(at, data, &mut s.stats)?;
        s.next = end as u32;
        self.session = Some(s);
        Ok(())
    }

    /// Checks the package in its slot and selects it for the next boot.
    /// Returns the new otadata sequence number.
    pub fn end(&mut self) -> Result<u32, &'static str> {
        let s = self.session.take().ok_or("no update in progress")?;
        if s.next != s.length {
            return Err("incomplete");
        }
        let st = &s.stats;
        event!(Info, "kupdate.written", length = s.length, erases = st.erases, writes = st.writes, max_us = st.max_us as u32, ms = time::ticks_to_ns(Current::now() - s.started) / 1_000_000);
        let r = self.check_and_select(&s);
        if r.is_err() {
            // A refused image must not stay bootable: if the running kernel
            // were later marked invalid, the bootloader would fall back to
            // whatever this slot holds (doc/16 §6.11).
            let erased = flash::erase_sector(SLOTS[s.slot], &mut flash::Stats::default()).is_ok();
            event!(Warn, "kupdate.slot_erased", slot = s.slot as u32, ok = erased);
        }
        r
    }

    /// The checks of `end`; selects the package for the next boot if they pass.
    fn check_and_select(&self, s: &Session) -> Result<u32, &'static str> {
        let started = Current::now();
        let bytes = self.slot_bytes(s.slot);
        let p = t5k::split(bytes).map_err(|_| "not a kernel package")?;
        if p.total_size != s.length as usize {
            return Err("length mismatch");
        }
        // Kernels only from a system key (doc/52 §4).
        let system = loader::check_signature(&p.signed).map_err(|e| match e {
            t5_abi::t5a::Error::UnknownKey => "unknown key",
            _ => "bad signature",
        })?;
        if !system {
            return Err("app key");
        }
        let rev = Current::chip_revision();
        if p.image.check_chip(rev).is_err() {
            event!(Warn, "kupdate.wrong_chip", chip_rev = rev, chip_id = p.image.chip_id, min_rev = p.image.min_rev, max_rev = p.image.max_rev);
            return Err("wrong chip");
        }
        let running = t5k::esp_image(self.slot_bytes(1 - s.slot)).map_err(|_| "running image unreadable")?;
        if p.image.secure_version < running.secure_version {
            event!(Warn, "kupdate.downgrade", offered = p.image.secure_version, running = running.secure_version);
            return Err("secure_version rollback");
        }
        event!(Info, "kupdate.verified", slot = s.slot as u32, version = p.image.version, secure_version = p.image.secure_version, chip_rev = rev, verify_us = time::ticks_to_ns(Current::now() - started) / 1000);
        let e = self.entries();
        let (sector, seq) = otadata::next(&e, s.slot);
        let mut entry = Entry::new(seq, state::NEW);
        if let Some((ip, port)) = s.net {
            entry.label[..4].copy_from_slice(NET_MAGIC);
            entry.label[4..8].copy_from_slice(&ip.to_le_bytes());
            entry.label[8..10].copy_from_slice(&port.to_le_bytes());
        }
        self.write_entry(sector, &entry)?;
        event!(Info, "kupdate.ready", slot = s.slot as u32, ota_seq = seq, sector = sector as u32, version = p.image.version);
        Ok(seq)
    }

    /// Rewrites otadata sector `sector` with `entry` and reads it back.
    fn write_entry(&self, sector: usize, entry: &Entry) -> Result<(), &'static str> {
        let at = OTADATA_OFFSET + sector as u32 * otadata::SECTOR_SIZE;
        let mut stats = flash::Stats::default();
        flash::erase_sector(at, &mut stats)?;
        flash::write(at, &entry.to_bytes(), &mut stats)?;
        if Entry::from_bytes(self.bytes(at, otadata::ENTRY_SIZE)) != *entry {
            return Err("otadata read-back mismatch");
        }
        Ok(())
    }

    /// After the boot self-test: a kernel started as `pending_verify`
    /// confirms itself if the test passed, and otherwise marks itself
    /// invalid and restarts, so the bootloader goes back to the other slot
    /// (doc/16 §3.4).
    pub fn after_selftest(&mut self, pass: bool) {
        let Some((sector, e)) = self.own_entry() else { return };
        if e.state != state::PENDING_VERIFY {
            return;
        }
        // Installed over the network: confirmed once the network works.
        if pass && let Some((server, _)) = self.net {
            let deadline = Current::now() + time::ms_to_ticks(t5_abi::update::NET_CONFIRM_MS);
            self.net = Some((server, Some(deadline)));
            crate::update::set_awaiting(Some(server));
            event!(Info, "kupdate.awaiting_net", ip = t5_abi::log::Addr(server.0), port = server.1 as u32, seconds = (t5_abi::update::NET_CONFIRM_MS / 1000) as u32);
            return;
        }
        if pass {
            match self.write_entry(sector, &Entry { state: state::VALID, ..e }) {
                Ok(()) => {
                    PENDING.store(false, Ordering::Relaxed);
                    event!(Info, "kupdate.confirmed", ota_seq = e.seq, slot = e.slot() as u32)
                }
                Err(why) => event!(Error, "kupdate.confirm_failed", ota_seq = e.seq, error = why),
            }
            return;
        }
        let r = self.write_entry(sector, &Entry { state: state::INVALID, ..e });
        event!(Error, "kupdate.rejected", ota_seq = e.seq, slot = e.slot() as u32, written = r.is_ok());
        Current::reset();
    }

    /// `update` `CONFIRM` from process `pid` (doc/67 §5): the server was
    /// reached; a kernel waiting for that confirms itself.
    pub fn confirm_net(&mut self, pid: u32) -> Result<(), &'static str> {
        let Some((server, Some(_))) = self.net else { return Err("not waiting for the network") };
        let (sector, e) = self.own_entry().filter(|(_, e)| e.state == state::PENDING_VERIFY).ok_or("not pending")?;
        self.write_entry(sector, &Entry { state: state::VALID, ..e })?;
        self.net = None;
        crate::update::set_awaiting(None);
        PENDING.store(false, Ordering::Relaxed);
        event!(Info, "kupdate.net_reached", by = pid, ip = t5_abi::log::Addr(server.0), port = server.1 as u32);
        event!(Info, "kupdate.confirmed", ota_seq = e.seq, slot = e.slot() as u32);
        Ok(())
    }

    /// From the service loop: a kernel that waited for the network too long
    /// goes back to the old one.
    pub fn poll(&mut self, now: u64) {
        let Some((server, Some(deadline))) = self.net else { return };
        if now < deadline {
            return;
        }
        event!(Error, "kupdate.no_network", ip = t5_abi::log::Addr(server.0), port = server.1 as u32);
        if let Some((sector, e)) = self.own_entry() {
            let r = self.write_entry(sector, &Entry { state: state::INVALID, ..e });
            event!(Error, "kupdate.rejected", ota_seq = e.seq, slot = e.slot() as u32, written = r.is_ok());
        }
        Current::reset();
    }
}
