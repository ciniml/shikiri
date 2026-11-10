//! ts2021's Noise channel to the control server: the IK handshake
//! (Noise_IK_25519_ChaChaPoly_BLAKE2s, prologue "Tailscale Control
//! Protocol v1"), then records of `[4][length, 2 bytes BE][ciphertext]`
//! with ChaCha20-Poly1305 and big-endian counters as nonces (Tailscale's
//! `control/controlbase`; the reference's `tailscale_noise.c`).

use alloc::boxed::Box;
use blake2::{Blake2s256, Digest};
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use hmac::{Mac, SimpleHmac};

use crate::keys::{KeyPair, LEN};
use crate::{Error, Result, read_exact, write_all};

const PROTOCOL: &[u8] = b"Noise_IK_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"Tailscale Control Protocol v1";
const VERSION: u16 = 1;
const MSG_INIT: u8 = 1;
const MSG_RESP: u8 = 2;
const MSG_RECORD: u8 = 4;

/// The initiation as sent (base64) in `X-Tailscale-Handshake`.
pub const INIT_LEN: usize = 101;
/// The response, the first bytes after the HTTP 101.
pub const RESP_LEN: usize = 51;
/// A record's most bytes, header and tag included.
const RECORD_MAX: usize = 4096;
const TAG_LEN: usize = 16;
/// Plaintext a record carries at most.
pub const PLAIN_MAX: usize = RECORD_MAX - 3 - TAG_LEN;

pub(crate) fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <SimpleHmac<Blake2s256> as Mac>::new_from_slice(key).unwrap();
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// Noise's HKDF with two outputs.
pub(crate) fn hkdf2(ck: &[u8; 32], ikm: &[u8]) -> ([u8; 32], [u8; 32]) {
    let prk = hmac(ck, &[ikm]);
    let a = hmac(&prk, &[&[1]]);
    let b = hmac(&prk, &[&a, &[2]]);
    (a, b)
}

fn nonce_le(n: u64) -> Nonce {
    let mut b = [0u8; 12];
    b[4..].copy_from_slice(&n.to_le_bytes());
    b.into()
}

fn nonce_be(n: u64) -> Nonce {
    let mut b = [0u8; 12];
    b[4..].copy_from_slice(&n.to_be_bytes());
    b.into()
}

struct Symmetric {
    h: [u8; 32],
    ck: [u8; 32],
    k: [u8; 32],
    n: u64,
}

impl Symmetric {
    fn new() -> Symmetric {
        let h: [u8; 32] = Blake2s256::digest(PROTOCOL).into();
        Symmetric { h, ck: h, k: [0; 32], n: 0 }
    }

    fn mix_hash(&mut self, data: &[u8]) {
        let mut d = Blake2s256::new();
        d.update(self.h);
        d.update(data);
        self.h = d.finalize().into();
    }

    fn mix_key(&mut self, ikm: &[u8]) {
        (self.ck, self.k) = hkdf2(&self.ck, ikm);
        self.n = 0;
    }

    /// Encrypts `buf[..len - 16]` in place, the tag after it.
    fn encrypt_and_hash(&mut self, buf: &mut [u8]) {
        let (pt, tag) = buf.split_at_mut(buf.len() - TAG_LEN);
        let c = ChaCha20Poly1305::new(Key::from_slice(&self.k));
        let t = c.encrypt_in_place_detached(&nonce_le(self.n), &self.h, pt).unwrap();
        tag.copy_from_slice(&t);
        self.n += 1;
        self.mix_hash(buf);
    }

    fn decrypt_and_hash(&mut self, buf: &[u8]) -> Result<()> {
        let mut pt = [0u8; 64];
        let (ct, tag) = buf.split_at(buf.len() - TAG_LEN);
        let pt = &mut pt[..ct.len()];
        pt.copy_from_slice(ct);
        let c = ChaCha20Poly1305::new(Key::from_slice(&self.k));
        c.decrypt_in_place_detached(&nonce_le(self.n), &self.h, pt, Tag::from_slice(tag)).map_err(|_| Error::Noise)?;
        self.n += 1;
        self.mix_hash(buf);
        Ok(())
    }
}

/// A handshake waiting for the server's response.
pub struct Handshake {
    sym: Symmetric,
    eph: KeyPair,
}

/// Starts the handshake: the initiation message for the server whose
/// Noise key is `server`. `eph` is fresh randomness.
pub fn initiate(machine: &KeyPair, server: &[u8; LEN], eph: [u8; LEN]) -> (Handshake, [u8; INIT_LEN]) {
    let eph = KeyPair::from_private(eph);
    let mut s = Symmetric::new();
    s.mix_hash(PROLOGUE);
    s.mix_hash(server);
    s.mix_hash(&eph.public);
    s.mix_key(&eph.dh(server));
    let mut msg = [0u8; INIT_LEN];
    msg[..2].copy_from_slice(&VERSION.to_be_bytes());
    msg[2] = MSG_INIT;
    msg[3..5].copy_from_slice(&96u16.to_be_bytes());
    msg[5..37].copy_from_slice(&eph.public);
    msg[37..69].copy_from_slice(&machine.public);
    s.encrypt_and_hash(&mut msg[37..85]);
    s.mix_key(&machine.dh(server));
    s.encrypt_and_hash(&mut msg[85..101]);
    (Handshake { sym: s, eph }, msg)
}

impl Handshake {
    /// Takes the server's response; the transport keys.
    pub fn finish(mut self, machine: &KeyPair, resp: &[u8; RESP_LEN]) -> Result<Session> {
        if resp[0] != MSG_RESP || u16::from_be_bytes([resp[1], resp[2]]) != 48 {
            return Err(Error::Proto("noise response"));
        }
        let re: [u8; 32] = resp[3..35].try_into().unwrap();
        let s = &mut self.sym;
        s.mix_hash(&re);
        s.mix_key(&self.eph.dh(&re));
        s.mix_key(&machine.dh(&re));
        s.decrypt_and_hash(&resp[35..51])?;
        let (send, recv) = hkdf2(&s.ck, &[]);
        Ok(Session { send: ChaCha20Poly1305::new(&send.into()), recv: ChaCha20Poly1305::new(&recv.into()), send_n: 0, recv_n: 0 })
    }
}

/// The transport keys and counters.
pub struct Session {
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    send_n: u64,
    recv_n: u64,
}

/// A byte stream over Noise records on `io`.
pub struct Conn<S> {
    pub io: S,
    s: Session,
    rbuf: Box<[u8; RECORD_MAX]>,
    rpos: usize,
    rlen: usize,
    wbuf: Box<[u8; RECORD_MAX]>,
}

impl<S: embedded_io::Read + embedded_io::Write> Conn<S> {
    pub fn new(io: S, s: Session) -> Conn<S> {
        Conn { io, s, rbuf: Box::new([0; RECORD_MAX]), rpos: 0, rlen: 0, wbuf: Box::new([0; RECORD_MAX]) }
    }

    fn fill(&mut self) -> Result<()> {
        let mut h = [0u8; 3];
        read_exact(&mut self.io, &mut h)?;
        let len = u16::from_be_bytes([h[1], h[2]]) as usize;
        if h[0] != MSG_RECORD || !(TAG_LEN..=RECORD_MAX - 3).contains(&len) {
            return Err(Error::Proto("noise record"));
        }
        read_exact(&mut self.io, &mut self.rbuf[..len])?;
        let (ct, tag) = self.rbuf[..len].split_at_mut(len - TAG_LEN);
        let tag = Tag::clone_from_slice(tag);
        self.s.recv.decrypt_in_place_detached(&nonce_be(self.s.recv_n), &[], ct, &tag).map_err(|_| Error::Noise)?;
        self.s.recv_n += 1;
        (self.rpos, self.rlen) = (0, len - TAG_LEN);
        Ok(())
    }

    /// Decrypted bytes, or bytes read from below, are waiting.
    pub fn buffered(&self) -> bool
    where
        S: crate::Ready,
    {
        self.rpos < self.rlen || self.io.buffered()
    }

    /// Some bytes (at least one; waits for a record).
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        while self.rpos == self.rlen {
            self.fill()?;
        }
        let n = buf.len().min(self.rlen - self.rpos);
        buf[..n].copy_from_slice(&self.rbuf[self.rpos..self.rpos + n]);
        self.rpos += n;
        Ok(n)
    }

    pub fn read_exact(&mut self, mut buf: &mut [u8]) -> Result<()> {
        while !buf.is_empty() {
            let n = self.read(buf)?;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    /// Skips `n` bytes.
    pub fn skip(&mut self, mut n: usize) -> Result<()> {
        while n > 0 {
            while self.rpos == self.rlen {
                self.fill()?;
            }
            let k = n.min(self.rlen - self.rpos);
            self.rpos += k;
            n -= k;
        }
        Ok(())
    }

    /// Writes `data` as records (not flushed).
    pub fn write_all(&mut self, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(PLAIN_MAX) {
            let len = chunk.len() + TAG_LEN;
            self.wbuf[0] = MSG_RECORD;
            self.wbuf[1..3].copy_from_slice(&(len as u16).to_be_bytes());
            self.wbuf[3..3 + chunk.len()].copy_from_slice(chunk);
            let t = self.s.send.encrypt_in_place_detached(&nonce_be(self.s.send_n), &[], &mut self.wbuf[3..3 + chunk.len()]).unwrap();
            self.s.send_n += 1;
            self.wbuf[3 + chunk.len()..3 + len].copy_from_slice(&t);
            write_all(&mut self.io, &self.wbuf[..3 + len])?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.io.flush().map_err(|_| Error::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server's side of IK, to check the initiator against.
    fn respond(server: &KeyPair, init: &[u8; INIT_LEN], eph: [u8; 32]) -> ([u8; 32], [u8; RESP_LEN], [u8; 32], [u8; 32]) {
        let mut s = Symmetric::new();
        s.mix_hash(PROLOGUE);
        s.mix_hash(&server.public);
        let ie: [u8; 32] = init[5..37].try_into().unwrap();
        s.mix_hash(&ie);
        s.mix_key(&server.dh(&ie));
        // The static key: decrypt, then hash the ciphertext.
        let c = ChaCha20Poly1305::new(Key::from_slice(&s.k));
        let mut is: [u8; 32] = init[37..69].try_into().unwrap();
        c.decrypt_in_place_detached(&nonce_le(s.n), &s.h, &mut is, Tag::from_slice(&init[69..85])).unwrap();
        s.n += 1;
        s.mix_hash(&init[37..85]);
        s.mix_key(&server.dh(&is));
        s.decrypt_and_hash(&init[85..101]).unwrap();
        let re = KeyPair::from_private(eph);
        let mut resp = [0u8; RESP_LEN];
        resp[0] = MSG_RESP;
        resp[1..3].copy_from_slice(&48u16.to_be_bytes());
        resp[3..35].copy_from_slice(&re.public);
        s.mix_hash(&re.public);
        s.mix_key(&re.dh(&ie));
        s.mix_key(&re.dh(&is));
        s.encrypt_and_hash(&mut resp[35..51]);
        let (i2r, r2i) = hkdf2(&s.ck, &[]);
        (is, resp, i2r, r2i)
    }

    #[test]
    fn handshake_and_records() {
        let machine = KeyPair::from_private([7; 32]);
        let server = KeyPair::from_private([9; 32]);
        let (hs, init) = initiate(&machine, &server.public, [3; 32]);
        assert_eq!(&init[..5], &[0, 1, 1, 0, 96]);
        let (is, resp, i2r, _r2i) = respond(&server, &init, [5; 32]);
        assert_eq!(is, machine.public);
        let mut sess = hs.finish(&machine, &resp).unwrap();
        // A record from the client decrypts with the server's receive key.
        let mut pt = *b"hello";
        let tag = sess.send.encrypt_in_place_detached(&nonce_be(0), &[], &mut pt).unwrap();
        let srv = ChaCha20Poly1305::new(&i2r.into());
        srv.decrypt_in_place_detached(&nonce_be(0), &[], &mut pt, &tag).unwrap();
        assert_eq!(&pt, b"hello");
        sess.send_n += 1;
        // A wrong server key fails.
        let other = KeyPair::from_private([11; 32]);
        let (hs, init) = initiate(&machine, &server.public, [3; 32]);
        let (_, mut resp, _, _) = respond(&server, &init, [5; 32]);
        resp[40] ^= 1;
        assert!(hs.finish(&machine, &resp).is_err());
        let _ = other;
    }
}
