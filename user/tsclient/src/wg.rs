//! WireGuard (the whitepaper, §5): both sides of the handshake
//! (Noise_IKpsk2 with the zero pre-shared key, as Tailscale) and the
//! transport messages, for the peers of the netmap. Packets arrive
//! through DERP from a node key; the node key is the peer's WireGuard key.
//!
//! Initiating takes a TAI64N timestamp from the caller (a wall clock: the
//! control server's time, doc/71 §4.7). Cookies (mac2) are neither checked
//! nor sent: no load to shed.

use alloc::vec;
use alloc::vec::Vec;
use blake2::digest::consts::U16;
use blake2::digest::{KeyInit, Mac};
use blake2::{Blake2s256, Blake2sMac, Digest};
use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, Key, Tag};
use rand_core::CryptoRngCore;

use crate::keys::{KeyPair, LEN};
use crate::noise::{hkdf2, hmac};

const CONSTRUCTION: &[u8] = b"Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s";
const IDENTIFIER: &[u8] = b"WireGuard v1 zx2c4 Jason@zx2c4.com";
const LABEL_MAC1: &[u8] = b"mac1----";

const INITIATION: u8 = 1;
const RESPONSE: u8 = 2;
const TRANSPORT: u8 = 4;
pub const INITIATION_LEN: usize = 148;
pub const RESPONSE_LEN: usize = 92;
const TAG_LEN: usize = 16;
/// Transport header: type, reserved, receiver, counter.
const HEADER: usize = 16;
/// Sessions end after this (REJECT_AFTER_TIME).
const REJECT_AFTER_MS: u64 = 180_000;
/// The initiator starts a new session after this (REKEY_AFTER_TIME).
const REKEY_AFTER_MS: u64 = 120_000;
/// An initiation unanswered this long is sent again (REKEY_TIMEOUT).
const REKEY_TIMEOUT_MS: u64 = 5_000;
const WINDOW: u64 = 64;

fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut d = Blake2s256::new();
    for p in parts {
        d.update(p);
    }
    d.finalize().into()
}

fn mac(key: &[u8; 32], data: &[u8]) -> [u8; 16] {
    let mut m = <Blake2sMac<U16> as KeyInit>::new_from_slice(key).unwrap();
    Mac::update(&mut m, data);
    m.finalize().into_bytes().into()
}

fn kdf1(c: &[u8; 32], ikm: &[u8]) -> [u8; 32] {
    hkdf2(c, ikm).0
}

fn kdf3(c: &[u8; 32], ikm: &[u8]) -> ([u8; 32], [u8; 32], [u8; 32]) {
    let prk = hmac(c, &[ikm]);
    let a = hmac(&prk, &[&[1]]);
    let b = hmac(&prk, &[&a, &[2]]);
    let d = hmac(&prk, &[&b, &[3]]);
    (a, b, d)
}

fn nonce(n: u64) -> chacha20poly1305::Nonce {
    let mut b = [0u8; 12];
    b[4..].copy_from_slice(&n.to_le_bytes());
    b.into()
}

/// Decrypts `ct` (with its tag) into `out`; `aad` the handshake hash.
fn open(key: &[u8; 32], aad: &[u8], ct: &[u8], out: &mut [u8]) -> bool {
    let (c, tag) = ct.split_at(ct.len() - TAG_LEN);
    out[..c.len()].copy_from_slice(c);
    ChaCha20Poly1305::new(Key::from_slice(key)).decrypt_in_place_detached(&nonce(0), aad, &mut out[..c.len()], Tag::from_slice(tag)).is_ok()
}

struct Session {
    local: u32,
    remote: u32,
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    send_n: u64,
    /// The highest counter received and the 64 below it seen (bit i: max - i).
    recv_max: u64,
    seen: u64,
    any: bool,
    at_ms: u64,
    /// This node started it (and so starts the next).
    initiated: bool,
}

/// An initiation sent, waiting for the response.
struct Pending {
    local: u32,
    e: KeyPair,
    c: [u8; 32],
    h: [u8; 32],
    at_ms: u64,
}

impl Session {
    /// Whether counter `n` is new (and marks it).
    fn accept(&mut self, n: u64) -> bool {
        if !self.any || n > self.recv_max {
            let shift = if self.any { n - self.recv_max } else { WINDOW };
            self.seen = if shift >= WINDOW { 0 } else { self.seen << shift };
            self.seen |= 1;
            self.recv_max = n;
            self.any = true;
            return true;
        }
        let back = self.recv_max - n;
        if back >= WINDOW || self.seen & 1 << back != 0 {
            return false;
        }
        self.seen |= 1 << back;
        true
    }
}

/// A netmap peer as WireGuard knows it.
pub struct Peer {
    pub key: [u8; LEN],
    /// Its tailnet IPv4 address: what it may send from.
    pub ip: [u8; 4],
    last_timestamp: [u8; 12],
    /// Answered, waiting for the peer's first transport message.
    next: Option<Session>,
    current: Option<Session>,
    previous: Option<Session>,
    pending: Option<Pending>,
}

/// What `Device::receive` made of a message.
#[derive(Debug, PartialEq, Eq)]
pub enum Received {
    /// Nothing to do (a keep-alive, a duplicate, something refused).
    Nothing(&'static str),
    /// Send this back to the sender (a handshake response).
    Reply(Vec<u8>),
    /// An IP packet from peer `peer` (index into `peers`).
    Packet { peer: usize, packet: Vec<u8> },
    /// Our initiation was answered: peer `peer` has a session to send on.
    Established { peer: usize },
}

pub struct Device {
    key: KeyPair,
    mac1_key: [u8; 32],
    /// Hash after the construction, identifier and our key.
    h0: [u8; 32],
    c0: [u8; 32],
    pub peers: Vec<Peer>,
    next_index: u32,
    /// The last initiator not among the peers (for the log).
    pub unknown: [u8; LEN],
}

impl Device {
    pub fn new(key: KeyPair, rng: &mut impl CryptoRngCore) -> Device {
        let c0 = hash(&[CONSTRUCTION]);
        let h0 = hash(&[&hash(&[&c0, IDENTIFIER]), &key.public]);
        let mac1_key = hash(&[LABEL_MAC1, &key.public]);
        Device { key, mac1_key, h0, c0, peers: Vec::new(), next_index: rng.next_u32(), unknown: [0; LEN] }
    }

    /// This node's key (its node key).
    pub fn public(&self) -> [u8; LEN] {
        self.key.public
    }

    /// Sets the peers (keys and addresses), keeping the sessions of those
    /// that stay.
    pub fn set_peers(&mut self, peers: impl Iterator<Item = ([u8; LEN], [u8; 4])>) {
        let mut old = core::mem::take(&mut self.peers);
        for (key, ip) in peers {
            match old.iter().position(|p| p.key == key) {
                Some(i) => {
                    let mut p = old.swap_remove(i);
                    p.ip = ip;
                    self.peers.push(p);
                }
                None => self.peers.push(Peer { key, ip, last_timestamp: [0; 12], next: None, current: None, previous: None, pending: None }),
            }
        }
    }

    /// A message that came from node key `from` (through DERP), or over
    /// UDP (None: an initiation names its sender inside).
    pub fn receive(&mut self, from: Option<&[u8; LEN]>, msg: &[u8], rng: &mut impl CryptoRngCore, now_ms: u64) -> Received {
        match (msg.first(), msg.len()) {
            (Some(&INITIATION), INITIATION_LEN) => self.initiation(from, msg, rng, now_ms),
            (Some(&TRANSPORT), n) if n >= HEADER + TAG_LEN => self.transport(msg, now_ms),
            (Some(&RESPONSE), RESPONSE_LEN) => self.response(msg, now_ms),
            _ => Received::Nothing("not wireguard"),
        }
    }

    fn initiation(&mut self, from: Option<&[u8; LEN]>, m: &[u8], rng: &mut impl CryptoRngCore, now_ms: u64) -> Received {
        if mac(&self.mac1_key, &m[..116]) != m[116..132] {
            return Received::Nothing("mac1");
        }
        let sender = u32::from_le_bytes(m[4..8].try_into().unwrap());
        let e: [u8; 32] = m[8..40].try_into().unwrap();
        let mut c = kdf1(&self.c0, &e);
        let mut h = hash(&[&self.h0, &e]);
        let (c1, k) = hkdf2(&c, &self.key.dh(&e));
        c = c1;
        let mut s = [0u8; 32];
        if !open(&k, &h, &m[40..88], &mut s) {
            return Received::Nothing("static");
        }
        h = hash(&[&h, &m[40..88]]);
        // The node key DERP named must be the WireGuard key inside.
        if from.is_some_and(|f| *f != s) {
            return Received::Nothing("key mismatch");
        }
        let Some(pi) = self.peers.iter().position(|p| p.key == s) else {
            self.unknown = s;
            return Received::Nothing("unknown peer");
        };
        let (c2, k) = hkdf2(&c, &self.key.dh(&s));
        c = c2;
        let mut ts = [0u8; 12];
        if !open(&k, &h, &m[88..116], &mut ts) {
            return Received::Nothing("timestamp");
        }
        h = hash(&[&h, &m[88..116]]);
        let peer = &mut self.peers[pi];
        if ts <= peer.last_timestamp {
            return Received::Nothing("replayed initiation");
        }
        peer.last_timestamp = ts;

        // The response.
        let mut er = [0u8; 32];
        rng.fill_bytes(&mut er);
        let er = KeyPair::from_private(er);
        let local = self.next_index;
        self.next_index = self.next_index.wrapping_add(1);
        let mut r = vec![0u8; RESPONSE_LEN];
        r[0] = RESPONSE;
        r[4..8].copy_from_slice(&local.to_le_bytes());
        r[8..12].copy_from_slice(&sender.to_le_bytes());
        r[12..44].copy_from_slice(&er.public);
        c = kdf1(&c, &er.public);
        h = hash(&[&h, &er.public]);
        c = kdf1(&c, &er.dh(&e));
        c = kdf1(&c, &er.dh(&s));
        let (c3, t, k) = kdf3(&c, &[0u8; 32]);
        c = c3;
        h = hash(&[&h, &t]);
        let tag = ChaCha20Poly1305::new(Key::from_slice(&k)).encrypt_in_place_detached(&nonce(0), &h, &mut []).unwrap();
        r[44..60].copy_from_slice(&tag);
        let mac1 = mac(&hash(&[LABEL_MAC1, &s]), &r[..60]);
        r[60..76].copy_from_slice(&mac1);
        // The initiator sends with the first key, we with the second.
        let (recv, send) = hkdf2(&c, &[]);
        peer.next = Some(Session {
            local,
            remote: sender,
            send: ChaCha20Poly1305::new(&send.into()),
            recv: ChaCha20Poly1305::new(&recv.into()),
            send_n: 0,
            recv_max: 0,
            seen: 0,
            any: false,
            at_ms: now_ms,
            initiated: false,
        });
        Received::Reply(r)
    }

    /// An initiation to peer `peer` with timestamp `tai64n` (later than
    /// any before: the peer refuses the rest).
    pub fn initiate(&mut self, peer: usize, tai64n: [u8; 12], rng: &mut impl CryptoRngCore, now_ms: u64) -> Option<Vec<u8>> {
        let r_pub = self.peers.get(peer)?.key;
        let mut e = [0u8; 32];
        rng.fill_bytes(&mut e);
        let e = KeyPair::from_private(e);
        let local = self.next_index;
        self.next_index = self.next_index.wrapping_add(1);
        let mut h = hash(&[&hash(&[&self.c0, IDENTIFIER]), &r_pub]);
        let mut m = vec![0u8; INITIATION_LEN];
        m[0] = INITIATION;
        m[4..8].copy_from_slice(&local.to_le_bytes());
        m[8..40].copy_from_slice(&e.public);
        let mut c = kdf1(&self.c0, &e.public);
        h = hash(&[&h, &e.public]);
        let (c1, k) = hkdf2(&c, &e.dh(&r_pub));
        c = c1;
        m[40..72].copy_from_slice(&self.key.public);
        let t = ChaCha20Poly1305::new(Key::from_slice(&k)).encrypt_in_place_detached(&nonce(0), &h, &mut m[40..72]).ok()?;
        m[72..88].copy_from_slice(&t);
        h = hash(&[&h, &m[40..88]]);
        let (c2, k) = hkdf2(&c, &self.key.dh(&r_pub));
        c = c2;
        m[88..100].copy_from_slice(&tai64n);
        let t = ChaCha20Poly1305::new(Key::from_slice(&k)).encrypt_in_place_detached(&nonce(0), &h, &mut m[88..100]).ok()?;
        m[100..116].copy_from_slice(&t);
        h = hash(&[&h, &m[88..116]]);
        let mac1 = mac(&hash(&[LABEL_MAC1, &r_pub]), &m[..116]);
        m[116..132].copy_from_slice(&mac1);
        self.peers[peer].pending = Some(Pending { local, e, c, h, at_ms: now_ms });
        Some(m)
    }

    fn response(&mut self, m: &[u8], now_ms: u64) -> Received {
        if mac(&self.mac1_key, &m[..60]) != m[60..76] {
            return Received::Nothing("mac1");
        }
        let sender = u32::from_le_bytes(m[4..8].try_into().unwrap());
        let receiver = u32::from_le_bytes(m[8..12].try_into().unwrap());
        let Some(pi) = self.peers.iter().position(|p| p.pending.as_ref().is_some_and(|q| q.local == receiver)) else {
            return Received::Nothing("response unasked");
        };
        let q = self.peers[pi].pending.as_ref().unwrap();
        let er: [u8; 32] = m[12..44].try_into().unwrap();
        let mut c = kdf1(&q.c, &er);
        let mut h = hash(&[&q.h, &er]);
        c = kdf1(&c, &q.e.dh(&er));
        c = kdf1(&c, &self.key.dh(&er));
        let (c3, t, k) = kdf3(&c, &[0u8; 32]);
        h = hash(&[&h, &t]);
        if !open(&k, &h, &m[44..60], &mut []) {
            return Received::Nothing("response");
        }
        // The initiator sends with the first key.
        let (send, recv) = hkdf2(&c3, &[]);
        let p = &mut self.peers[pi];
        let local = p.pending.take().unwrap().local;
        p.previous = p.current.take();
        p.current = Some(Session {
            local,
            remote: sender,
            send: ChaCha20Poly1305::new(&send.into()),
            recv: ChaCha20Poly1305::new(&recv.into()),
            send_n: 0,
            recv_max: 0,
            seen: 0,
            any: false,
            at_ms: now_ms,
            initiated: true,
        });
        Received::Established { peer: pi }
    }

    /// Peer `peer` has a session to send on now.
    pub fn can_send(&self, peer: usize, now_ms: u64) -> bool {
        self.peers.get(peer).and_then(|p| p.current.as_ref()).is_some_and(|s| now_ms.saturating_sub(s.at_ms) <= REJECT_AFTER_MS)
    }

    /// Sending to peer `peer` should start a handshake: no session to
    /// send on, or ours is old enough to replace; and no initiation is
    /// out (or it went unanswered).
    pub fn wants_handshake(&self, peer: usize, now_ms: u64) -> bool {
        let Some(p) = self.peers.get(peer) else { return false };
        if p.pending.as_ref().is_some_and(|q| now_ms.saturating_sub(q.at_ms) < REKEY_TIMEOUT_MS) {
            return false;
        }
        match &p.current {
            Some(s) => now_ms.saturating_sub(s.at_ms) > if s.initiated { REKEY_AFTER_MS } else { REJECT_AFTER_MS },
            None => true,
        }
    }

    fn transport(&mut self, m: &[u8], now_ms: u64) -> Received {
        let local = u32::from_le_bytes(m[4..8].try_into().unwrap());
        let n = u64::from_le_bytes(m[8..16].try_into().unwrap());
        for (pi, p) in self.peers.iter_mut().enumerate() {
            let which = [&p.next, &p.current, &p.previous].iter().position(|s| s.as_ref().is_some_and(|s| s.local == local));
            let Some(which) = which else { continue };
            let s = [&mut p.next, &mut p.current, &mut p.previous][which].as_mut().unwrap();
            if now_ms.saturating_sub(s.at_ms) > REJECT_AFTER_MS {
                return Received::Nothing("session expired");
            }
            let (ct, tag) = m[HEADER..].split_at(m.len() - HEADER - TAG_LEN);
            let mut pt = ct.to_vec();
            if s.recv.decrypt_in_place_detached(&nonce(n), &[], &mut pt, Tag::from_slice(tag)).is_err() {
                return Received::Nothing("transport decrypt");
            }
            if !s.accept(n) {
                return Received::Nothing("replayed transport");
            }
            if which == 0 {
                // The peer confirmed the session: it becomes the current one.
                p.previous = p.current.take();
                p.current = p.next.take();
            }
            if pt.is_empty() {
                return Received::Nothing("keep-alive");
            }
            // The packet's own length (padding dropped), from a source the
            // peer may use.
            let Some(len) = ipv4_len(&pt) else { return Received::Nothing("not ipv4") };
            pt.truncate(len);
            if pt[12..16] != p.ip {
                return Received::Nothing("source not allowed");
            }
            return Received::Packet { peer: pi, packet: pt };
        }
        Received::Nothing("no session")
    }

    /// The transport message carrying `packet` to peer `peer`, if a
    /// session is up.
    pub fn encrypt(&mut self, peer: usize, packet: &[u8], now_ms: u64) -> Option<Vec<u8>> {
        let s = self.peers.get_mut(peer)?.current.as_mut()?;
        if now_ms.saturating_sub(s.at_ms) > REJECT_AFTER_MS {
            return None;
        }
        let padded = packet.len().div_ceil(16) * 16;
        let mut m = vec![0u8; HEADER + padded + TAG_LEN];
        m[0] = TRANSPORT;
        m[4..8].copy_from_slice(&s.remote.to_le_bytes());
        m[8..16].copy_from_slice(&s.send_n.to_le_bytes());
        m[HEADER..HEADER + packet.len()].copy_from_slice(packet);
        let tag = s.send.encrypt_in_place_detached(&nonce(s.send_n), &[], &mut m[HEADER..HEADER + padded]).ok()?;
        s.send_n += 1;
        m[HEADER + padded..].copy_from_slice(&tag);
        Some(m)
    }
}

/// An IPv4 packet's total length, if it is one and fits.
fn ipv4_len(p: &[u8]) -> Option<usize> {
    if p.len() < 20 || p[0] >> 4 != 4 {
        return None;
    }
    let len = u16::from_be_bytes([p[2], p[3]]) as usize;
    (20..=p.len()).contains(&len).then_some(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter(u64);
    impl rand_core::RngCore for Counter {
        fn next_u32(&mut self) -> u32 {
            self.next_u64() as u32
        }
        fn next_u64(&mut self) -> u64 {
            self.0 += 1;
            self.0.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        }
        fn fill_bytes(&mut self, d: &mut [u8]) {
            for b in d {
                *b = self.next_u64() as u8;
            }
        }
        fn try_fill_bytes(&mut self, d: &mut [u8]) -> core::result::Result<(), rand_core::Error> {
            self.fill_bytes(d);
            Ok(())
        }
    }
    impl rand_core::CryptoRng for Counter {}

    /// The initiator's side, written out from the whitepaper, to check
    /// the responder against.
    fn initiate(i: &KeyPair, r_pub: &[u8; 32], e: &KeyPair, ts: [u8; 12]) -> ([u8; 148], [u8; 32], [u8; 32]) {
        let c0 = hash(&[CONSTRUCTION]);
        let mut h = hash(&[&hash(&[&c0, IDENTIFIER]), r_pub]);
        let mut m = [0u8; 148];
        m[0] = 1;
        m[4..8].copy_from_slice(&7u32.to_le_bytes());
        m[8..40].copy_from_slice(&e.public);
        let mut c = kdf1(&c0, &e.public);
        h = hash(&[&h, &e.public]);
        let (c1, k) = hkdf2(&c, &e.dh(r_pub));
        c = c1;
        m[40..72].copy_from_slice(&i.public);
        let t = ChaCha20Poly1305::new(Key::from_slice(&k)).encrypt_in_place_detached(&nonce(0), &h, &mut m[40..72]).unwrap();
        m[72..88].copy_from_slice(&t);
        h = hash(&[&h, &m[40..88]]);
        let (c2, k) = hkdf2(&c, &i.dh(r_pub));
        c = c2;
        m[88..100].copy_from_slice(&ts);
        let t = ChaCha20Poly1305::new(Key::from_slice(&k)).encrypt_in_place_detached(&nonce(0), &h, &mut m[88..100]).unwrap();
        m[100..116].copy_from_slice(&t);
        h = hash(&[&h, &m[88..116]]);
        let mac1 = mac(&hash(&[LABEL_MAC1, r_pub]), &m[..116]);
        m[116..132].copy_from_slice(&mac1);
        (m, c, h)
    }

    #[test]
    fn handshake_and_transport() {
        let mut rng = Counter(1);
        let me = KeyPair::from_private([1; 32]);
        let peer = KeyPair::from_private([2; 32]);
        let mut d = Device::new(me.clone(), &mut rng);
        d.set_peers([(peer.public, [100, 64, 0, 2])].into_iter());
        let e = KeyPair::from_private([3; 32]);
        let (m, mut c, mut h) = initiate(&peer, &me.public, &e, [0, 0, 0, 0, 0x40, 0, 0, 1, 0, 0, 0, 0]);

        let Received::Reply(r) = d.receive(Some(&peer.public), &m, &mut rng, 0) else { panic!() };
        assert_eq!(r.len(), RESPONSE_LEN);
        assert_eq!(&r[8..12], &7u32.to_le_bytes());
        // The initiator finishes: the empty payload opens, mac1 checks.
        assert_eq!(mac(&hash(&[LABEL_MAC1, &peer.public]), &r[..60]), r[60..76]);
        let er: [u8; 32] = r[12..44].try_into().unwrap();
        c = kdf1(&c, &er);
        h = hash(&[&h, &er]);
        c = kdf1(&c, &e.dh(&er));
        c = kdf1(&c, &peer.dh(&er));
        let (c3, t, k) = kdf3(&c, &[0; 32]);
        h = hash(&[&h, &t]);
        let mut none = [0u8; 0];
        assert!(open(&k, &h, &r[44..60], &mut none));
        let (send, recv) = hkdf2(&c3, &[]);

        // A replayed initiation is refused.
        assert_eq!(d.receive(Some(&peer.public), &m, &mut rng, 0), Received::Nothing("replayed initiation"));

        // The initiator's first transport message: an ICMP echo request.
        let local = u32::from_le_bytes(r[4..8].try_into().unwrap());
        let mut ip = vec![0x45, 0, 0, 28, 0, 0, 0, 0, 64, 1, 0, 0, 100, 64, 0, 2, 100, 64, 0, 1];
        ip.extend_from_slice(&[8, 0, 0, 0, 0, 1, 0, 1]);
        let mut pt = ip.clone();
        pt.resize(32, 0);
        let mut msg = vec![4, 0, 0, 0];
        msg.extend_from_slice(&local.to_le_bytes());
        msg.extend_from_slice(&0u64.to_le_bytes());
        let tag = ChaCha20Poly1305::new(&send.into()).encrypt_in_place_detached(&nonce(0), &[], &mut pt).unwrap();
        msg.extend_from_slice(&pt);
        msg.extend_from_slice(&tag);
        assert_eq!(d.receive(Some(&peer.public), &msg, &mut rng, 1), Received::Packet { peer: 0, packet: ip.clone() });
        assert_eq!(d.receive(Some(&peer.public), &msg, &mut rng, 1), Received::Nothing("replayed transport"));

        // Our answer opens with the initiator's receive key.
        let out = d.encrypt(0, &ip, 2).unwrap();
        assert_eq!(&out[4..8], &7u32.to_le_bytes());
        let mut body = out[16..out.len() - 16].to_vec();
        ChaCha20Poly1305::new(&recv.into())
            .decrypt_in_place_detached(&nonce(0), &[], &mut body, Tag::from_slice(&out[out.len() - 16..]))
            .unwrap();
        assert_eq!(&body[..28], &ip[..]);
    }

    #[test]
    fn initiate_to_device() {
        let mut rng = Counter(9);
        let (ka, kb) = (KeyPair::from_private([4; 32]), KeyPair::from_private([5; 32]));
        let mut a = Device::new(ka.clone(), &mut rng);
        let mut b = Device::new(kb.clone(), &mut rng);
        a.set_peers([(kb.public, [100, 64, 0, 2])].into_iter());
        b.set_peers([(ka.public, [100, 64, 0, 1])].into_iter());
        assert!(a.wants_handshake(0, 0) && !a.can_send(0, 0));
        let m = a.initiate(0, [0, 0, 0, 0, 0x40, 0, 0, 1, 0, 0, 0, 0], &mut rng, 0).unwrap();
        assert!(!a.wants_handshake(0, 1000));
        assert!(a.wants_handshake(0, 6000));
        let Received::Reply(r) = b.receive(Some(&ka.public), &m, &mut rng, 0) else { panic!() };
        assert_eq!(a.receive(Some(&kb.public), &r, &mut rng, 1), Received::Established { peer: 0 });
        assert_eq!(a.receive(Some(&kb.public), &r, &mut rng, 1), Received::Nothing("response unasked"));
        assert!(a.can_send(0, 2) && !a.wants_handshake(0, 2));
        let ip = vec![0x45, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0, 100, 64, 0, 1, 100, 64, 0, 2];
        let t = a.encrypt(0, &ip, 2).unwrap();
        assert_eq!(b.receive(Some(&ka.public), &t, &mut rng, 2), Received::Packet { peer: 0, packet: ip.clone() });
        let back = b.encrypt(0, &[&ip[..12], &ip[16..20], &ip[12..16]].concat(), 3).unwrap();
        assert!(matches!(a.receive(Some(&kb.public), &back, &mut rng, 3), Received::Packet { .. }));
        // Ours is replaced after REKEY_AFTER_TIME.
        assert!(a.wants_handshake(0, 121_000));
        assert!(!b.wants_handshake(0, 121_000));
    }

    #[test]
    fn replay_window() {
        let mut s = Session {
            local: 0,
            remote: 0,
            send: ChaCha20Poly1305::new(&[0; 32].into()),
            recv: ChaCha20Poly1305::new(&[0; 32].into()),
            send_n: 0,
            recv_max: 0,
            seen: 0,
            any: false,
            at_ms: 0,
            initiated: false,
        };
        assert!(s.accept(5));
        assert!(s.accept(3));
        assert!(!s.accept(3));
        assert!(s.accept(100));
        assert!(!s.accept(36));
        assert!(s.accept(37));
        assert!(!s.accept(100));
    }
}
