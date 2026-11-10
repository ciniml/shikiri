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

#[derive(Debug, PartialEq, Eq)]
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
    let nonce: [u8; NONCE] = pkt.get(MAGIC.len() + LEN..HEADER)?.try_into().ok()?;
    let sealed = pkt.get(HEADER..)?;
    let (tag, ct) = sealed.split_at_checked(TAG)?;
    let mut m = ct.to_vec();
    let b = SalsaBox::new(&PublicKey::from(*from), &SecretKey::from(me.private));
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
    let mut ct = body(m);
    let b = SalsaBox::new(&PublicKey::from(*to), &SecretKey::from(me.private));
    let tag = b.encrypt_in_place_detached(&nonce.into(), b"", &mut ct).expect("box");
    let mut p = Vec::with_capacity(HEADER + TAG + ct.len());
    p.extend_from_slice(MAGIC);
    p.extend_from_slice(&me.public);
    p.extend_from_slice(&nonce);
    p.extend_from_slice(&tag);
    p.extend_from_slice(&ct);
    p
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
        assert_eq!(open(&b, &a.public, &p), Some(ping));
        // Not for a third key.
        assert_eq!(open(&KeyPair::from_private([9; 32]), &a.public, &p), None);
        let pong = Message::Pong { tx: [1; TXID], src: v4([192, 0, 2, 1], 41641) };
        assert_eq!(open(&a, &b.public, &seal(&b, &a.public, [4; NONCE], &pong)), Some(pong));
        let cmm = Message::CallMeMaybe { endpoints: alloc::vec![v4([192, 0, 2, 2], 1), v4([198, 51, 100, 1], 2)] };
        assert_eq!(open(&a, &b.public, &seal(&b, &a.public, [5; NONCE], &cmm)), Some(cmm));
    }
}
