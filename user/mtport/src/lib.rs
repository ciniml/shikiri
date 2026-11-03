#![no_std]

//! simple-matter on tab5-os (doc/49): what the core asks of the platform.
//!
//! - [`KernelRng`]: `crypto::Rng` over the kernel's `random` (doc/47; the
//!   SAR ADC entropy source and the health tests). Needs `--permit random`.
//! - [`FileKvs`]: `kvs::Kvs` over `svc:fs` (the SD card). Needs `--permit
//!   svc:fs`. A failed file operation is `Error::InvalidState` (the core
//!   has no I/O error), a key or value too long `Error::NoSpace`.

use simple_matter::Error as MtError;
use simple_matter::crypto::Rng;
use simple_matter::kvs::Kvs;
use t5_abi::fs::{flag, status};
use t5_abi::store::crc32;
use t5_rt::{Files, FsError};

pub struct KernelRng;

impl Rng for KernelRng {
    fn fill_bytes(&mut self, dest: &mut [u8]) -> simple_matter::error::Result<()> {
        t5_rt::random_fill(dest).map_err(|_| MtError::Crypto)
    }
}

/// The directory of the store's files.
pub const DIR: &str = "T5OS/MATTER";
/// Longest key and value.
pub const KEY_MAX: usize = 32;
pub const VALUE_MAX: usize = 4096;

const MAGIC: &[u8; 4] = b"T5KV";
/// MAGIC, generation, key length, value length, CRC-32 of what follows.
const HEADER: usize = 4 + 4 + 2 + 2 + 4;
const RECORD_MAX: usize = HEADER + KEY_MAX + VALUE_MAX;

/// Each key in two files, `T5OS/MATTER/<crc32 of the key>.KVA` and `.KVB`
/// (8.3 names; FAT has no rename): a write goes to the file that does not
/// hold the newest good record, with the next generation, so a write cut
/// short leaves the previous value readable. A record carries its key
/// (names are hashes) and a CRC.
pub struct FileKvs {
    fs: Files,
    buf: [u8; RECORD_MAX],
}

#[derive(Clone, Copy)]
struct Slot {
    generation: u32,
    len: usize,
}

impl FileKvs {
    /// Connects to `svc:fs` and makes the directory.
    pub fn open() -> Result<FileKvs, FsError> {
        let fs = Files::connect()?;
        for dir in ["T5OS", DIR] {
            match fs.mkdir(dir) {
                Ok(()) | Err(FsError::Status(status::EXISTS)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(FileKvs { fs, buf: [0; RECORD_MAX] })
    }

    fn path(key: &[u8], ab: u8, out: &mut [u8; 32]) -> usize {
        let hex = b"0123456789ABCDEF";
        let h = crc32(key);
        let mut n = 0;
        for &b in DIR.as_bytes().iter().chain(b"/") {
            out[n] = b;
            n += 1;
        }
        for i in (0..8).rev() {
            out[n] = hex[((h >> (4 * i)) & 0xF) as usize];
            n += 1;
        }
        for &b in b".KV".iter().chain(core::slice::from_ref(&ab)) {
            out[n] = b;
            n += 1;
        }
        n
    }

    /// Reads file A or B into `self.buf`; its generation and value length
    /// if it holds a good record of `key`.
    fn read_slot(&mut self, key: &[u8], ab: u8) -> Result<Option<Slot>, MtError> {
        let mut p = [0u8; 32];
        let n = Self::path(key, ab, &mut p);
        let path = core::str::from_utf8(&p[..n]).unwrap_or("");
        let f = match self.fs.open(path, flag::READ) {
            Ok(f) => f,
            Err(FsError::Status(status::NOT_FOUND)) => return Ok(None),
            Err(_) => return Err(MtError::InvalidState),
        };
        let got = self.fs.read(f, &mut self.buf);
        let _ = self.fs.close(f);
        let got = got.map_err(|_| MtError::InvalidState)?;
        let b = &self.buf[..got];
        if got < HEADER || &b[..4] != MAGIC {
            return Ok(None);
        }
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let key_len = u16::from_le_bytes([b[8], b[9]]) as usize;
        let len = u16::from_le_bytes([b[10], b[11]]) as usize;
        if HEADER + key_len + len != got || crc32(&b[HEADER..]) != u32_at(12) || &b[HEADER..HEADER + key_len] != key {
            return Ok(None);
        }
        Ok(Some(Slot { generation: u32_at(4), len }))
    }

    /// The newest good slot of `key`: (A or B, its slot).
    fn newest(&mut self, key: &[u8]) -> Result<Option<(u8, Slot)>, MtError> {
        let a = self.read_slot(key, b'A')?;
        let b = self.read_slot(key, b'B')?;
        Ok(match (a, b) {
            (Some(a), Some(b)) if b.generation.wrapping_sub(a.generation) as i32 > 0 => Some((b'B', b)),
            (Some(a), _) => Some((b'A', a)),
            (None, Some(b)) => Some((b'B', b)),
            (None, None) => None,
        })
    }
}

impl Kvs for FileKvs {
    fn get(&mut self, key: &[u8], out: &mut [u8]) -> simple_matter::error::Result<Option<usize>> {
        if key.len() > KEY_MAX {
            return Err(MtError::NoSpace);
        }
        let Some((ab, slot)) = self.newest(key)? else { return Ok(None) };
        if slot.len > out.len() {
            return Err(MtError::NoSpace);
        }
        // `newest` read B last: read the winner again if it was A and B
        // also exists in the buffer.
        if ab == b'A' {
            self.read_slot(key, b'A')?;
        }
        let at = HEADER + key.len();
        out[..slot.len].copy_from_slice(&self.buf[at..at + slot.len]);
        Ok(Some(slot.len))
    }

    fn set(&mut self, key: &[u8], value: &[u8]) -> simple_matter::error::Result<()> {
        if key.len() > KEY_MAX || value.len() > VALUE_MAX {
            return Err(MtError::NoSpace);
        }
        let (target, generation) = match self.newest(key)? {
            Some((b'A', s)) => (b'B', s.generation.wrapping_add(1)),
            Some((_, s)) => (b'A', s.generation.wrapping_add(1)),
            None => (b'A', 1),
        };
        let b = &mut self.buf;
        b[..4].copy_from_slice(MAGIC);
        b[4..8].copy_from_slice(&generation.to_le_bytes());
        b[8..10].copy_from_slice(&(key.len() as u16).to_le_bytes());
        b[10..12].copy_from_slice(&(value.len() as u16).to_le_bytes());
        b[HEADER..HEADER + key.len()].copy_from_slice(key);
        b[HEADER + key.len()..HEADER + key.len() + value.len()].copy_from_slice(value);
        let end = HEADER + key.len() + value.len();
        let crc = crc32(&b[HEADER..end]);
        b[12..16].copy_from_slice(&crc.to_le_bytes());
        let mut p = [0u8; 32];
        let n = Self::path(key, target, &mut p);
        let path = core::str::from_utf8(&p[..n]).unwrap_or("");
        let f = self.fs.open(path, flag::WRITE | flag::CREATE).map_err(|_| MtError::InvalidState)?;
        let r = self.fs.write(f, &self.buf[..end]);
        let c = self.fs.close(f);
        r.and(c).map_err(|_| MtError::InvalidState)
    }

    fn remove(&mut self, key: &[u8]) -> simple_matter::error::Result<()> {
        for ab in [b'A', b'B'] {
            let mut p = [0u8; 32];
            let n = Self::path(key, ab, &mut p);
            let path = core::str::from_utf8(&p[..n]).unwrap_or("");
            match self.fs.delete(path) {
                Ok(()) | Err(FsError::Status(status::NOT_FOUND)) => {}
                Err(_) => return Err(MtError::InvalidState),
            }
        }
        Ok(())
    }
}
