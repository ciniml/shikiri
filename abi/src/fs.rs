// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The file service's protocol (M7c, doc/18 §5). A client connects to
//! `svc:fs`; the service grants it `SLICE` bytes of shared memory, through
//! which file data and directory listings travel. Requests and replies are
//! channel messages:
//!
//! - request: `u8 op | u8 flags | u16 0 | u32 a | u32 b | path (UTF-8, '/'-separated)`
//!
//! A path's parts are 8.3 names or long names (doc/63), matched without
//! regard to (ASCII) case. A new file or directory whose name is not an
//! upper case 8.3 name is made with it as its long name (case kept) beside
//! an 8.3 alias.
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
//! | LIST_LONG | – | – | directory | entries written (`LONG_ENTRY_SIZE` each) / entries in all |
//! | DELETE | – | – | file | – |
//! | MKDIR | – | – | directory | – |
//!
//! Access (doc/52 §3.3): a client may use its own folder `T5OS/APPS/<NAME>`
//! (its app name, upper case, cut to 8 characters; written `~` in a path,
//! made on first use), and what its grants say: `svc:fs/r:PATH` (read,
//! list) and `svc:fs/rw:PATH` (also write, delete, make directories), PATH
//! and everything under it (an empty PATH: the whole card). Making the
//! directories above an allowed place is allowed. Paths are compared
//! without regard to case; `.` and `..` are refused. Anything else is `DENIED`.

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
    /// LIST with long names (doc/63). An older service answers
    /// `BAD_REQUEST`: use LIST.
    pub const LIST_LONG: u8 = 9;
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
    /// The client's grants do not cover the path (doc/52).
    pub const DENIED: u32 = 10;
}

/// One LIST entry in the slice: name (NUL-padded "NAME.EXT"), size,
/// attributes (`ATTR_DIR`).
pub const ENTRY_SIZE: usize = 20;
pub const ENTRY_NAME: usize = 13;
pub const ATTR_DIR: u8 = 0x10;

/// One LIST_LONG entry: name (NUL-padded UTF-8: the long name if there is
/// one, cut at `LONG_NAME` bytes on a character boundary, else "NAME.EXT"),
/// u32 size, attributes, 3 bytes 0.
pub const LONG_ENTRY_SIZE: usize = 128;
pub const LONG_NAME: usize = 120;

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
