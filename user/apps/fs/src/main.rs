#![no_std]
#![no_main]

//! The file service (M7c, doc/18 §5): the SD card's first FAT16/32
//! partition through `embedded-sdmmc`, for clients on `svc:fs`. Each client
//! gets a 64 KiB slice of shared memory for file data and listings; requests
//! and replies are channel messages (`t5_abi::fs`). Each client may use
//! its own folder and the paths its grants name (doc/52, `t5_abi::fs`);
//! the rest is refused (`fs.denied`). Open files belong to the client that
//! opened them. Long names (doc/63) work for what exists: each part of a
//! path is looked up by its long or 8.3 name, either case, and used by its
//! 8.3 name; new names must be 8.3.
//!
//! Built with `--permit sd --permit shm --permit listen:fs`.

use core::cell::RefCell;
use core::ops::ControlFlow;

use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, LfnBuffer, Mode, RawDirectory, RawFile, RawVolume, TimeSource, Timestamp, VolumeIdx, VolumeManager,
};
use t5_rt::{Channel, Error, FOREVER, Grants, Handle, Listener, Shm, chan, event, rights, sd, t5a, wait_raw};
use t5_rt::fs::{self as proto, flag, op, status};

t5_rt::entry!(main);

const CLIENTS: usize = 4;
const FILES: usize = 8;

// ---- the card, as embedded-sdmmc's block device ----

struct Card {
    h: Handle,
    sectors: u32,
    buf: RefCell<[u8; sd::MAX_IO]>,
}

#[derive(Debug)]
struct CardError(Error);

impl core::fmt::Display for CardError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "sd error {:?}", self.0)
    }
}

impl core::error::Error for CardError {}

const PER_IO: usize = sd::MAX_IO / sd::SECTOR;

impl BlockDevice for Card {
    type Error = CardError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), CardError> {
        let mut buf = self.buf.borrow_mut();
        for (k, chunk) in blocks.chunks_mut(PER_IO).enumerate() {
            let lba = start.0 + (k * PER_IO) as u32;
            let len = chunk.len() * sd::SECTOR;
            self.h.ctl(sd::SEEK, lba as u64).map_err(CardError)?;
            self.h.read(&mut buf[..len]).map_err(CardError)?;
            for (i, b) in chunk.iter_mut().enumerate() {
                b.contents.copy_from_slice(&buf[i * sd::SECTOR..(i + 1) * sd::SECTOR]);
            }
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), CardError> {
        let mut buf = self.buf.borrow_mut();
        for (k, chunk) in blocks.chunks(PER_IO).enumerate() {
            let lba = start.0 + (k * PER_IO) as u32;
            for (i, b) in chunk.iter().enumerate() {
                buf[i * sd::SECTOR..(i + 1) * sd::SECTOR].copy_from_slice(&b.contents);
            }
            self.h.ctl(sd::SEEK, lba as u64).map_err(CardError)?;
            self.h.write(&buf[..chunk.len() * sd::SECTOR]).map_err(CardError)?;
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, CardError> {
        Ok(BlockCount(self.sectors))
    }
}

/// No real-time clock yet: every file gets the same time (doc/18 §10).
struct FixedClock;

impl TimeSource for FixedClock {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp { year_since_1970: 56, zero_indexed_month: 9, zero_indexed_day: 30, hours: 12, minutes: 0, seconds: 0 }
    }
}

type Vm = VolumeManager<Card, FixedClock, 4, FILES, 1>;

fn status_of<E: core::error::Error>(e: &embedded_sdmmc::Error<E>) -> u32 {
    use embedded_sdmmc::Error as E2;
    match e {
        E2::NotFound => status::NOT_FOUND,
        E2::FileAlreadyExists | E2::DirAlreadyExists => status::EXISTS,
        E2::FilenameError(_) => status::BAD_NAME,
        E2::TooManyOpenFiles | E2::TooManyOpenDirs => status::TOO_MANY_OPEN,
        E2::DeviceError(_) => status::IO,
        E2::OpenedDirAsFile | E2::OpenedFileAsDir => status::NOT_A_FILE,
        E2::NotEnoughSpace | E2::DiskFull => status::FULL,
        _ => status::OTHER,
    }
}

// ---- the service ----

struct Client {
    chan: Channel,
    /// Offset of the client's slice in the pool.
    offset: usize,
    /// What it may use (doc/52): its grants, and its own folder.
    grants: Grants,
    own: [u8; OWN_MAX],
    own_len: usize,
    name: [u8; t5a::MAX_NAME],
    name_len: usize,
}

/// `T5OS/APPS/` and up to 8 characters.
const APPS_DIR: &str = "T5OS/APPS";
const OWN_MAX: usize = APPS_DIR.len() + 1 + 8;
/// The longest path a request carries.
const PATH_MAX: usize = chan::MSG_MAX - proto::REQUEST_HEADER;

impl Client {
    fn new(chan: Channel, offset: usize) -> Client {
        let mut name = [0u8; t5a::MAX_NAME];
        let name_len = chan.peer_name(&mut name);
        let mut own = [0u8; OWN_MAX];
        own[..APPS_DIR.len()].copy_from_slice(APPS_DIR.as_bytes());
        own[APPS_DIR.len()] = b'/';
        let n = name_len.min(8);
        for (d, s) in own[APPS_DIR.len() + 1..].iter_mut().zip(&name[..n]) {
            *d = s.to_ascii_uppercase();
        }
        Client { grants: chan.peer_grants(), chan, offset, own, own_len: APPS_DIR.len() + 1 + n, name, name_len }
    }

    fn own(&self) -> &[u8] {
        &self.own[..self.own_len]
    }

    /// Whether the client may read (or write) `path` (normalised): its own
    /// folder, or a `rw:` (or, to read, `r:`) grant over it.
    fn may(&self, path: &[u8], write: bool) -> bool {
        under(path, self.own())
            || self.grants.iter().any(|g| {
                let p = match (g.strip_prefix(b"rw:"), g.strip_prefix(b"r:")) {
                    (Some(p), _) => p,
                    (None, Some(p)) if !write => p,
                    _ => return false,
                };
                let mut buf = [0u8; PATH_MAX];
                under(path, upper(p, &mut buf))
            })
    }

    /// Whether `path` is a directory above a place it may write (making it
    /// is harmless).
    fn above_allowed(&self, path: &[u8]) -> bool {
        let above = |p: &[u8]| p.len() > path.len() && p.starts_with(path) && p[path.len()] == b'/';
        above(self.own())
            || self.grants.iter().filter_map(|g| g.strip_prefix(b"rw:")).any(|p| {
                let mut buf = [0u8; PATH_MAX];
                above(upper(p, &mut buf))
            })
    }

    fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("?")
    }
}

/// Whether `path` is `p` or under it (an empty `p`: everything).
fn under(path: &[u8], p: &[u8]) -> bool {
    p.is_empty() || path == p || (path.len() > p.len() && path.starts_with(p) && path[p.len()] == b'/')
}

/// A grant's path as the card compares it.
fn upper<'b>(p: &[u8], buf: &'b mut [u8; PATH_MAX]) -> &'b [u8] {
    let p = p.strip_prefix(b"/").unwrap_or(p);
    let p = p.strip_suffix(b"/").unwrap_or(p);
    let n = p.len().min(PATH_MAX);
    for (d, s) in buf.iter_mut().zip(&p[..n]) {
        *d = s.to_ascii_uppercase();
    }
    &buf[..n]
}

/// `path` as the card compares it: upper case, no leading or trailing `/`,
/// `~` for the client's own folder; `None` for `.`, `..` or empty parts.
fn normalise<'b>(path: &str, own: &[u8], out: &'b mut [u8; PATH_MAX]) -> Option<&'b str> {
    let path = path.trim_matches('/');
    let rest = if path == "~" { Some("") } else { path.strip_prefix("~/") };
    let mut n = 0;
    let mut put = |b: &[u8]| -> Option<()> {
        out.get_mut(n..n + b.len())?.copy_from_slice(b);
        n += b.len();
        Some(())
    };
    let tail = match rest {
        Some(r) => {
            put(own)?;
            if !r.is_empty() {
                put(b"/")?;
            }
            r
        }
        None => path,
    };
    if !tail.is_empty() {
        for (i, part) in tail.split('/').enumerate() {
            if part.is_empty() || part == "." || part == ".." {
                return None;
            }
            if i > 0 {
                put(b"/")?;
            }
            for b in part.bytes() {
                put(&[b.to_ascii_uppercase()])?;
            }
        }
    }
    core::str::from_utf8(&out[..n]).ok()
}

struct Open {
    client: usize,
    file: RawFile,
}

struct Service<'a> {
    vm: &'a Vm,
    volume: RawVolume,
    pool: &'static mut [u8],
    files: [Option<Open>; FILES],
}

/// An entry's 8.3 name, as embedded-sdmmc takes it ("NAME.EXT").
#[derive(Clone, Copy)]
struct Short {
    buf: [u8; 12],
    len: usize,
}

impl Short {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

/// The last name `parent` found: an existing entry's 8.3 name, or the
/// path's last part as given (its length; it ends the path).
enum Name {
    Short(Short),
    Given(usize),
}

impl Name {
    fn as_str<'a>(&'a self, path: &'a str) -> &'a str {
        match self {
            Name::Short(s) => s.as_str(),
            Name::Given(n) => {
                let path = path.trim_matches('/');
                &path[path.len() - n..]
            }
        }
    }
}

/// A buffer for any long name (255 UTF-16 units, 3 bytes each at most).
const LFN_BYTES: usize = 255 * 3;

impl Service<'_> {
    /// The 8.3 name of the entry in `dir` called `part` (doc/63): its long
    /// name or its 8.3 name, either case. `None` if there is none (`part`
    /// may then be a new 8.3 name).
    fn lookup(&self, dir: RawDirectory, part: &str) -> Option<Short> {
        let mut storage = [0u8; LFN_BYTES];
        let mut lfn = LfnBuffer::new(&mut storage);
        let mut found = None;
        let _ = self.vm.iterate_dir_lfn(dir, &mut lfn, |e, long| {
            let mut s = Short { buf: [0; 12], len: 0 };
            let mut w = Writer { buf: &mut s.buf, n: 0 };
            let _ = core::fmt::write(&mut w, format_args!("{}", e.name));
            s.len = w.n;
            if long.is_some_and(|l| l.eq_ignore_ascii_case(part)) || s.as_str().eq_ignore_ascii_case(part) {
                found = Some(s);
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });
        found
    }

    /// Opens the directory that holds the last component of `path`, and
    /// returns it with that last name: the existing entry's 8.3 name, or
    /// the name as given. The caller closes the directory.
    fn parent(&self, path: &str) -> Result<(RawDirectory, Name), u32> {
        let path = path.trim_matches('/');
        let mut dir = self.vm.open_root_dir(self.volume).map_err(|e| status_of(&e))?;
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            let short = self.lookup(dir, part);
            if parts.peek().is_none() {
                return Ok((dir, short.map_or(Name::Given(part.len()), Name::Short)));
            }
            let next = match short {
                Some(s) => self.vm.open_dir(dir, s.as_str()),
                None => Err(embedded_sdmmc::Error::NotFound),
            };
            let _ = self.vm.close_dir(dir);
            dir = next.map_err(|e| status_of(&e))?;
        }
        Ok((dir, Name::Given(0)))
    }

    /// The directory at `path` ("" = root).
    fn dir(&self, path: &str) -> Result<RawDirectory, u32> {
        let (parent, last) = self.parent(path)?;
        let last = last.as_str(path);
        if last.is_empty() {
            return Ok(parent);
        }
        let d = self.vm.open_dir(parent, last);
        let _ = self.vm.close_dir(parent);
        d.map_err(|e| status_of(&e))
    }

    fn file(&self, client: usize, id: u32) -> Result<RawFile, u32> {
        match self.files.get(id as usize) {
            Some(Some(o)) if o.client == client => Ok(o.file),
            _ => Err(status::BAD_REQUEST),
        }
    }

    fn handle(&mut self, client: usize, c: &Client, msg: &[u8]) -> [u8; proto::REPLY_SIZE] {
        let r = self.handle_inner(client, c, msg);
        match r {
            Ok((v0, v1)) => proto::reply(status::OK, v0, v1),
            Err(s) => proto::reply(s, 0, 0),
        }
    }

    /// Makes `path`'s directories (and it), what exists already is fine.
    fn make_dirs(&self, path: &str) {
        for (i, _) in path.match_indices('/').map(|(i, s)| (i, s)).chain(core::iter::once((path.len(), ""))) {
            if let Ok((dir, name)) = self.parent(&path[..i]) {
                let _ = self.vm.make_dir_in_dir(dir, name.as_str(&path[..i]));
                let _ = self.vm.close_dir(dir);
            }
        }
    }

    fn handle_inner(&mut self, client: usize, c: &Client, msg: &[u8]) -> Result<(u32, u32), u32> {
        if msg.len() < proto::REQUEST_HEADER {
            return Err(status::BAD_REQUEST);
        }
        let (code, flags, a, b) = (msg[0], msg[1], proto::u32_at(msg, 4), proto::u32_at(msg, 8));
        let raw = core::str::from_utf8(&msg[proto::REQUEST_HEADER..]).map_err(|_| status::BAD_NAME)?;
        let mut buf = [0u8; PATH_MAX];
        let path = normalise(raw, c.own(), &mut buf).ok_or(status::BAD_NAME)?;
        // The own folder is made the first time it is named.
        if raw.trim_start_matches('/').starts_with('~') {
            self.make_dirs(core::str::from_utf8(c.own()).unwrap_or(""));
        }
        let denied = |op: &str| {
            event!(Warn, "fs.denied", app = c.name(), op = op, path = path);
            status::DENIED
        };
        let slice = c.offset;
        // The client's slice; the pool outlives the service and no other
        // reference to this part exists while a request is handled.
        let mem = unsafe { core::slice::from_raw_parts_mut(self.pool.as_mut_ptr().add(slice), proto::SLICE) };
        match code {
            op::OPEN => {
                if !c.may(path.as_bytes(), flags & flag::WRITE != 0) {
                    return Err(denied("open"));
                }
                let mode = match flags {
                    f if f & flag::WRITE == 0 => Mode::ReadOnly,
                    f if f & flag::CREATE != 0 && f & flag::APPEND != 0 => Mode::ReadWriteCreateOrAppend,
                    f if f & flag::CREATE != 0 => Mode::ReadWriteCreateOrTruncate,
                    f if f & flag::APPEND != 0 => Mode::ReadWriteAppend,
                    _ => Mode::ReadWriteTruncate,
                };
                let slot = self.files.iter().position(Option::is_none).ok_or(status::TOO_MANY_OPEN)?;
                let (dir, name) = self.parent(path)?;
                let f = self.vm.open_file_in_dir(dir, name.as_str(path), mode);
                let _ = self.vm.close_dir(dir);
                let file = f.map_err(|e| status_of(&e))?;
                let len = self.vm.file_length(file).unwrap_or(0);
                self.files[slot] = Some(Open { client, file });
                Ok((slot as u32, len))
            }
            op::READ => {
                let f = self.file(client, a)?;
                let n = (b as usize).min(proto::SLICE);
                let mut done = 0;
                while done < n {
                    match self.vm.read(f, &mut mem[done..n]) {
                        Ok(0) => break,
                        Ok(k) => done += k,
                        Err(embedded_sdmmc::Error::EndOfFile) => break,
                        Err(e) => return Err(status_of(&e)),
                    }
                }
                Ok((done as u32, 0))
            }
            op::WRITE => {
                let f = self.file(client, a)?;
                let n = (b as usize).min(proto::SLICE);
                self.vm.write(f, &mem[..n]).map_err(|e| status_of(&e))?;
                Ok((n as u32, 0))
            }
            op::SEEK => {
                let f = self.file(client, a)?;
                self.vm.file_seek_from_start(f, b).map_err(|e| status_of(&e))?;
                Ok((0, 0))
            }
            op::CLOSE => {
                let f = self.file(client, a)?;
                self.files[a as usize] = None;
                self.vm.close_file(f).map_err(|e| status_of(&e))?;
                Ok((0, 0))
            }
            op::LIST_LONG => {
                if !c.may(path.as_bytes(), false) {
                    return Err(denied("list"));
                }
                let d = self.dir(path)?;
                let (mut written, mut total) = (0usize, 0u32);
                let mut storage = [0u8; LFN_BYTES];
                let mut lfn = LfnBuffer::new(&mut storage);
                let r = self.vm.iterate_dir_lfn(d, &mut lfn, |e, long| {
                    // Not the volume label, nor `.` and `..`.
                    if e.attributes.is_volume() {
                        return ControlFlow::Continue(());
                    }
                    total += 1;
                    let o = written * proto::LONG_ENTRY_SIZE;
                    if o + proto::LONG_ENTRY_SIZE <= mem.len() {
                        let rec = &mut mem[o..o + proto::LONG_ENTRY_SIZE];
                        rec.fill(0);
                        match long {
                            Some(l) => {
                                // Cut on a character boundary.
                                let mut n = l.len().min(proto::LONG_NAME);
                                while !l.is_char_boundary(n) {
                                    n -= 1;
                                }
                                rec[..n].copy_from_slice(&l.as_bytes()[..n]);
                            }
                            None => {
                                let mut w = Writer { buf: &mut rec[..proto::LONG_NAME], n: 0 };
                                let _ = core::fmt::write(&mut w, format_args!("{}", e.name));
                            }
                        }
                        rec[proto::LONG_NAME..proto::LONG_NAME + 4].copy_from_slice(&e.size.to_le_bytes());
                        rec[proto::LONG_NAME + 4] = if e.attributes.is_directory() { proto::ATTR_DIR } else { 0 };
                        written += 1;
                    }
                    ControlFlow::Continue(())
                });
                let _ = self.vm.close_dir(d);
                r.map_err(|e| status_of(&e))?;
                Ok((written as u32, total))
            }
            op::LIST => {
                if !c.may(path.as_bytes(), false) {
                    return Err(denied("list"));
                }
                let d = self.dir(path)?;
                let (mut written, mut total) = (0usize, 0u32);
                let r = self.vm.iterate_dir(d, |e| {
                    total += 1;
                    let o = written * proto::ENTRY_SIZE;
                    if o + proto::ENTRY_SIZE <= mem.len() {
                        let rec = &mut mem[o..o + proto::ENTRY_SIZE];
                        rec.fill(0);
                        let mut w = Writer { buf: &mut rec[..proto::ENTRY_NAME], n: 0 };
                        let _ = core::fmt::write(&mut w, format_args!("{}", e.name));
                        rec[proto::ENTRY_NAME..proto::ENTRY_NAME + 4].copy_from_slice(&e.size.to_le_bytes());
                        rec[proto::ENTRY_NAME + 4] = if e.attributes.is_directory() { proto::ATTR_DIR } else { 0 };
                        written += 1;
                    }
                    ControlFlow::Continue(())
                });
                let _ = self.vm.close_dir(d);
                r.map_err(|e| status_of(&e))?;
                Ok((written as u32, total))
            }
            op::DELETE => {
                if !c.may(path.as_bytes(), true) {
                    return Err(denied("delete"));
                }
                let (dir, name) = self.parent(path)?;
                let r = self.vm.delete_entry_in_dir(dir, name.as_str(path));
                let _ = self.vm.close_dir(dir);
                r.map_err(|e| status_of(&e))?;
                Ok((0, 0))
            }
            op::MKDIR => {
                if !c.may(path.as_bytes(), true) && !c.above_allowed(path.as_bytes()) {
                    return Err(denied("mkdir"));
                }
                let (dir, name) = self.parent(path)?;
                let r = self.vm.make_dir_in_dir(dir, name.as_str(path));
                let _ = self.vm.close_dir(dir);
                r.map_err(|e| status_of(&e))?;
                Ok((0, 0))
            }
            _ => Err(status::BAD_REQUEST),
        }
    }

    /// Closes what a departing client left open.
    fn drop_client(&mut self, client: usize) {
        for o in self.files.iter_mut() {
            if o.as_ref().is_some_and(|o| o.client == client) {
                let f = o.take().unwrap().file;
                let _ = self.vm.close_file(f);
            }
        }
    }
}

/// `core::fmt` into a fixed buffer (the 8.3 name).
struct Writer<'a> {
    buf: &'a mut [u8],
    n: usize,
}

impl core::fmt::Write for Writer<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let k = s.len().min(self.buf.len() - self.n);
        self.buf[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

fn main(_: usize, _: usize) -> usize {
    match run() {
        Ok(()) => 0,
        Err(check) => {
            event!(Error, "fs.failed", step = check);
            check as usize
        }
    }
}

fn run() -> Result<(), u32> {
    let h = Handle::open(sd::NAME, rights::READ | rights::WRITE | rights::CTL).map_err(|_| 1u32)?;
    let sectors = h.ctl(sd::SECTORS, 0).map_err(|_| 2u32)? as u32;
    let vm: Vm = VolumeManager::new_with_limits(Card { h, sectors, buf: RefCell::new([0; sd::MAX_IO]) }, FixedClock, 5000);
    let volume = vm.open_raw_volume(VolumeIdx(0)).map_err(|e| {
        event!(Error, "fs.no_volume", status = status_of(&e));
        3u32
    })?;
    let pool_shm = Shm::alloc(CLIENTS * proto::SLICE).map_err(|_| 4u32)?;
    let pool = pool_shm.map().map_err(|_| 5u32)?;
    let l = Listener::open(proto::SERVICE).map_err(|_| 6u32)?;
    event!(Info, "fs.ready", sectors = sectors, clients = CLIENTS as u32);

    let mut svc = Service { vm: &vm, volume, pool, files: [const { None }; FILES] };
    let mut clients: [Option<Client>; CLIENTS] = [const { None }; CLIENTS];
    let mut msg = [0u8; chan::MSG_MAX];
    loop {
        let mut hs = [0u32; 1 + CLIENTS];
        let mut who = [0usize; 1 + CLIENTS];
        hs[0] = l.0.raw();
        let mut n = 1;
        for (i, c) in clients.iter().enumerate() {
            if let Some(c) = c {
                hs[n] = c.chan.0.raw();
                who[n] = i;
                n += 1;
            }
        }
        let ev = wait_raw(&hs[..n], FOREVER).map_err(|_| 7u32)?;
        if ev.index == 0 {
            let c = l.accept().map_err(|_| 8u32)?;
            let Some(i) = clients.iter().position(Option::is_none) else {
                drop(c); // full: the client sees PEER_CLOSED
                continue;
            };
            let offset = i * proto::SLICE;
            svc.pool[offset..offset + proto::SLICE].fill(0);
            if pool_shm.grant(&c, offset, proto::SLICE).is_err() {
                continue;
            }
            let client = Client::new(c, offset);
            event!(Info, "fs.client", index = i as u32, app = client.name());
            clients[i] = Some(client);
            continue;
        }
        let i = who[ev.index];
        if ev.kind == chan::PEER_CLOSED {
            svc.drop_client(i);
            clients[i] = None;
            continue;
        }
        let c = clients[i].as_ref().unwrap();
        while let Ok(len) = c.chan.0.read(&mut msg) {
            let reply = svc.handle(i, c, &msg[..len]);
            let _ = c.chan.send(&reply);
        }
    }
}
