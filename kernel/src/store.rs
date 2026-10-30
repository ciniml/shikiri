//! The app store in flash (`t5_abi::store`, doc/14 §2), read through the
//! window the platform maps. Images are verified when they are loaded, and
//! before an install is committed.
//!
//! Installing (doc/14 §3, M3d): the image goes into free extents, never over
//! the app it replaces; only then is the catalog rewritten, into the sector
//! that does not hold the catalog in force. An install cut short at any point
//! leaves the old catalog, and with it the old app, in force.
//!
//! Updates (M5b, doc/16 §4): installing over a valid app keeps the old one
//! as `Previous` and puts the new one in as `PendingVerify`. Its first run
//! decides: a normal end (or 2 s of running) confirms it and drops the old
//! one; a fault brings the old one back.

use t5_abi::store::{self, Catalog, Entry, State};
use t5_abi::t5a;
use t5_abi::{event, log::Addr};

use crate::flash;
use crate::loader;
use crate::platform::{Current, Platform};
use crate::time;

/// An install between `install_begin` and `install_end`.
struct Install {
    first: u16,
    count: u16,
    length: u32,
    /// Offset the next data frame must start at.
    next: u32,
    stats: flash::Stats,
    started: u64,
}

impl Install {
    fn offset(&self) -> u32 {
        store::APPS_OFFSET + self.first as u32 * store::EXTENT_SIZE
    }
}

pub struct Store {
    window: usize,
    catalog: Option<Catalog<'static>>,
    /// The sector (0 or 1) that holds `catalog`.
    active: Option<usize>,
    install: Option<Install>,
}

/// Where a new catalog is built before it is written (RAM, as the flash
/// write path requires).
static mut SECTOR_BUF: [u8; store::SECTOR_SIZE] = [0; store::SECTOR_SIZE];

impl Store {
    /// `window` is the address of flash offset 0.
    pub fn open(window: usize) -> Store {
        let mut s = Store { window, catalog: None, active: None, install: None };
        s.reload();
        match &s.catalog {
            Some(c) => event!(Info, "store.open", window = Addr(window as u32), catalog_seq = c.seq, apps = c.count as u32),
            None => event!(Warn, "store.empty", window = Addr(window as u32)),
        }
        s
    }

    fn bytes(&self, offset: u32, len: usize) -> &'static [u8] {
        unsafe { core::slice::from_raw_parts((self.window + offset as usize) as *const u8, len) }
    }

    fn reload(&mut self) {
        let sector = |i: u32| self.bytes(store::APPMETA_OFFSET + i * store::SECTOR_SIZE as u32, store::SECTOR_SIZE);
        let (a, b) = (sector(0), sector(1));
        self.active = store::active_index(a, b);
        self.catalog = self.active.and_then(|i| Catalog::parse(if i == 0 { a } else { b }));
    }

    pub fn entries(&self) -> impl Iterator<Item = Entry> + Clone + 'static {
        self.catalog.into_iter().flat_map(|c| c.entries())
    }

    /// The image bytes of the current version of `name` (not yet verified).
    pub fn image(&self, name: &str) -> Option<&'static [u8]> {
        let e = self.entry(name)?;
        Some(self.bytes(e.offset(), e.length as usize))
    }

    /// The version of `name` kept to go back to.
    fn previous(&self, name: &str) -> Option<Entry> {
        self.entries().find(|e| e.name() == name && e.state == State::Previous)
    }

    pub fn log_contents(&self) {
        for e in self.entries() {
            event!(
                Info,
                "store.app",
                name = e.name(),
                major = e.version[0],
                minor = e.version[1],
                patch = e.version[2],
                offset = Addr(e.offset()),
                length = e.length,
                state = e.state as u8,
                flags = e.flags
            );
        }
    }

    /// The catalog entry of the current version of `name`.
    pub fn entry(&self, name: &str) -> Option<Entry> {
        self.entries().find(|e| e.name() == name && e.is_current())
    }

    /// Starts an install of `length` bytes; returns the first extent. Drops
    /// an install already in progress. `busy` lists extents (first, count)
    /// still in use outside the catalog: the text of running XIP processes
    /// whose app was removed or replaced (doc/15 §4).
    pub fn install_begin(&mut self, length: u32, busy: &[(u16, u16)]) -> Result<u32, &'static str> {
        if let Some(old) = self.install.take() {
            event!(Warn, "store.install_dropped", received = old.next, length = old.length);
        }
        if (length as usize) < t5a::HEADER_SIZE || length > store::APPS_SIZE {
            return Err("bad length");
        }
        let count = store::extents_for(length);
        let in_use = busy.iter().map(|&(first, count)| Entry { extent_first: first, extent_count: count, ..EMPTY });
        let first = store::find_free(self.entries().chain(in_use), count).ok_or("store full")?;
        event!(Info, "store.install_begin", length = length, extent_first = first, extents = count, busy = busy.len() as u32);
        self.install = Some(Install { first, count, length, next: 0, stats: flash::Stats::default(), started: Current::now() });
        Ok(first as u32)
    }

    /// Drops an install in progress (a kernel update begins).
    pub fn install_cancel(&mut self) {
        if let Some(old) = self.install.take() {
            event!(Warn, "store.install_dropped", received = old.next, length = old.length);
        }
    }

    /// Writes the next piece of the image, erasing each sector as the data
    /// reaches it. Any error drops the install.
    pub fn install_data(&mut self, offset: u32, data: &[u8]) -> Result<(), &'static str> {
        let mut i = self.install.take().ok_or("no install in progress")?;
        let end = offset as usize + data.len();
        let full = data.len() == t5_abi::ctl::INSTALL_CHUNK || end == i.length as usize;
        if offset != i.next || end > i.length as usize || !full {
            return Err("data out of order");
        }
        let at = i.offset() + offset;
        if at as usize % store::SECTOR_SIZE == 0 {
            flash::erase_sector(at, &mut i.stats)?;
        }
        flash::write(at, data, &mut i.stats)?;
        i.next = end as u32;
        self.install = Some(i);
        Ok(())
    }

    /// Verifies the written image and makes it the app of its name.
    pub fn install_end(&mut self) -> Result<Entry, &'static str> {
        let mut i = self.install.take().ok_or("no install in progress")?;
        if i.next != i.length {
            return Err("image incomplete");
        }
        let image = self.bytes(i.offset(), i.length as usize);
        let info = match loader::verify(image).and_then(|signed| t5a::parse(signed, t5_abi::ABI_MAJOR, t5_abi::ABI_MINOR)) {
            Ok(img) => img.info,
            Err(e) => {
                let mut why = t5_abi::log::FmtBuf::<64>::new();
                let _ = core::fmt::Write::write_fmt(&mut why, format_args!("{e:?}"));
                event!(Warn, "store.rejected", error = why.as_str());
                return Err("image rejected");
            }
        };
        let old = self.entry(info.name);
        let prev = self.previous(info.name);
        let installed = old.iter().chain(prev.iter()).map(|e| e.secure_version).max();
        if let Some(installed) = installed
            && installed > info.secure_version
        {
            event!(Warn, "store.rollback", name = info.name, installed = installed, offered = info.secure_version);
            return Err("secure_version rollback");
        }
        let mut name = [0u8; 32];
        name[..info.name.len()].copy_from_slice(info.name.as_bytes());
        let entry = Entry {
            name,
            extent_first: i.first,
            extent_count: i.count,
            length: i.length,
            version: info.version,
            secure_version: info.secure_version,
            state: State::Valid,
            flags: info.flags as u8,
        };
        // Over a valid version, the new one must prove itself and the old
        // one stays as `Previous`; over an unconfirmed one, that one goes
        // and the `Previous` stays (doc/16 §4.2).
        let keep = match (old, prev) {
            (Some(o), _) if o.state == State::Valid => Some(Entry { state: State::Previous, ..o }),
            (Some(_), Some(p)) => Some(p),
            _ => None,
        };
        let entry = Entry { state: if keep.is_some() { State::PendingVerify } else { State::Valid }, ..entry };
        let seq = self.commit(|e| e.name() != info.name, [Some(entry), keep], &mut i.stats)?;
        let s = i.stats;
        event!(
            Info,
            "store.installed",
            name = info.name,
            major = info.version[0],
            minor = info.version[1],
            patch = info.version[2],
            extent_first = i.first,
            length = i.length,
            catalog_seq = seq,
            replaced = old.is_some(),
            pending = keep.is_some(),
            erases = s.erases,
            writes = s.writes,
            erase_us = s.erase_us,
            write_us = s.write_us,
            max_op_us = s.max_us,
            total_ms = time::ticks_to_ns(Current::now() - i.started) / 1_000_000
        );
        Ok(entry)
    }

    pub fn remove(&mut self, name: &str) -> Result<u32, &'static str> {
        if self.entries().all(|e| e.name() != name) {
            return Err("no such app");
        }
        let seq = self.commit(|e| e.name() != name, [None, None], &mut flash::Stats::default())?;
        event!(Info, "store.removed", name = name, catalog_seq = seq);
        Ok(seq)
    }

    /// Makes the unconfirmed version of `name` valid and drops the previous
    /// one, if that version is still the one at extent `first` (another may
    /// have been installed meanwhile). `how` says what proved it.
    pub fn confirm(&mut self, name: &str, first: u16, how: &'static str) -> Result<u32, &'static str> {
        let e = self.entry(name).filter(|e| e.state == State::PendingVerify && e.extent_first == first).ok_or("no longer pending")?;
        let seq = self.commit(|x| x.name() != name, [Some(Entry { state: State::Valid, ..e }), None], &mut flash::Stats::default())?;
        event!(Info, "store.confirmed", name = name, major = e.version[0], minor = e.version[1], patch = e.version[2], by = how, catalog_seq = seq);
        Ok(seq)
    }

    /// Brings back the previous version of `name`, dropping the current one
    /// (with `Some(first)`: only if that is still the unconfirmed version at
    /// extent `first`).
    pub fn rollback(&mut self, name: &str, first: Option<u16>, why: &'static str) -> Result<u32, &'static str> {
        if let Some(first) = first
            && !self.entry(name).is_some_and(|e| e.state == State::PendingVerify && e.extent_first == first)
        {
            return Err("no longer pending");
        }
        let p = self.previous(name).ok_or("no previous version")?;
        let seq = self.commit(|x| x.name() != name, [Some(Entry { state: State::Valid, ..p }), None], &mut flash::Stats::default())?;
        event!(Warn, "store.rolled_back", name = name, major = p.version[0], minor = p.version[1], patch = p.version[2], reason = why, catalog_seq = seq);
        Ok(seq)
    }

    /// Writes a catalog of the entries `keep` accepts plus those in `add`, with the
    /// next sequence number, into the sector not in force; returns the new
    /// sequence number.
    fn commit(&mut self, keep: impl Fn(&Entry) -> bool, add: [Option<Entry>; 2], stats: &mut flash::Stats) -> Result<u32, &'static str> {
        let seq = self.catalog.map_or(1, |c| c.seq.wrapping_add(1));
        let target = if self.active == Some(0) { 1 } else { 0 };
        let buf = unsafe { &mut *core::ptr::addr_of_mut!(SECTOR_BUF) };
        if !store::build_sector(seq, self.entries().filter(|e| keep(e)).chain(add.into_iter().flatten()), buf) {
            return Err("catalog full");
        }
        let at = store::APPMETA_OFFSET + target * store::SECTOR_SIZE as u32;
        flash::erase_sector(at, stats)?;
        flash::write(at, buf, stats)?;
        self.reload();
        if self.catalog.map(|c| c.seq) != Some(seq) || self.active != Some(target as usize) {
            event!(Error, "store.commit_failed", catalog_seq = seq, sector = target);
            return Err("catalog write failed");
        }
        Ok(seq)
    }

    /// Erase, write and read back a spare sector, the last of `apps`
    /// (doc/14 §5.4). Leaves it erased.
    pub fn flash_test(&mut self) -> Result<u32, &'static str> {
        let last = (store::EXTENTS - 1) as u16;
        let in_use = |first: u16, count: u16| first <= last && last < first + count;
        if self.entries().any(|e| in_use(e.extent_first, e.extent_count))
            || self.install.as_ref().is_some_and(|i| in_use(i.first, i.count))
        {
            return Err("spare sector in use");
        }
        let at = store::APPS_OFFSET + store::APPS_SIZE - store::SECTOR_SIZE as u32;
        let pattern = |k: usize| (k as u8).wrapping_mul(7) ^ (k >> 8) as u8 ^ 0x5A;
        let read = |k: usize| unsafe { core::ptr::read_volatile((self.window + at as usize + k) as *const u8) };
        let mut stats = flash::Stats::default();
        let mut erase = flash::Stats::default();
        flash::erase_sector(at, &mut erase)?;
        let erased = (0..store::SECTOR_SIZE).all(|k| read(k) == 0xFF);
        let mut page = [0u8; flash::PAGE];
        for p in 0..store::SECTOR_SIZE / flash::PAGE {
            for (j, b) in page.iter_mut().enumerate() {
                *b = pattern(p * flash::PAGE + j);
            }
            flash::write(at + (p * flash::PAGE) as u32, &page, &mut stats)?;
        }
        let written = (0..store::SECTOR_SIZE).all(|k| read(k) == pattern(k));
        flash::erase_sector(at, &mut erase)?;
        let cleaned = (0..store::SECTOR_SIZE).all(|k| read(k) == 0xFF);
        let (device_id, chip_size) = Current::flash_chip().unwrap_or((0, 0));
        let ok = erased && written && cleaned;
        event!(
            Info,
            "flash.test",
            ok = ok,
            offset = Addr(at),
            erased = erased,
            written = written,
            cleaned = cleaned,
            erase_us = erase.erase_us / 2,
            write_us = stats.write_us,
            max_write_us = stats.max_us,
            device_id = device_id,
            chip_size = chip_size
        );
        if ok { Ok(at) } else { Err("read back differs") }
    }
}

/// A blank entry, for describing extents in use.
const EMPTY: Entry = Entry {
    name: [0; 32],
    extent_first: 0,
    extent_count: 0,
    length: 0,
    version: [0; 3],
    secure_version: 0,
    state: State::Invalid,
    flags: 0,
};
