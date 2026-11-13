// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! A DERP client (Tailscale's relay; the reference's `tailscale_derp.c`,
//! tailscale.com/derp): `GET /derp` upgraded on TLS, the server's key, our
//! node key and a NaCl box of `{"version":2}`, the server's info, then
//! frames of `[type][length, 4 bytes BE][payload]`. Packets go to and come
//! from node keys: `SendPacket` (destination key, packet), `RecvPacket`
//! (source key, packet).

use alloc::vec;
use alloc::vec::Vec;
use crypto_box::aead::AeadInPlace;
use crypto_box::{PublicKey, SalsaBox, SecretKey};

use crate::control::http_status;
use crate::keys::{KeyPair, LEN};
use crate::{Error, Result, read_exact, write_all};

const SERVER_KEY: u8 = 0x01;
const CLIENT_INFO: u8 = 0x02;
const SERVER_INFO: u8 = 0x03;
const SEND_PACKET: u8 = 0x04;
const RECV_PACKET: u8 = 0x05;
const KEEP_ALIVE: u8 = 0x06;
const NOTE_PREFERRED: u8 = 0x07;
const PEER_GONE: u8 = 0x08;
const PING: u8 = 0x12;
const PONG: u8 = 0x13;

/// "DERP🔑" before the server's key.
const MAGIC: &[u8; 8] = b"DERP\xF0\x9F\x94\x91";
/// The largest packet DERP carries, and its frame.
pub const PACKET_MAX: usize = 64 << 10;
const FRAME_MAX: usize = PACKET_MAX + LEN;

/// What `read` got.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    /// A packet from a node key.
    Packet { src: [u8; LEN], data: &'a [u8] },
    KeepAlive,
    /// A peer we sent to left the server.
    PeerGone,
    /// Anything else (a ping answered here, health, restarting).
    Other(u8),
}

pub struct Derp<S> {
    pub io: S,
    frame: Vec<u8>,
}

impl<S: embedded_io::Read + embedded_io::Write> Derp<S> {
    /// Upgrades a fresh TLS stream to `host` to DERP and logs in with the
    /// node key. `nonce` is fresh randomness (the box's).
    pub fn connect(mut io: S, host: &str, node: &KeyPair, nonce: [u8; 24]) -> Result<Derp<S>> {
        let req = alloc::format!("GET /derp HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: DERP\r\n\r\n");
        write_all(&mut io, req.as_bytes())?;
        io.flush().map_err(|_| Error::Io)?;
        let status = http_status(&mut io)?;
        if status != 101 {
            return Err(Error::Http(status));
        }
        let mut d = Derp { io, frame: vec![0; FRAME_MAX] };
        let (ty, len) = d.read_frame()?;
        if ty != SERVER_KEY || len < 40 || d.frame[..8] != *MAGIC {
            return Err(Error::Proto("derp server key"));
        }
        let server: [u8; LEN] = d.frame[8..40].try_into().unwrap();

        // Our key, the nonce, the box (tag, then ciphertext: NaCl's order).
        let info = b"{\"version\":2}";
        let mut msg = Vec::with_capacity(LEN + 24 + 16 + info.len());
        msg.extend_from_slice(&node.public);
        msg.extend_from_slice(&nonce);
        let mut ct = info.to_vec();
        let b = SalsaBox::new(&PublicKey::from(server), &SecretKey::from(node.private));
        let tag = b.encrypt_in_place_detached(&nonce.into(), b"", &mut ct).map_err(|_| Error::Proto("derp box"))?;
        msg.extend_from_slice(&tag);
        msg.extend_from_slice(&ct);
        d.send_frame(CLIENT_INFO, &[&msg])?;
        d.io.flush().map_err(|_| Error::Io)?;

        let (ty, _) = d.read_frame()?;
        if ty != SERVER_INFO {
            return Err(Error::Proto("derp server info"));
        }
        // This is our home region: peers' packets for us come here.
        d.send_frame(NOTE_PREFERRED, &[&[1]])?;
        d.io.flush().map_err(|_| Error::Io)?;
        Ok(d)
    }

    fn read_frame(&mut self) -> Result<(u8, usize)> {
        let mut h = [0u8; 5];
        read_exact(&mut self.io, &mut h)?;
        let len = u32::from_be_bytes(h[1..5].try_into().unwrap()) as usize;
        if len > FRAME_MAX {
            return Err(Error::Proto("derp frame size"));
        }
        read_exact(&mut self.io, &mut self.frame[..len])?;
        Ok((h[0], len))
    }

    fn send_frame(&mut self, ty: u8, parts: &[&[u8]]) -> Result<()> {
        let len: usize = parts.iter().map(|p| p.len()).sum();
        let mut h = [0u8; 5];
        h[0] = ty;
        h[1..].copy_from_slice(&(len as u32).to_be_bytes());
        write_all(&mut self.io, &h)?;
        for p in parts {
            write_all(&mut self.io, p)?;
        }
        Ok(())
    }

    /// Reads one frame (answering pings).
    pub fn read(&mut self) -> Result<Frame<'_>> {
        let (ty, len) = self.read_frame()?;
        Ok(match ty {
            RECV_PACKET if len >= LEN => Frame::Packet { src: self.frame[..LEN].try_into().unwrap(), data: &self.frame[LEN..len] },
            KEEP_ALIVE => Frame::KeepAlive,
            PEER_GONE => Frame::PeerGone,
            PING if len == 8 => {
                let p: [u8; 8] = self.frame[..8].try_into().unwrap();
                self.send_frame(PONG, &[&p])?;
                self.io.flush().map_err(|_| Error::Io)?;
                Frame::Other(PING)
            }
            t => Frame::Other(t),
        })
    }

    /// Sends `packet` to the node key `dst`.
    pub fn send(&mut self, dst: &[u8; LEN], packet: &[u8]) -> Result<()> {
        if packet.len() > PACKET_MAX {
            return Err(Error::Size);
        }
        self.send_frame(SEND_PACKET, &[dst, packet])?;
        self.io.flush().map_err(|_| Error::Io)
    }
}

impl<S: crate::Ready> Derp<S> {
    /// Bytes of the next frame have been read already.
    pub fn buffered(&self) -> bool {
        self.io.buffered()
    }
}
