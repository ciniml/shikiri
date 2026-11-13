// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! Curve25519 keys: the machine key (the Noise channel to the control
//! server), the node key (WireGuard and DERP), the disco key (path
//! discovery). Written as `prefix:` and 64 hex digits (`nodekey:…`).

use alloc::string::String;
use rand_core::CryptoRngCore;

pub const LEN: usize = 32;

/// Bytes of the kernel's `random` per seed: it claims 16 bits of
/// min-entropy per 32-bit word (doc/47 §5), so 256 bits in these.
pub const RAW_SEED: usize = 64;

/// The random generator's seed from `RAW_SEED` bytes of the kernel's
/// `random`, conditioned by BLAKE2s (doc/47 §8, doc/71 §4.6): its raw
/// words are XORs of registers and a timer, biased bit by bit.
pub fn seed(raw: &[u8; RAW_SEED]) -> [u8; 32] {
    use blake2::{Blake2s256, Digest};
    let mut d = Blake2s256::new();
    d.update(b"tab5-os ts seed v1");
    d.update(raw);
    d.finalize().into()
}

#[derive(Clone)]
pub struct KeyPair {
    pub private: [u8; LEN],
    pub public: [u8; LEN],
}

impl KeyPair {
    pub fn from_private(private: [u8; LEN]) -> KeyPair {
        let private = clamp(private);
        KeyPair { private, public: x25519_dalek::x25519(private, x25519_dalek::X25519_BASEPOINT_BYTES) }
    }

    pub fn generate(rng: &mut impl CryptoRngCore) -> KeyPair {
        let mut k = [0u8; LEN];
        rng.fill_bytes(&mut k);
        KeyPair::from_private(k)
    }

    /// The shared secret with `public`.
    pub fn dh(&self, public: &[u8; LEN]) -> [u8; LEN] {
        x25519_dalek::x25519(self.private, *public)
    }
}

/// RFC 7748's clamping (x25519 does it too; kept so the stored key is the
/// one used).
fn clamp(mut k: [u8; LEN]) -> [u8; LEN] {
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;
    k
}

/// The client's three keys.
#[derive(Clone)]
pub struct Keys {
    pub machine: KeyPair,
    pub node: KeyPair,
    pub disco: KeyPair,
}

impl Keys {
    pub fn generate(rng: &mut impl CryptoRngCore) -> Keys {
        Keys { machine: KeyPair::generate(rng), node: KeyPair::generate(rng), disco: KeyPair::generate(rng) }
    }

    /// The private keys, machine, node, disco (for storing).
    pub fn to_bytes(&self) -> [u8; 3 * LEN] {
        let mut b = [0u8; 3 * LEN];
        b[..LEN].copy_from_slice(&self.machine.private);
        b[LEN..2 * LEN].copy_from_slice(&self.node.private);
        b[2 * LEN..].copy_from_slice(&self.disco.private);
        b
    }

    pub fn from_bytes(b: &[u8; 3 * LEN]) -> Keys {
        let k = |i: usize| KeyPair::from_private(b[i * LEN..(i + 1) * LEN].try_into().unwrap());
        Keys { machine: k(0), node: k(1), disco: k(2) }
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// `prefix` and the key in hex (`nodekey:…`).
pub fn to_hex(prefix: &str, key: &[u8; LEN]) -> String {
    let mut s = String::with_capacity(prefix.len() + 2 * LEN);
    s.push_str(prefix);
    for b in key {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}

/// A key in hex after any `word:` prefix.
pub fn from_hex(s: &str) -> Option<[u8; LEN]> {
    let hex = s.rsplit(':').next()?.trim();
    if hex.len() != 2 * LEN {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut k = [0u8; LEN];
    for (i, p) in hex.as_bytes().chunks_exact(2).enumerate() {
        k[i] = digit(p[0])? << 4 | digit(p[1])?;
    }
    Some(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let k = KeyPair::from_private([1; 32]);
        let s = to_hex("nodekey:", &k.public);
        assert!(s.starts_with("nodekey:") && s.len() == 8 + 64);
        assert_eq!(from_hex(&s), Some(k.public));
        assert_eq!(from_hex("mkey:00"), None);
        // RFC 7748 §6.1: Alice's public key.
        let a = from_hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a").unwrap();
        assert_eq!(to_hex("", &KeyPair::from_private(a).public), "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
    }

    #[test]
    fn seed_is_blake2s() {
        let mut raw = [0u8; RAW_SEED];
        raw.iter_mut().enumerate().for_each(|(i, b)| *b = i as u8);
        // hashlib.blake2s(b"tab5-os ts seed v1" + bytes(range(64))).
        assert_eq!(to_hex("", &seed(&raw)), "5cd4bd13639f6dd2f691fecce6ed8404df45734d62602874f2e821dcae8ab3db");
    }

    #[test]
    fn keys_bytes() {
        let k = Keys::from_bytes(&[2; 96]);
        assert_eq!(Keys::from_bytes(&k.to_bytes()).node.public, k.node.public);
    }
}
