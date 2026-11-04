#![no_std]
#![no_main]

//! The settings service (doc/50, doc/51 §2): named values for the other
//! services (`t5_abi::cfg`), kept in the P4's flash (`flash:cfg`, the first
//! 64 KiB of `userdata`): the table in sectors 0 and 1 (`cfg::Table`'s
//! image; a save goes to the older copy, so one cut short leaves the last),
//! the sealing key in sectors 2 and 3. A secret's value is sealed
//! (`seal`). Each client's grants (doc/52, `t5_abi::cfg`) say which keys it
//! may read, change, and read the secret values of; others are refused
//! (`cfg.denied`). Values are never logged, keys are.
//!
//! The first time the flash holds no table, the SD card's `T5OS/CONFIG.A`
//! and `.B` (doc/50, before the flash) are taken over, their secrets
//! sealed, and deleted from the card.
//!
//! `run cfg MODE`: 0, the flash (without it, as 1 but changes answer
//! `NOT_SAVED`); 1, in memory only. Built with `--permit listen:cfg
//! --permit flash:cfg --permit random --permit svc:fs/rw:T5OS/CONFIG.A
//! --permit svc:fs/rw:T5OS/CONFIG.B`.

mod seal;

use t5_rt::cfg::{self, FILE_MAX, HEADER, STORED_MAX, Table, VALUE_MAX, err};
use t5_rt::{Channel, Error, FOREVER, Files, Grants, Handle, Listener, cfgflash, chan, event, fs, rights, sleep_ms, t5a, wait_raw};

use seal::Key;

t5_rt::entry!(main);

/// The SD card's files of doc/50.
const SD_FILES: [&str; 2] = ["T5OS/CONFIG.A", "T5OS/CONFIG.B"];
/// The table's copies and the key's, as positions in `flash:cfg`.
const TABLE_AT: [u64; 2] = [0, cfgflash::SECTOR as u64];
const KEY_AT: [u64; 2] = [2 * cfgflash::SECTOR as u64, 3 * cfgflash::SECTOR as u64];
const CLIENTS: usize = 6;

struct Store {
    table: Table,
    flash: Option<Handle>,
    key: Option<Key>,
    /// The generation and copy (0 = A, 1 = B) of the newest good table in
    /// flash (`None`: none yet).
    newest: Option<(u32, usize)>,
    buf: [u8; FILE_MAX],
}

fn main(mode: usize, _: usize) -> usize {
    static mut STORE: Store = Store { table: Table::new(), flash: None, key: None, newest: None, buf: [0; FILE_MAX] };
    // SAFETY: the only reference, taken once.
    let store = unsafe { &mut *core::ptr::addr_of_mut!(STORE) };
    // Listening first: apps that need `svc:cfg` start meanwhile, and their
    // requests wait until the table is loaded.
    let listener = match Listener::open(cfg::SVC) {
        Ok(l) => l,
        Err(e) => {
            event!(Error, "cfg.failed", step = 1u32, error = e as u32);
            return 1;
        }
    };
    if mode == 0 {
        store.open_flash();
    }
    event!(Info, "cfg.ready", values = store.table.iter().count() as u32, flash = store.flash.is_some(), sealing = store.key.is_some());
    serve(store, &listener)
}

impl Store {
    fn flash(&self) -> Result<&Handle, Error> {
        self.flash.as_ref().ok_or(Error::NoDevice)
    }

    fn read_at(&mut self, at: u64, len: usize) -> Result<(), Error> {
        let f = self.flash.as_ref().ok_or(Error::NoDevice)?;
        f.ctl(cfgflash::SEEK, at)?;
        f.read(&mut self.buf[..len]).map(|_| ())
    }

    /// Erases the sector at `at` and writes `data` there.
    fn write_at(&self, at: u64, data: &[u8]) -> Result<(), Error> {
        let f = self.flash()?;
        f.ctl(cfgflash::ERASE, at)?;
        f.ctl(cfgflash::SEEK, at)?;
        f.write(data).map(|_| ())
    }

    /// The key (made the first time), the newest table, or the SD card's.
    fn open_flash(&mut self) {
        match Handle::open(cfgflash::NAME, rights::READ | rights::WRITE | rights::CTL) {
            Ok(h) => self.flash = Some(h),
            Err(e) => {
                event!(Warn, "cfg.no_flash", error = e as u32);
                return;
            }
        }
        self.load_key();
        for (i, &at) in TABLE_AT.iter().enumerate() {
            let len = match self.read_at(at, HEADER) {
                Ok(()) => Table::image_len(&self.buf[..HEADER]),
                Err(_) => None,
            };
            let Some(len) = len else { continue };
            if self.read_at(at, len).is_err() {
                continue;
            }
            match Table::load(&self.buf[..len]) {
                Some((t, generation)) if self.newest.is_none_or(|(g, _)| cfg::newer(g, generation)) => {
                    self.table = t;
                    self.newest = Some((generation, i));
                }
                Some(_) => {}
                // (A save cut short.)
                None => event!(Warn, "cfg.bad_copy", copy = i as u32),
            }
        }
        match self.newest {
            Some((generation, copy)) => event!(Info, "cfg.loaded", generation = generation, copy = copy as u32),
            None => self.take_over_sd(),
        }
    }

    fn load_key(&mut self) {
        for &at in &KEY_AT {
            if self.read_at(at, seal::RECORD).is_ok()
                && let Some(k) = Key::from_record(&self.buf[..seal::RECORD])
            {
                self.key = Some(k);
                return;
            }
        }
        let Some((k, record)) = Key::create() else {
            event!(Error, "cfg.no_key");
            return;
        };
        let ok = KEY_AT.iter().all(|&at| self.write_at(at, &record).is_ok());
        event!(Info, "cfg.key_created", saved = ok);
        if ok {
            self.key = Some(k);
        }
    }

    /// The SD card's table of doc/50: taken over, its secrets sealed, saved
    /// in flash, and deleted from the card. Without one, an empty table is
    /// saved, so this happens once. The kernel starts `fs` at boot; it may
    /// take a moment to listen.
    fn take_over_sd(&mut self) {
        let mut files = None;
        for _ in 0..30 {
            files = Files::connect().ok();
            if files.is_some() {
                break;
            }
            sleep_ms(100);
        }
        let mut found = 0;
        if let Some(files) = &mut files {
            let mut newest: Option<u32> = None;
            for path in SD_FILES {
                let Ok(f) = files.open(path, fs::flag::READ) else { continue };
                let n = files.read(f, &mut self.buf).unwrap_or(0);
                let _ = files.close(f);
                if let Some((t, generation)) = Table::load(&self.buf[..n])
                    && newest.is_none_or(|g| cfg::newer(g, generation))
                {
                    self.table = t;
                    newest = Some(generation);
                }
                found += 1;
            }
            self.seal_all();
        }
        let saved = self.save().is_ok();
        let mut deleted = 0;
        if saved && let Some(files) = &files {
            for path in SD_FILES {
                deleted += files.delete(path).is_ok() as u32;
            }
        }
        event!(Info, "cfg.took_over_sd", files = found, values = self.table.iter().count() as u32, saved = saved, deleted = deleted);
    }

    /// Seals every secret not sealed yet (taken over from the SD card).
    fn seal_all(&mut self) {
        let Some(key) = &self.key else { return };
        let mut todo = [[0u8; cfg::KEY_MAX + 1]; 16];
        let mut n = 0;
        for (flags, k, _) in self.table.iter() {
            if flags & cfg::SECRET != 0 && flags & cfg::SEALED == 0 && n < todo.len() {
                todo[n][0] = k.len() as u8;
                todo[n][1..1 + k.len()].copy_from_slice(k);
                n += 1;
            }
        }
        for t in &todo[..n] {
            let k = &t[1..1 + t[0] as usize];
            let mut sealed = [0u8; STORED_MAX];
            let Some(v) = self.table.get(k).map(|(_, v)| v) else { continue };
            if let Some(m) = key.seal(k, v, &mut sealed) {
                let _ = self.table.set(k, cfg::SECRET | cfg::SEALED, &sealed[..m]);
            }
        }
    }

    /// Writes the table over the older copy.
    fn save(&mut self) -> Result<(), Error> {
        self.flash()?;
        let (generation, copy) = self.newest.map_or((1, 0), |(g, c)| (g.wrapping_add(1), 1 - c));
        let n = self.table.image(generation, &mut self.buf);
        let image = self.buf;
        self.write_at(TABLE_AT[copy], &image[..n])?;
        self.newest = Some((generation, copy));
        Ok(())
    }

    /// After a change: saved, or `NOT_SAVED`.
    fn changed(&mut self, key: &[u8]) -> u8 {
        let key = core::str::from_utf8(key).unwrap_or("?");
        if self.flash.is_none() {
            event!(Info, "cfg.changed", key = key, saved = false);
            return err::NOT_SAVED;
        }
        match self.save() {
            Ok(()) => {
                event!(Info, "cfg.changed", key = key, saved = true);
                0
            }
            Err(e) => {
                event!(Warn, "cfg.save_failed", key = key, error = e as u32);
                err::NOT_SAVED
            }
        }
    }

    /// A value as a client may see it into `out` after `head` bytes: none
    /// of a secret without a `secret:` grant on it, a sealed one opened.
    fn value_out(&self, out: &mut [u8; cfg::MSG_MAX], head: usize, flags: u8, name: &[u8], v: &[u8], secret: bool) -> usize {
        if flags & cfg::SECRET != 0 && !secret {
            return head;
        }
        if flags & cfg::SEALED != 0 {
            let mut plain = [0u8; VALUE_MAX];
            let opened = self.key.as_ref().and_then(|k| k.open(name, v, &mut plain));
            let Some(n) = opened else {
                event!(Warn, "cfg.unseal_failed", key = core::str::from_utf8(name).unwrap_or("?"));
                return head;
            };
            out[head..head + n].copy_from_slice(&plain[..n]);
            return head + n;
        }
        out[head..head + v.len()].copy_from_slice(v);
        head + v.len()
    }

    /// One request of client `c`; the answer into `out`.
    fn handle(&mut self, req: &[u8], c: &Client, out: &mut [u8; cfg::MSG_MAX]) -> usize {
        let g = &c.grants;
        let readable = |k: &[u8]| g.prefix_of(b"r", k) || g.prefix_of(b"secret", k);
        let denied = |out: &mut [u8; cfg::MSG_MAX], op: &str, k: &[u8]| {
            event!(Warn, "cfg.denied", app = c.name(), op = op, key = core::str::from_utf8(k).unwrap_or("?"));
            out[0] = cfg::ERR;
            out[1] = err::DENIED;
            2
        };
        // `[op, key_len, key…, rest…]` (`SET`: the flags first).
        let keyed = |at: usize| -> Option<(&[u8], &[u8])> {
            let k = *req.get(at)? as usize;
            Some((req.get(at + 1..at + 1 + k)?, &req[at + 1 + k..]))
        };
        let error = |out: &mut [u8; cfg::MSG_MAX], code: u8| {
            out[0] = cfg::ERR;
            out[1] = code;
            2
        };
        match (req.first().copied(), req.get(1).copied()) {
            (Some(cfg::GET), _) => match keyed(1).map(|(k, _)| (k, self.table.get(k))) {
                Some((k, _)) if !readable(k) => denied(out, "get", k),
                Some((k, Some((flags, v)))) => {
                    out[0] = cfg::VALUE;
                    out[1] = flags & cfg::SECRET;
                    self.value_out(out, 2, flags, k, v, g.prefix_of(b"secret", k))
                }
                Some((_, None)) => {
                    out[0] = cfg::NONE;
                    1
                }
                None => error(out, err::BAD),
            },
            (Some(cfg::SET), Some(flags)) => {
                let Some((k, v)) = keyed(2) else { return error(out, err::BAD) };
                if !g.prefix_of(b"w", k) {
                    return denied(out, "set", k);
                }
                if v.len() > VALUE_MAX {
                    return error(out, err::TOO_LONG);
                }
                // A secret (new, or one already) is sealed when there is a key.
                let is_secret = flags & cfg::SECRET != 0 || self.table.get(k).is_some_and(|(f, _)| f & cfg::SECRET != 0);
                let mut sealed = [0u8; STORED_MAX];
                let r = match (is_secret, &self.key) {
                    (true, Some(key)) => match key.seal(k, v, &mut sealed) {
                        Some(m) => self.table.set(k, cfg::SECRET | cfg::SEALED, &sealed[..m]),
                        None => Err(err::BAD),
                    },
                    (true, None) => self.table.set(k, cfg::SECRET, v),
                    (false, _) => self.table.set(k, 0, v),
                };
                if let Err(code) = r {
                    return error(out, code);
                }
                match self.changed(k) {
                    0 => {
                        out[0] = cfg::OK;
                        1
                    }
                    code => error(out, code),
                }
            }
            (Some(cfg::DEL), _) => {
                let Some((k, _)) = keyed(1) else { return error(out, err::BAD) };
                if !g.prefix_of(b"w", k) {
                    return denied(out, "del", k);
                }
                if self.table.remove(k) {
                    let code = self.changed(k);
                    if code != 0 {
                        return error(out, code);
                    }
                }
                out[0] = cfg::OK;
                1
            }
            // Only the keys the client may read.
            (Some(cfg::LIST), Some(index)) => match self.table.iter().filter(|(_, k, _)| readable(k)).nth(index as usize) {
                Some((flags, k, v)) => {
                    out[0] = cfg::ENTRY;
                    out[1] = flags & cfg::SECRET;
                    out[2] = k.len() as u8;
                    out[3..3 + k.len()].copy_from_slice(k);
                    self.value_out(out, 3 + k.len(), flags, k, v, g.prefix_of(b"secret", k))
                }
                None => {
                    out[0] = cfg::END;
                    1
                }
            },
            _ => error(out, err::BAD),
        }
    }
}

/// A client: its connection, its grants and its app name (for the log).
struct Client {
    chan: Channel,
    grants: Grants,
    name: [u8; t5a::MAX_NAME],
    name_len: usize,
}

impl Client {
    fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("?")
    }
}

fn serve(store: &mut Store, listener: &Listener) -> ! {
    let mut clients: [Option<Client>; CLIENTS] = [const { None }; CLIENTS];
    loop {
        let mut hs = [0u32; 1 + CLIENTS];
        let mut at = [usize::MAX; 1 + CLIENTS];
        hs[0] = listener.0.raw();
        let mut n = 1;
        for (i, c) in clients.iter().enumerate() {
            if let Some(c) = c {
                hs[n] = c.chan.0.raw();
                at[n] = i;
                n += 1;
            }
        }
        let closed = match wait_raw(&hs[..n], FOREVER) {
            Ok(ev) if ev.kind == chan::PEER_CLOSED && ev.index >= 1 => Some(at[ev.index]),
            _ => None,
        };
        while let Ok(chan) = listener.accept() {
            match clients.iter_mut().find(|c| c.is_none()) {
                Some(slot) => {
                    let mut name = [0u8; t5a::MAX_NAME];
                    let name_len = chan.peer_name(&mut name);
                    *slot = Some(Client { grants: chan.peer_grants(), chan, name, name_len });
                }
                // Too many: closing it tells the client.
                None => event!(Warn, "cfg.too_many_clients"),
            }
        }
        for (i, slot) in clients.iter_mut().enumerate() {
            let Some(c) = slot else { continue };
            let mut req = [0u8; cfg::MSG_MAX];
            let mut out = [0u8; cfg::MSG_MAX];
            loop {
                match c.chan.0.read(&mut req) {
                    Ok(k) => {
                        let m = store.handle(&req[..k], c, &mut out);
                        let _ = c.chan.send(&out[..m]);
                    }
                    Err(Error::Busy) if closed != Some(i) => break,
                    Err(_) => {
                        *slot = None;
                        break;
                    }
                }
            }
        }
    }
}
