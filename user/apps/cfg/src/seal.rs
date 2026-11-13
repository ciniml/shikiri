// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! Secrets sealed with AES-256-GCM (doc/51 §2.3): a value becomes `nonce
//! (12) | ciphertext | tag (16)`, its key name the associated data, so a
//! sealed value moved to another key does not open. The key is 32 random
//! bytes kept in the settings' flash, in two copies; it protects copies of
//! the table that leave the flash (and is the place an eFuse-derived key
//! goes later), not the flash itself.

use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, Tag};
use t5_rt::cfg::{SEAL_EXTRA, STORED_MAX, VALUE_MAX};
use t5_rt::store::crc32;

/// `MAGIC | version: u32 | key (32) | crc32 of what comes before`.
const MAGIC: &[u8; 4] = b"T5CK";
const VERSION: u32 = 1;
pub const RECORD: usize = 4 + 4 + 32 + 4;

pub struct Key(Aes256Gcm);

impl Key {
    /// The key a record holds, if it is whole.
    pub fn from_record(r: &[u8]) -> Option<Key> {
        let r = r.get(..RECORD)?;
        let ok = &r[..4] == MAGIC
            && u32::from_le_bytes([r[4], r[5], r[6], r[7]]) == VERSION
            && crc32(&r[..40]).to_le_bytes() == r[40..44];
        if !ok {
            return None;
        }
        Aes256Gcm::new_from_slice(&r[8..40]).ok().map(Key)
    }

    /// A new key and its record.
    pub fn create() -> Option<(Key, [u8; RECORD])> {
        let mut r = [0u8; RECORD];
        r[..4].copy_from_slice(MAGIC);
        r[4..8].copy_from_slice(&VERSION.to_le_bytes());
        t5_rt::random_fill(&mut r[8..40]).ok()?;
        let crc = crc32(&r[..40]);
        r[40..44].copy_from_slice(&crc.to_le_bytes());
        Some((Key::from_record(&r)?, r))
    }

    /// `value` sealed into `out`; its length.
    pub fn seal(&self, name: &[u8], value: &[u8], out: &mut [u8; STORED_MAX]) -> Option<usize> {
        let n = value.len();
        if n > VALUE_MAX {
            return None;
        }
        t5_rt::random_fill(&mut out[..12]).ok()?;
        out[12..12 + n].copy_from_slice(value);
        let (nonce, rest) = out.split_at_mut(12);
        let tag = self.0.encrypt_in_place_detached(Nonce::from_slice(nonce), name, &mut rest[..n]).ok()?;
        out[12 + n..12 + n + 16].copy_from_slice(&tag);
        Some(n + SEAL_EXTRA)
    }

    /// A sealed value opened into `out`; its length.
    pub fn open(&self, name: &[u8], sealed: &[u8], out: &mut [u8; VALUE_MAX]) -> Option<usize> {
        let n = sealed.len().checked_sub(SEAL_EXTRA).filter(|&n| n <= VALUE_MAX)?;
        out[..n].copy_from_slice(&sealed[12..12 + n]);
        let tag = Tag::from_slice(&sealed[12 + n..]);
        self.0.decrypt_in_place_detached(Nonce::from_slice(&sealed[..12]), name, &mut out[..n], tag).ok()?;
        Some(n)
    }
}
