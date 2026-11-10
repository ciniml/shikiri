//! Just enough of an HTTP/2 client for the control server, over the Noise
//! stream: requests as POSTs (HPACK literals, no Huffman), responses read
//! frame by frame, the `:status` from the static table. Flow control: the
//! windows open to 16 MiB at the start and are topped up as data is read
//! (the reference left them at 64 KiB: a long map stream stalls there).

use alloc::vec;
use alloc::vec::Vec;

use crate::noise::Conn;
use crate::{Error, Result};

const DATA: u8 = 0;
const HEADERS: u8 = 1;
const RST_STREAM: u8 = 3;
const SETTINGS: u8 = 4;
const PING: u8 = 6;
const GOAWAY: u8 = 7;
const WINDOW_UPDATE: u8 = 8;
const CONTINUATION: u8 = 9;

const END_STREAM: u8 = 1;
const ACK: u8 = 1;
const END_HEADERS: u8 = 4;
const PADDED: u8 = 8;
const PRIORITY: u8 = 0x20;

const SETTINGS_INITIAL_WINDOW_SIZE: u16 = 4;
/// The windows this side opens.
const WINDOW: u32 = 1 << 24;
/// Read bytes owed back before a WINDOW_UPDATE.
const TOP_UP: u32 = 1 << 22;
/// The default SETTINGS_MAX_FRAME_SIZE (we do not raise it).
const FRAME_MAX: usize = 16384;

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
/// Tailscale's early payload before the server's first frame: this magic,
/// a 4-byte length, JSON.
const EARLY_MAGIC: &[u8] = b"\xff\xff\xffTS";

/// What `next` read for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The response headers: the status (0: not decoded).
    Headers { sid: u32, status: u16, end: bool },
    /// Body bytes, in `H2::data()`.
    Data { sid: u32, end: bool },
    /// The server reset the stream.
    Reset { sid: u32 },
}

pub struct H2<S> {
    pub conn: Conn<S>,
    next_sid: u32,
    frame: Vec<u8>,
    data: (usize, usize),
    headers: Vec<u8>,
    owed_conn: u32,
    owed: Vec<(u32, u32)>,
    authority: &'static str,
}

impl<S: embedded_io::Read + embedded_io::Write> H2<S> {
    /// The preface and settings both ways. Returns the early payload's
    /// JSON, if the server sent one.
    pub fn start(conn: Conn<S>, authority: &'static str) -> Result<(H2<S>, Option<Vec<u8>>)> {
        let mut h = H2 {
            conn,
            next_sid: 1,
            frame: vec![0; FRAME_MAX],
            data: (0, 0),
            headers: Vec::new(),
            owed_conn: 0,
            owed: Vec::new(),
            authority,
        };
        let mut settings = [0u8; 6];
        settings[..2].copy_from_slice(&SETTINGS_INITIAL_WINDOW_SIZE.to_be_bytes());
        settings[2..].copy_from_slice(&WINDOW.to_be_bytes());
        h.conn.write_all(PREFACE)?;
        h.send(SETTINGS, 0, 0, &settings)?;
        h.send(WINDOW_UPDATE, 0, 0, &(WINDOW - 65535).to_be_bytes())?;
        h.conn.flush()?;
        let mut head = [0u8; 9];
        h.conn.read_exact(&mut head)?;
        let mut early = None;
        if head[..5] == *EARLY_MAGIC {
            let len = u32::from_be_bytes(head[5..9].try_into().unwrap()) as usize;
            if len > 1 << 16 {
                return Err(Error::Proto("early payload"));
            }
            let mut e = vec![0; len];
            h.conn.read_exact(&mut e)?;
            early = Some(e);
            h.conn.read_exact(&mut head)?;
        }
        // The server's SETTINGS first.
        let (len, ty, flags, _) = parse_head(&head);
        if ty != SETTINGS || flags & ACK != 0 || len > FRAME_MAX {
            return Err(Error::Proto("h2 settings"));
        }
        h.conn.skip(len)?;
        h.send(SETTINGS, ACK, 0, &[])?;
        h.conn.flush()?;
        Ok((h, early))
    }

    fn send(&mut self, ty: u8, flags: u8, sid: u32, payload: &[u8]) -> Result<()> {
        let mut head = [0u8; 9];
        head[..3].copy_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
        head[3] = ty;
        head[4] = flags;
        head[5..].copy_from_slice(&(sid & 0x7FFF_FFFF).to_be_bytes());
        // One write: one record for small frames.
        if payload.len() <= 512 {
            let mut b = [0u8; 9 + 512];
            b[..9].copy_from_slice(&head);
            b[9..9 + payload.len()].copy_from_slice(payload);
            return self.conn.write_all(&b[..9 + payload.len()]);
        }
        self.conn.write_all(&head)?;
        self.conn.write_all(payload)
    }

    /// POSTs `body` (JSON) to `path`; the stream's id.
    pub fn post(&mut self, path: &str, body: &[u8]) -> Result<u32> {
        let sid = self.next_sid;
        self.next_sid += 2;
        let mut hb = Vec::with_capacity(128);
        hb.push(0x83); // :method POST
        hb.push(0x87); // :scheme https
        literal(&mut hb, 4, path.as_bytes()); // :path
        literal(&mut hb, 1, self.authority.as_bytes()); // :authority
        literal(&mut hb, 31, b"application/json"); // content-type
        let mut num = [0u8; 10];
        literal(&mut hb, 28, decimal(body.len(), &mut num)); // content-length
        self.send(HEADERS, END_HEADERS | if body.is_empty() { END_STREAM } else { 0 }, sid, &hb)?;
        let mut chunks = body.chunks(FRAME_MAX).peekable();
        while let Some(c) = chunks.next() {
            self.send(DATA, if chunks.peek().is_none() { END_STREAM } else { 0 }, sid, c)?;
        }
        self.conn.flush()?;
        Ok(sid)
    }

    /// The body bytes of the last `Event::Data`.
    pub fn data(&self) -> &[u8] {
        &self.frame[self.data.0..self.data.1]
    }

    /// The next frame for a stream, answering the connection's own.
    pub fn next(&mut self) -> Result<Event> {
        loop {
            if let Some(e) = self.step()? {
                return Ok(e);
            }
        }
    }

    /// Reads one frame: a stream's event, or `None` for the connection's
    /// own frames (answered here).
    pub fn step(&mut self) -> Result<Option<Event>> {
        {
            let mut head = [0u8; 9];
            self.conn.read_exact(&mut head)?;
            let (len, ty, flags, sid) = parse_head(&head);
            if len > FRAME_MAX {
                return Err(Error::Proto("h2 frame size"));
            }
            self.conn.read_exact(&mut self.frame[..len])?;
            match ty {
                SETTINGS => {
                    if flags & ACK == 0 {
                        self.send(SETTINGS, ACK, 0, &[])?;
                        self.conn.flush()?;
                    }
                }
                PING => {
                    if flags & ACK == 0 && len == 8 {
                        let mut p = [0u8; 8];
                        p.copy_from_slice(&self.frame[..8]);
                        self.send(PING, ACK, 0, &p)?;
                        self.conn.flush()?;
                    }
                }
                GOAWAY => return Err(Error::Proto("h2 goaway")),
                RST_STREAM => return Ok(Some(Event::Reset { sid })),
                HEADERS | CONTINUATION => {
                    let (mut a, mut b) = (0, len);
                    if ty == HEADERS {
                        if flags & PADDED != 0 {
                            let pad = *self.frame.first().ok_or(Error::Proto("h2 padding"))? as usize;
                            a += 1;
                            b = b.checked_sub(pad).ok_or(Error::Proto("h2 padding"))?;
                        }
                        if flags & PRIORITY != 0 {
                            a += 5;
                        }
                        self.headers.clear();
                    }
                    if a > b {
                        return Err(Error::Proto("h2 headers"));
                    }
                    self.headers.extend_from_slice(&self.frame[a..b]);
                    if flags & END_HEADERS != 0 {
                        return Ok(Some(Event::Headers { sid, status: status(&self.headers), end: flags & END_STREAM != 0 }));
                    }
                }
                DATA => {
                    self.owe(sid, len as u32)?;
                    let (mut a, mut b) = (0, len);
                    if flags & PADDED != 0 {
                        let pad = *self.frame.first().ok_or(Error::Proto("h2 padding"))? as usize;
                        a = 1;
                        b = b.checked_sub(pad).ok_or(Error::Proto("h2 padding"))?;
                    }
                    if a > b {
                        return Err(Error::Proto("h2 data"));
                    }
                    self.data = (a, b);
                    return Ok(Some(Event::Data { sid, end: flags & END_STREAM != 0 }));
                }
                // WINDOW_UPDATE (requests are small), PRIORITY, PUSH_PROMISE
                // (never enabled), unknown: ignored.
                _ => {}
            }
        }
        Ok(None)
    }

    /// Counts `n` bytes read on `sid`; gives them back past `TOP_UP`.
    fn owe(&mut self, sid: u32, n: u32) -> Result<()> {
        self.owed_conn += n;
        let i = match self.owed.iter().position(|o| o.0 == sid) {
            Some(i) => i,
            None => {
                self.owed.push((sid, 0));
                self.owed.len() - 1
            }
        };
        self.owed[i].1 += n;
        let mut sent = false;
        if self.owed[i].1 >= TOP_UP {
            let k = core::mem::take(&mut self.owed[i].1);
            self.send(WINDOW_UPDATE, 0, sid, &k.to_be_bytes())?;
            sent = true;
        }
        if self.owed_conn >= TOP_UP {
            let k = core::mem::take(&mut self.owed_conn);
            self.send(WINDOW_UPDATE, 0, 0, &k.to_be_bytes())?;
            sent = true;
        }
        // The TLS below holds writes until flushed.
        if sent { self.conn.flush() } else { Ok(()) }
    }

    /// Forgets a finished stream's flow-control count.
    pub fn done(&mut self, sid: u32) {
        self.owed.retain(|o| o.0 != sid);
    }
}

impl<S: embedded_io::Read + embedded_io::Write + crate::Ready> H2<S> {
    /// Bytes of the next frame have been read already (the Noise records
    /// or the buffer under them hold some): `step` will not wait long.
    pub fn buffered(&self) -> bool {
        self.conn.buffered()
    }
}

fn parse_head(h: &[u8; 9]) -> (usize, u8, u8, u32) {
    let len = u32::from_be_bytes([0, h[0], h[1], h[2]]) as usize;
    (len, h[3], h[4], u32::from_be_bytes(h[5..9].try_into().unwrap()) & 0x7FFF_FFFF)
}

/// A literal with incremental indexing, its name from the static table.
fn literal(out: &mut Vec<u8>, name: u8, value: &[u8]) {
    out.push(0x40 | name);
    integer(out, 0, 7, value.len());
    out.extend_from_slice(value);
}

/// An HPACK integer with an `n`-bit prefix (first byte's high bits `flags`).
fn integer(out: &mut Vec<u8>, flags: u8, n: u32, v: usize) {
    let max = (1usize << n) - 1;
    if v < max {
        out.push(flags | v as u8);
        return;
    }
    out.push(flags | max as u8);
    let mut v = v - max;
    while v >= 128 {
        out.push((v % 128) as u8 | 0x80);
        v /= 128;
    }
    out.push(v as u8);
}

fn decimal(mut v: usize, buf: &mut [u8; 10]) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            return &buf[i..];
        }
    }
}

/// The `:status`: the first field of a response's header block (pseudo
/// headers come first), indexed from the static table (200, 204, 206, 304,
/// 400, 404, 500) or a literal with that name and three plain digits; 0 if
/// neither (a Huffman-coded status: still not 2xx as expected).
fn status(h: &[u8]) -> u16 {
    const STATIC: [u16; 7] = [200, 204, 206, 304, 400, 404, 500];
    let mut i = 0;
    // Dynamic table size updates may come before.
    while h.get(i).is_some_and(|b| b & 0xE0 == 0x20) {
        i += 1;
    }
    let Some(&b) = h.get(i) else { return 0 };
    if b & 0x80 != 0 {
        let idx = (b & 0x7F) as usize;
        return if (8..=14).contains(&idx) { STATIC[idx - 8] } else { 0 };
    }
    let name = if b & 0x40 != 0 { b & 0x3F } else { b & 0x0F };
    if !(8..=14).contains(&name) {
        return 0;
    }
    match h.get(i + 1..i + 5) {
        Some([3, d @ ..]) if d.iter().all(u8::is_ascii_digit) => d.iter().fold(0, |v, c| v * 10 + (c - b'0') as u16),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses() {
        assert_eq!(status(&[0x88]), 200);
        assert_eq!(status(&[0x20, 0x8d]), 404);
        assert_eq!(status(&[0x48, 3, b'4', b'0', b'1', 0x5f]), 401);
        assert_eq!(status(&[0x08, 3, b'1', b'0', b'1']), 101);
        assert_eq!(status(&[0x48, 0x82, 0x68, 0x01]), 0);
        assert_eq!(status(&[]), 0);
    }

    #[test]
    fn integers() {
        let mut v = alloc::vec::Vec::new();
        integer(&mut v, 0, 7, 10);
        integer(&mut v, 0, 7, 127);
        integer(&mut v, 0, 5, 1337);
        assert_eq!(v, [10, 127, 0, 31, 154, 10]);
    }
}
