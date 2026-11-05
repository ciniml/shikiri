//! Who each process is (doc/52 §3.2): its app name, the pid that started it,
//! and its signed Permissions TLV, copied when it starts (the store's image
//! may be replaced while it runs). The kernel checks `open` against it, and
//! servers read a client's grants on their service from it. A process may
//! drop entries for good (`permit_drop`, doc/56 §4).

use t5_abi::{chan, t5a};

/// As many as processes run at once.
const ENTRIES: usize = crate::sched::MAX_PROCS + 2;

pub struct Ident {
    pub pid: u32,
    /// The pid that started it (`proc:spawn`); 0 for the kernel and the host.
    pub parent: u32,
    /// Signed with a system key: no per-process limits (doc/56 §2).
    pub system: bool,
    name: [u8; t5a::MAX_NAME],
    name_len: usize,
    perms: [u8; chan::GRANTS_MAX],
    perms_len: usize,
}

impl Ident {
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }

    pub fn permissions(&self) -> &[u8] {
        &self.perms[..self.perms_len]
    }
}

static mut TABLE: [Option<Ident>; ENTRIES] = [const { None }; ENTRIES];

fn table() -> &'static mut [Option<Ident>; ENTRIES] {
    // Under the kernel lock.
    unsafe { &mut *core::ptr::addr_of_mut!(TABLE) }
}

/// Records process `pid`. Err if its permissions are longer than
/// `GRANTS_MAX` or the table is full (both checked before it starts).
pub fn add(pid: u32, name: &[u8], permissions: &[u8], system: bool) -> Result<(), &'static str> {
    if permissions.len() > chan::GRANTS_MAX {
        return Err("permissions too long");
    }
    let t = table();
    let slot = t.iter_mut().find(|e| e.as_ref().is_none_or(|e| e.pid == pid)).ok_or("too many processes")?;
    let mut e = Ident { pid, parent: 0, system, name: [0; t5a::MAX_NAME], name_len: name.len().min(t5a::MAX_NAME), perms: [0; chan::GRANTS_MAX], perms_len: permissions.len() };
    e.name[..e.name_len].copy_from_slice(&name[..e.name_len]);
    e.perms[..permissions.len()].copy_from_slice(permissions);
    *slot = Some(e);
    Ok(())
}

pub fn remove(pid: u32) {
    for e in table().iter_mut() {
        if e.as_ref().is_some_and(|e| e.pid == pid) {
            *e = None;
        }
    }
}

pub fn get(pid: u32) -> Option<&'static Ident> {
    table().iter().flatten().find(|e| e.pid == pid)
}

/// Removes `entry` and every `entry/GRANT` from the permissions of `pid`
/// (doc/56 §4); how many were removed.
pub fn drop_permit(pid: u32, entry: &[u8]) -> usize {
    let Some(e) = table().iter_mut().flatten().find(|e| e.pid == pid) else { return 0 };
    let mut kept = [0u8; chan::GRANTS_MAX];
    let (mut len, mut removed) = (0, 0);
    for p in e.perms[..e.perms_len].split(|&b| b == 0).filter(|p| !p.is_empty()) {
        // A wider entry stays: `svc:fs` is not "all but `rw:X`".
        if p == entry || (p.len() > entry.len() && p.starts_with(entry) && p[entry.len()] == b'/') {
            removed += 1;
            continue;
        }
        let sep = (len > 0) as usize;
        if sep == 1 {
            kept[len] = 0;
        }
        kept[len + sep..len + sep + p.len()].copy_from_slice(p);
        len += sep + p.len();
    }
    e.perms = kept;
    e.perms_len = len;
    removed
}

pub fn set_parent(pid: u32, parent: u32) {
    if let Some(e) = table().iter_mut().flatten().find(|e| e.pid == pid) {
        e.parent = parent;
    }
}

/// Whether `ancestor` started `pid`, or started a process that did.
pub fn descends_from(pid: u32, ancestor: u32) -> bool {
    let mut p = pid;
    for _ in 0..ENTRIES {
        match get(p) {
            Some(e) if e.parent == ancestor && ancestor != 0 => return true,
            Some(e) if e.parent != 0 => p = e.parent,
            _ => return false,
        }
    }
    false
}

/// The grants of `pid` on service `name`, NUL-separated, into `out`;
/// their length, or `None` if `out` is too short.
pub fn service_grants(pid: u32, name: &[u8], out: &mut [u8]) -> Option<usize> {
    let Some(e) = get(pid) else { return Some(0) };
    let mut prefix = [0u8; 4 + t5a::MAX_NAME];
    let p = b"svc:";
    prefix[..4].copy_from_slice(p);
    let n = name.len().min(t5a::MAX_NAME);
    prefix[4..4 + n].copy_from_slice(&name[..n]);
    let mut len = 0;
    for g in t5a::grants(e.permissions(), &prefix[..4 + n]) {
        let sep = (len > 0) as usize;
        let end = len + sep + g.len();
        if end > out.len() {
            return None;
        }
        if sep == 1 {
            out[len] = 0;
        }
        out[len + sep..end].copy_from_slice(g);
        len = end;
    }
    Some(len)
}
