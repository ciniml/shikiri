#![cfg_attr(not(test), no_std)]

//! A Tailscale client's protocols (doc/71), after the reference in C for
//! the Tab5 (portable_terminal's `components/tailscale`): the machine,
//! node and disco keys, the ts2021 Noise IK handshake and its records,
//! HTTP/2 over them, the register and map requests, the netmap. No I/O of
//! its own: the caller passes a stream (`embedded_io::Read + Write`,
//! usually TLS over TCP) and the randomness.
//!
//! Needs an allocator: the map responses are JSON of no fixed size.

extern crate alloc;

pub mod control;
pub mod derp;
pub mod h2;
pub mod ip;
pub mod keys;
pub mod netcheck;
pub mod netmap;
pub mod node;
pub mod noise;
pub mod stream;
pub mod stun;
pub mod tls;
pub mod wg;

/// A stream that can tell whether bytes it read are still waiting.
pub trait Ready {
    fn buffered(&self) -> bool;
}

impl<S> Ready for stream::Buffered<S> {
    fn buffered(&self) -> bool {
        stream::Buffered::buffered(self)
    }
}

/// What went wrong, with the step for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The stream failed or closed.
    Io,
    /// An HTTP status other than the one expected (0: not decoded).
    Http(u16),
    /// The peer broke the protocol (what it got wrong).
    Proto(&'static str),
    /// A Noise record did not decrypt.
    Noise,
    /// JSON that did not parse.
    Json,
    /// Something did not fit.
    Size,
}

pub type Result<T> = core::result::Result<T, Error>;

/// Reads exactly `buf.len()` bytes.
pub(crate) fn read_exact<S: embedded_io::Read>(s: &mut S, mut buf: &mut [u8]) -> Result<()> {
    while !buf.is_empty() {
        match s.read(buf) {
            Ok(0) | Err(_) => return Err(Error::Io),
            Ok(n) => buf = &mut buf[n..],
        }
    }
    Ok(())
}

pub(crate) fn write_all<S: embedded_io::Write>(s: &mut S, mut buf: &[u8]) -> Result<()> {
    while !buf.is_empty() {
        match s.write(buf) {
            Ok(0) | Err(_) => return Err(Error::Io),
            Ok(n) => buf = &buf[n..],
        }
    }
    Ok(())
}
