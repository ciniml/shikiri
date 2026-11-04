//! The settings service (doc/50, doc/51): named values kept by the `cfg`
//! process. A client connects to `svc:cfg`; its grants (doc/52) say which
//! keys it may read (`svc:cfg/r:PREFIX`), change (`w:PREFIX`) and read the
//! secret values of (`secret:PREFIX`, system key only); an empty PREFIX is
//! every key. Other keys answer `ERR DENIED` (and `LIST` skips them). One
//! request at a time; each gets one answer (`LIST`: one entry per
//! request).
//!
//! | request | answer |
//! |---|---|
//! | `[GET, key_len, key…]` | `[VALUE, flags, value…]` (a secret without `secret:`: no value) or `[NONE]` |
//! | `[SET, flags, key_len, key…, value…]` | `[OK]` or `[ERR, code]` |
//! | `[DEL, key_len, key…]` | `[OK]` (also when there was none) or `[ERR, code]` |
//! | `[LIST, index]` | `[ENTRY, flags, key_len, key…, value…]` (secrets as for `GET`) or `[END]` |
//!
//! A key is 1 to `KEY_MAX` bytes of `a-z 0-9 . _ -`; a value 0 to
//! `VALUE_MAX` bytes. A secret stays secret until it is deleted.
//!
//! The stored table (`Table::image`): `MAGIC | generation: u32 | length:
//! u16 | 0: u16 | crc32 of the records: u32`, then the records `[flags,
//! key_len, value_len, key…, value…]`. Two copies, A and B: a save goes to
//! the copy not holding the newest good table, so one cut short leaves the
//! last. `cfg` keeps them in flash (`t5_abi::cfgflash`, doc/51 §2.2), a
//! secret's value sealed (`SEALED`: nonce, ciphertext, tag).

use crate::store::crc32;

pub const SVC: &str = "cfg";
pub const KEY_MAX: usize = 32;
/// The longest value a client gives or gets.
pub const VALUE_MAX: usize = 128;
/// What sealing adds to a value: the nonce and the tag (AES-256-GCM).
pub const SEAL_EXTRA: usize = 12 + 16;
/// The longest value the table holds.
pub const STORED_MAX: usize = VALUE_MAX + SEAL_EXTRA;
/// The records' room.
pub const DATA_MAX: usize = 4096 - HEADER;
pub const HEADER: usize = 16;
pub const FILE_MAX: usize = HEADER + DATA_MAX;
pub const MAGIC: [u8; 4] = *b"T5CF";
/// The longest message either way.
pub const MSG_MAX: usize = 3 + KEY_MAX + VALUE_MAX;

// Requests.
pub const GET: u8 = 1;
pub const SET: u8 = 2;
pub const DEL: u8 = 3;
pub const LIST: u8 = 4;
// Answers.
pub const OK: u8 = 0x81;
pub const VALUE: u8 = 0x82;
pub const NONE: u8 = 0x83;
pub const ENTRY: u8 = 0x84;
pub const END: u8 = 0x85;
pub const ERR: u8 = 0x86;

/// Flags of a value.
pub const SECRET: u8 = 1;
/// In the table only (clients never see it): the value is sealed.
pub const SEALED: u8 = 2;

/// `ERR` codes.
pub mod err {
    /// A malformed request, or a key with other characters.
    pub const BAD: u8 = 1;
    /// The key or the value is too long.
    pub const TOO_LONG: u8 = 2;
    /// No room left in the table.
    pub const FULL: u8 = 3;
    /// Changed, but not saved (lost when `cfg` restarts).
    pub const NOT_SAVED: u8 = 4;
    /// The client's grants do not cover the key (doc/52).
    pub const DENIED: u8 = 5;
}

pub fn valid_key(key: &[u8]) -> bool {
    !key.is_empty() && key.len() <= KEY_MAX && key.iter().all(|&c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

/// The values, as records one after another.
pub struct Table {
    data: [u8; DATA_MAX],
    len: usize,
}

impl Default for Table {
    fn default() -> Table {
        Table::new()
    }
}

impl Table {
    pub const fn new() -> Table {
        Table { data: [0; DATA_MAX], len: 0 }
    }

    /// The records: (start, flags, key, value).
    fn records(&self) -> impl Iterator<Item = (usize, u8, &[u8], &[u8])> {
        let d = &self.data[..self.len];
        let mut at = 0;
        core::iter::from_fn(move || {
            let h = d.get(at..at + 3)?;
            let (k, v) = (h[1] as usize, h[2] as usize);
            let key = d.get(at + 3..at + 3 + k)?;
            let value = d.get(at + 3 + k..at + 3 + k + v)?;
            let start = at;
            at += 3 + k + v;
            Some((start, h[0], key, value))
        })
    }

    /// (flags, key, value) of each value.
    pub fn iter(&self) -> impl Iterator<Item = (u8, &[u8], &[u8])> {
        self.records().map(|(_, f, k, v)| (f, k, v))
    }

    pub fn get(&self, key: &[u8]) -> Option<(u8, &[u8])> {
        self.iter().find(|(_, k, _)| *k == key).map(|(f, _, v)| (f, v))
    }

    /// Sets `key`; a secret stays one. Err: an `err` code.
    pub fn set(&mut self, key: &[u8], flags: u8, value: &[u8]) -> Result<(), u8> {
        if key.len() > KEY_MAX || value.len() > STORED_MAX {
            return Err(err::TOO_LONG);
        }
        if !valid_key(key) {
            return Err(err::BAD);
        }
        let old = self.get(key).map(|(f, v)| (f, v.len()));
        let freed = old.map_or(0, |(_, n)| 3 + key.len() + n);
        if self.len - freed + 3 + key.len() + value.len() > DATA_MAX {
            return Err(err::FULL);
        }
        let flags = flags | old.map_or(0, |(f, _)| f & SECRET);
        let flags = if flags & SECRET == 0 { flags & !SEALED } else { flags };
        self.remove(key);
        let n = self.len;
        self.data[n] = flags;
        self.data[n + 1] = key.len() as u8;
        self.data[n + 2] = value.len() as u8;
        self.data[n + 3..n + 3 + key.len()].copy_from_slice(key);
        self.data[n + 3 + key.len()..n + 3 + key.len() + value.len()].copy_from_slice(value);
        self.len = n + 3 + key.len() + value.len();
        Ok(())
    }

    /// True if there was one.
    pub fn remove(&mut self, key: &[u8]) -> bool {
        let Some((start, _, k, v)) = self.records().find(|(_, _, k, _)| *k == key) else { return false };
        let end = start + 3 + k.len() + v.len();
        self.data.copy_within(end..self.len, start);
        self.len -= end - start;
        true
    }

    /// The length of the stored table that begins with `head` (at least
    /// `HEADER` bytes), if it is one.
    pub fn image_len(head: &[u8]) -> Option<usize> {
        let len = u16::from_le_bytes([*head.get(8)?, *head.get(9)?]) as usize;
        (head.get(..4)? == MAGIC && len <= DATA_MAX).then_some(HEADER + len)
    }

    /// The file's bytes (`FILE_MAX` at most) with `generation`.
    pub fn image(&self, generation: u32, out: &mut [u8; FILE_MAX]) -> usize {
        let body = &self.data[..self.len];
        out[..4].copy_from_slice(&MAGIC);
        out[4..8].copy_from_slice(&generation.to_le_bytes());
        out[8..10].copy_from_slice(&(self.len as u16).to_le_bytes());
        out[10..12].fill(0);
        out[12..16].copy_from_slice(&crc32(body).to_le_bytes());
        out[HEADER..HEADER + self.len].copy_from_slice(body);
        HEADER + self.len
    }

    /// A file's table and generation, if it is whole and every record good.
    pub fn load(file: &[u8]) -> Option<(Table, u32)> {
        let h = file.get(..HEADER)?;
        let u32_at = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
        let len = u16::from_le_bytes([h[8], h[9]]) as usize;
        if h[..4] != MAGIC || len > DATA_MAX || file.len() != HEADER + len {
            return None;
        }
        let body = &file[HEADER..];
        if crc32(body) != u32_at(12) {
            return None;
        }
        let mut t = Table::new();
        t.data[..len].copy_from_slice(body);
        t.len = len;
        let mut used = 0;
        for (start, _, k, v) in t.records() {
            if !valid_key(k) || v.len() > STORED_MAX {
                return None;
            }
            used = start + 3 + k.len() + v.len();
        }
        (used == len).then_some((t, u32_at(4)))
    }
}

/// Of two copies' generations, whether `b` is the newer.
pub fn newer(a: u32, b: u32) -> bool {
    b.wrapping_sub(a) as i32 > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_remove() {
        let mut t = Table::new();
        t.set(b"wifi.ssid", 0, b"net").unwrap();
        t.set(b"wifi.psk", SECRET, b"pass").unwrap();
        t.set(b"wifi.ssid", 0, b"other").unwrap();
        assert_eq!(t.get(b"wifi.ssid"), Some((0, &b"other"[..])));
        // A secret stays one when set again without the flag.
        t.set(b"wifi.psk", 0, b"pass2").unwrap();
        assert_eq!(t.get(b"wifi.psk"), Some((SECRET, &b"pass2"[..])));
        assert_eq!(t.iter().count(), 2);
        assert!(t.remove(b"wifi.ssid"));
        assert!(!t.remove(b"wifi.ssid"));
        assert_eq!(t.get(b"wifi.ssid"), None);
        assert_eq!(t.get(b"wifi.psk").unwrap().1, b"pass2");
    }

    #[test]
    fn limits() {
        let mut t = Table::new();
        assert_eq!(t.set(b"Wifi", 0, b""), Err(err::BAD));
        assert_eq!(t.set(b"", 0, b""), Err(err::BAD));
        assert_eq!(t.set(&[b'a'; KEY_MAX + 1], 0, b""), Err(err::TOO_LONG));
        assert_eq!(t.set(b"a", 0, &[0; STORED_MAX + 1]), Err(err::TOO_LONG));
        let mut i = 0u32;
        let r = loop {
            let mut k = *b"k.0000";
            for (j, d) in (0..4).zip([i / 1000, i / 100 % 10, i / 10 % 10, i % 10]) {
                k[2 + j] = b'0' + d as u8;
            }
            if let Err(e) = t.set(&k, 0, &[7; VALUE_MAX]) {
                break e;
            }
            i += 1;
        };
        assert_eq!(r, err::FULL);
        assert_eq!(t.iter().count(), i as usize);
        // Replacing a value with one as long still fits.
        t.set(b"k.0000", 0, &[8; VALUE_MAX]).unwrap();
    }

    #[test]
    fn file_round_trip_and_damage() {
        let mut t = Table::new();
        t.set(b"a", 0, b"1").unwrap();
        t.set(b"b.c", SECRET, b"xyz").unwrap();
        let mut f = [0u8; FILE_MAX];
        let n = t.image(41, &mut f);
        assert_eq!(Table::image_len(&f[..HEADER]), Some(n));
        let (u, generation) = Table::load(&f[..n]).unwrap();
        assert_eq!(generation, 41);
        assert_eq!(u.get(b"b.c"), Some((SECRET, &b"xyz"[..])));
        assert!(Table::load(&f[..n - 1]).is_none());
        f[HEADER + 1] ^= 1;
        assert!(Table::load(&f[..n]).is_none());
        assert!(newer(41, 42) && !newer(42, 41) && newer(u32::MAX, 0));
    }
}
