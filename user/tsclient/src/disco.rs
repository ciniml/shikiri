// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! Disco, Tailscale's path discovery (tailscale.com/disco): `TS💬`, the
//! sender's disco key, a nonce, and a NaCl box (tag, then ciphertext) from
//! the sender's disco key to the receiver's. Inside, a type and a version
//! byte: Ping (transaction id, the sender's node key), Pong (transaction
//! id, the address the ping came from), CallMeMaybe (endpoints). Sent
//! through DERP or straight over UDP.

use alloc::vec::Vec;
use crypto_box::aead::AeadInPlace;
use crypto_box::{PublicKey, SalsaBox, SecretKey};

use crate::keys::{KeyPair, LEN};

pub const MAGIC: &[u8; 6] = b"TS\xF0\x9F\x92\xAC";
const NONCE: usize = 24;
const TAG: usize = 16;
const HEADER: usize = MAGIC.len() + LEN + NONCE;
pub const TXID: usize = 12;

const PING: u8 = 1;
const PONG: u8 = 2;
const CALL_ME_MAYBE: u8 = 3;

/// An address as disco writes it: IPv6 (IPv4-mapped for IPv4) and port.
pub type Addr = ([u8; 16], u16);

pub fn v4(ip: [u8; 4], port: u16) -> Addr {
    let mut a = [0u8; 16];
    a[10] = 0xFF;
    a[11] = 0xFF;
    a[12..].copy_from_slice(&ip);
    (a, port)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Ping { tx: [u8; TXID], node_key: Option<[u8; LEN]> },
    Pong { tx: [u8; TXID], src: Addr },
    CallMeMaybe { endpoints: Vec<Addr> },
    Other(u8),
}

/// The sender's disco key, if `pkt` is a disco message.
pub fn sender(pkt: &[u8]) -> Option<[u8; LEN]> {
    if pkt.len() < HEADER + TAG + 2 || !pkt.starts_with(MAGIC) {
        return None;
    }
    pkt[MAGIC.len()..MAGIC.len() + LEN].try_into().ok()
}

/// Opens a disco message to `me` from the disco key `from`.
pub fn open(me: &KeyPair, from: &[u8; LEN], pkt: &[u8]) -> Option<Message> {
    open_with(&SalsaBox::new(&PublicKey::from(*from), &SecretKey::from(me.private)), pkt)
}

fn open_with(b: &SalsaBox, pkt: &[u8]) -> Option<Message> {
    let nonce: [u8; NONCE] = pkt.get(MAGIC.len() + LEN..HEADER)?.try_into().ok()?;
    let sealed = pkt.get(HEADER..)?;
    let (tag, ct) = sealed.split_at_checked(TAG)?;
    let mut m = ct.to_vec();
    b.decrypt_in_place_detached(&nonce.into(), b"", &mut m, tag.into()).ok()?;
    let (&ty, rest) = m.split_first()?;
    let body = rest.get(1..)?;
    Some(match ty {
        PING if body.len() >= TXID => {
            let tx = body[..TXID].try_into().unwrap();
            Message::Ping { tx, node_key: body.get(TXID..TXID + LEN).map(|k| k.try_into().unwrap()) }
        }
        PONG if body.len() >= TXID + 18 => {
            let a: [u8; 16] = body[TXID..TXID + 16].try_into().unwrap();
            Message::Pong { tx: body[..TXID].try_into().unwrap(), src: (a, u16::from_be_bytes([body[TXID + 16], body[TXID + 17]])) }
        }
        CALL_ME_MAYBE => Message::CallMeMaybe { endpoints: body.chunks_exact(18).map(|c| (c[..16].try_into().unwrap(), u16::from_be_bytes([c[16], c[17]]))).collect() },
        t => Message::Other(t),
    })
}

fn body(m: &Message) -> Vec<u8> {
    let mut b = Vec::new();
    match m {
        Message::Ping { tx, node_key } => {
            b.extend_from_slice(&[PING, 0]);
            b.extend_from_slice(tx);
            if let Some(k) = node_key {
                b.extend_from_slice(k);
            }
        }
        Message::Pong { tx, src } => {
            b.extend_from_slice(&[PONG, 0]);
            b.extend_from_slice(tx);
            b.extend_from_slice(&src.0);
            b.extend_from_slice(&src.1.to_be_bytes());
        }
        Message::CallMeMaybe { endpoints } => {
            b.extend_from_slice(&[CALL_ME_MAYBE, 0]);
            for (a, p) in endpoints {
                b.extend_from_slice(a);
                b.extend_from_slice(&p.to_be_bytes());
            }
        }
        Message::Other(t) => b.extend_from_slice(&[*t, 0]),
    }
    b
}

/// Seals `m` from `me` to the disco key `to`; `nonce` fresh randomness.
pub fn seal(me: &KeyPair, to: &[u8; LEN], nonce: [u8; NONCE], m: &Message) -> Vec<u8> {
    seal_with(&SalsaBox::new(&PublicKey::from(*to), &SecretKey::from(me.private)), &me.public, nonce, m)
}

fn seal_with(b: &SalsaBox, me: &[u8; LEN], nonce: [u8; NONCE], m: &Message) -> Vec<u8> {
    let mut ct = body(m);
    let tag = b.encrypt_in_place_detached(&nonce.into(), b"", &mut ct).expect("box");
    let mut p = Vec::with_capacity(HEADER + TAG + ct.len());
    p.extend_from_slice(MAGIC);
    p.extend_from_slice(me);
    p.extend_from_slice(&nonce);
    p.extend_from_slice(&tag);
    p.extend_from_slice(&ct);
    p
}

/// Peers' boxes kept.
const BOXES: usize = 16;

/// `open` and `seal` with the box for each peer kept: making one is an
/// X25519 (about 9 ms on the ESP32-P4), and a peer that wakes sends a
/// dozen messages or so at once.
pub struct Boxes {
    me: KeyPair,
    /// Least recently used first.
    boxes: Vec<([u8; LEN], SalsaBox)>,
}

impl Boxes {
    pub fn new(me: KeyPair) -> Boxes {
        Boxes { me, boxes: Vec::new() }
    }

    fn get(&mut self, peer: &[u8; LEN]) -> &SalsaBox {
        let b = match self.boxes.iter().position(|b| b.0 == *peer) {
            Some(i) => self.boxes.remove(i),
            None => {
                if self.boxes.len() == BOXES {
                    self.boxes.remove(0);
                }
                (*peer, SalsaBox::new(&PublicKey::from(*peer), &SecretKey::from(self.me.private)))
            }
        };
        self.boxes.push(b);
        &self.boxes.last().unwrap().1
    }

    pub fn open(&mut self, from: &[u8; LEN], pkt: &[u8]) -> Option<Message> {
        open_with(self.get(from), pkt)
    }

    pub fn seal(&mut self, to: &[u8; LEN], nonce: [u8; NONCE], m: &Message) -> Vec<u8> {
        let me = self.me.public;
        seal_with(self.get(to), &me, nonce, m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let (a, b) = (KeyPair::from_private([7; 32]), KeyPair::from_private([8; 32]));
        let ping = Message::Ping { tx: [1; TXID], node_key: Some([2; LEN]) };
        let p = seal(&a, &b.public, [3; NONCE], &ping);
        assert_eq!(sender(&p), Some(a.public));
        assert_eq!(open(&b, &a.public, &p), Some(ping.clone()));
        // Not for a third key.
        assert_eq!(open(&KeyPair::from_private([9; 32]), &a.public, &p), None);
        let pong = Message::Pong { tx: [1; TXID], src: v4([192, 0, 2, 1], 41641) };
        assert_eq!(open(&a, &b.public, &seal(&b, &a.public, [4; NONCE], &pong)), Some(pong));
        let cmm = Message::CallMeMaybe { endpoints: alloc::vec![v4([192, 0, 2, 2], 1), v4([198, 51, 100, 1], 2)] };
        assert_eq!(open(&a, &b.public, &seal(&b, &a.public, [5; NONCE], &cmm)), Some(cmm.clone()));
        // The same through kept boxes, one peer pushed out and made again.
        let mut boxes = Boxes::new(b.clone());
        assert_eq!(boxes.open(&a.public, &p), Some(ping.clone()));
        for k in 0..BOXES as u8 {
            boxes.seal(&[k; LEN], [6; NONCE], &cmm);
        }
        assert_eq!((boxes.boxes.len(), boxes.open(&a.public, &p)), (BOXES, Some(ping)));
        assert_eq!(open(&a, &b.public, &boxes.seal(&a.public, [7; NONCE], &cmm)), Some(cmm));
    }
}
