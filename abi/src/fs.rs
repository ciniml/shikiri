//! The file service's protocol (M7c, doc/18 §5). A client connects to
//! `svc:fs`; the service grants it `SLICE` bytes of shared memory, through
//! which file data and directory listings travel. Requests and replies are
//! channel messages:
//!
//! - request: `u8 op | u8 flags | u16 0 | u32 a | u32 b | path (UTF-8, '/'-separated, 8.3 names)`
//! - reply: `u32 status | u32 v0 | u32 v1`
//!
//! | op | a | b | path | v0 / v1 |
//! |---|---|---|---|---|
//! | OPEN | – | – | file | file id / size |
//! | READ | file id | bytes | – | bytes read into the slice (0 = end) |
//! | WRITE | file id | bytes | – | bytes written from the slice |
//! | SEEK | file id | offset | – | – |
//! | CLOSE | file id | – | – | – |
//! | LIST | – | – | directory ("" = root) | entries written (`ENTRY_SIZE` each) / entries in all |
//! | DELETE | – | – | file | – |
//! | MKDIR | – | – | directory | – |

pub const SERVICE: &str = "fs";
pub const SLICE: usize = 64 << 10;
pub const REQUEST_HEADER: usize = 12;
pub const REPLY_SIZE: usize = 12;

pub mod op {
    pub const OPEN: u8 = 1;
    pub const READ: u8 = 2;
    pub const WRITE: u8 = 3;
    pub const SEEK: u8 = 4;
    pub const CLOSE: u8 = 5;
    pub const LIST: u8 = 6;
    pub const DELETE: u8 = 7;
    pub const MKDIR: u8 = 8;
}

/// OPEN flags.
pub mod flag {
    pub const READ: u8 = 1;
    pub const WRITE: u8 = 2;
    /// With WRITE: create the file if missing, else empty it.
    pub const CREATE: u8 = 4;
    /// With WRITE: write at the end of an existing file.
    pub const APPEND: u8 = 8;
}

/// Reply status.
pub mod status {
    pub const OK: u32 = 0;
    pub const NOT_FOUND: u32 = 1;
    pub const EXISTS: u32 = 2;
    pub const BAD_NAME: u32 = 3;
    pub const TOO_MANY_OPEN: u32 = 4;
    pub const IO: u32 = 5;
    pub const BAD_REQUEST: u32 = 6;
    pub const NOT_A_FILE: u32 = 7;
    pub const FULL: u32 = 8;
    pub const OTHER: u32 = 9;
}

/// One LIST entry in the slice: name (NUL-padded "NAME.EXT"), size,
/// attributes (`ATTR_DIR`).
pub const ENTRY_SIZE: usize = 20;
pub const ENTRY_NAME: usize = 13;
pub const ATTR_DIR: u8 = 0x10;

pub fn request(op: u8, flags: u8, a: u32, b: u32, path: &str, out: &mut [u8; crate::chan::MSG_MAX]) -> Option<usize> {
    let n = REQUEST_HEADER + path.len();
    if n > out.len() {
        return None;
    }
    out[0] = op;
    out[1] = flags;
    out[2..4].fill(0);
    out[4..8].copy_from_slice(&a.to_le_bytes());
    out[8..12].copy_from_slice(&b.to_le_bytes());
    out[12..n].copy_from_slice(path.as_bytes());
    Some(n)
}

pub fn reply(status: u32, v0: u32, v1: u32) -> [u8; REPLY_SIZE] {
    let mut r = [0u8; REPLY_SIZE];
    r[0..4].copy_from_slice(&status.to_le_bytes());
    r[4..8].copy_from_slice(&v0.to_le_bytes());
    r[8..12].copy_from_slice(&v1.to_le_bytes());
    r
}

pub fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
