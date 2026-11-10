//! The control server's steps, as the reference (`tailscale_control.c`):
//! `GET /key` over TLS for the server's Noise key, then on a second TLS
//! connection `POST /ts2021` with the Noise initiation in a header, the
//! 101 and the Noise response, HTTP/2 over the Noise stream, and on it
//! `/machine/register` and `/machine/map` (a stream of `[length, 4 bytes
//! LE][JSON]` responses).

use alloc::string::String;
use alloc::vec::Vec;
use serde_json::json;

use crate::h2::{Event, H2};
use crate::keys::{self, Keys};
use crate::noise::{self, Conn};
use crate::{Error, Result, read_exact, write_all};

/// The capability version in the HTTP headers and `/key` (the reference's).
pub const CAP_VERSION: u32 = 47;
/// The protocol version in the requests' JSON.
pub const VERSION: u32 = 65;
/// A map response larger than this is refused.
pub const MAP_MAX: usize = 512 << 10;

/// How an HTTP/1.1 body is framed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Framing {
    Length(usize),
    Chunked,
    /// Until the server closes.
    Close,
}

/// Reads one line (CR LF dropped), up to 1 KiB.
fn http_line<S: embedded_io::Read>(s: &mut S, line: &mut Vec<u8>) -> Result<()> {
    line.clear();
    loop {
        let mut c = [0u8];
        read_exact(s, &mut c)?;
        match c[0] {
            b'\r' => {}
            b'\n' => return Ok(()),
            c if line.len() < 1024 => line.push(c),
            _ => return Err(Error::Size),
        }
    }
}

/// Reads an HTTP/1.1 response's status line and headers; the status and
/// how the body is framed.
fn http_head<S: embedded_io::Read>(s: &mut S) -> Result<(u16, Framing)> {
    let mut status = 0;
    let mut length = Framing::Close;
    let mut line = Vec::with_capacity(128);
    let mut first = true;
    loop {
        http_line(s, &mut line)?;
        if line.is_empty() {
            return Ok((status, length));
        }
        let l = core::str::from_utf8(&line).map_err(|_| Error::Proto("http header"))?;
        if first {
            status = l.split(' ').nth(1).and_then(|v| v.parse().ok()).ok_or(Error::Proto("http status"))?;
            first = false;
        } else if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if k.eq_ignore_ascii_case("content-length") {
                length = Framing::Length(v.parse().map_err(|_| Error::Proto("http length"))?);
            } else if k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked") {
                length = Framing::Chunked;
            }
        }
    }
}

/// An HTTP/1.1 response's status, its headers skipped (an upgrade's).
pub(crate) fn http_status<S: embedded_io::Read>(s: &mut S) -> Result<u16> {
    http_head(s).map(|(status, _)| status)
}

/// A whole body of at most `max` bytes.
fn http_body<S: embedded_io::Read>(s: &mut S, framing: Framing, max: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    match framing {
        Framing::Length(n) => {
            if n > max {
                return Err(Error::Size);
            }
            body.resize(n, 0);
            read_exact(s, &mut body)?;
        }
        Framing::Chunked => {
            let mut line = Vec::new();
            loop {
                http_line(s, &mut line)?;
                let hex = core::str::from_utf8(&line).map_err(|_| Error::Proto("http chunk"))?;
                let n = usize::from_str_radix(hex.split(';').next().unwrap_or("").trim(), 16).map_err(|_| Error::Proto("http chunk"))?;
                if n == 0 {
                    break;
                }
                if body.len() + n > max {
                    return Err(Error::Size);
                }
                let at = body.len();
                body.resize(at + n, 0);
                read_exact(s, &mut body[at..])?;
                http_line(s, &mut line)?;
            }
        }
        Framing::Close => {
            let mut b = [0u8; 256];
            loop {
                match s.read(&mut b) {
                    Ok(0) | Err(_) => break,
                    Ok(n) if body.len() + n <= max => body.extend_from_slice(&b[..n]),
                    Ok(_) => return Err(Error::Size),
                }
            }
        }
    }
    Ok(body)
}

/// The server's Noise key (`GET /key`, its `publicKey`), on a TLS stream
/// to `host` that is then done with (`Connection: close`).
pub fn server_key<S: embedded_io::Read + embedded_io::Write>(s: &mut S, host: &str) -> Result<[u8; keys::LEN]> {
    let req = alloc::format!("GET /key?v={CAP_VERSION} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    write_all(s, req.as_bytes())?;
    s.flush().map_err(|_| Error::Io)?;
    let (status, framing) = http_head(s)?;
    if status != 200 {
        return Err(Error::Http(status));
    }
    let body = http_body(s, framing, 1024)?;
    let v: serde_json::Value = serde_json::from_slice(&body).map_err(|_| Error::Json)?;
    v.get("publicKey").and_then(|k| k.as_str()).and_then(keys::from_hex).ok_or(Error::Proto("key"))
}

/// The Noise channel on a fresh TLS stream: the upgrade, the handshake,
/// HTTP/2. `eph` is fresh randomness. Returns the channel and the early
/// payload (JSON) if one came.
pub fn connect<S: embedded_io::Read + embedded_io::Write>(
    mut s: S,
    host: &'static str,
    keys: &Keys,
    server: &[u8; keys::LEN],
    eph: [u8; keys::LEN],
) -> Result<(H2<S>, Option<Vec<u8>>)> {
    let (hs, init) = noise::initiate(&keys.machine, server, eph);
    let req = alloc::format!(
        "POST /ts2021 HTTP/1.1\r\nHost: {host}\r\nUpgrade: tailscale-control-protocol\r\nConnection: upgrade\r\n\
         Tailscale-Cap-Version: {CAP_VERSION}\r\nX-Tailscale-Handshake: {}\r\nContent-Length: 0\r\n\r\n",
        base64(&init)
    );
    write_all(&mut s, req.as_bytes())?;
    s.flush().map_err(|_| Error::Io)?;
    let (status, _) = http_head(&mut s)?;
    if status != 101 {
        return Err(Error::Http(status));
    }
    let mut resp = [0u8; noise::RESP_LEN];
    read_exact(&mut s, &mut resp)?;
    let session = hs.finish(&keys.machine, &resp)?;
    H2::start(Conn::new(s, session), host)
}

/// The Hostinfo this client sends.
fn hostinfo(hostname: &str, derp: i32) -> serde_json::Value {
    let mut h = json!({ "OS": "linux", "Hostname": hostname, "GoArch": "riscv", "GoVersion": "tab5-os" });
    if derp > 0 {
        h["NetInfo"] = json!({ "PreferredDERP": derp, "LinkType": "wifi" });
    }
    h
}

/// What `/machine/register` answered.
#[derive(Debug, Default)]
pub struct Registered {
    /// Where a person must log in (no usable auth key); empty if none.
    pub auth_url: String,
    /// The node is authorized.
    pub authorized: bool,
    pub error: String,
}

/// Registers the node key (with `auth_key` if not empty).
pub fn register<S: embedded_io::Read + embedded_io::Write>(h: &mut H2<S>, keys: &Keys, hostname: &str, auth_key: &str) -> Result<Registered> {
    let node = keys::to_hex("nodekey:", &keys.node.public);
    let mut req = json!({
        "Version": VERSION,
        "NodeKey": node,
        // As the reference: the same key (no rotation).
        "OldNodeKey": node,
        "Hostinfo": hostinfo(hostname, 0),
        "Endpoints": [],
    });
    if !auth_key.is_empty() {
        req["Auth"] = json!({ "AuthKey": auth_key });
    }
    let body = serde_json::to_vec(&req).map_err(|_| Error::Json)?;
    let sid = h.post("/machine/register", &body)?;
    let resp = read_body(h, sid, 64 * 1024)?;
    let v: serde_json::Value = serde_json::from_slice(&resp).map_err(|_| Error::Json)?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").into();
    Ok(Registered { auth_url: s("AuthURL"), authorized: v.get("MachineAuthorized").and_then(|x| x.as_bool()).unwrap_or(false), error: s("Error") })
}

/// A whole response body (status 200 expected).
fn read_body<S: embedded_io::Read + embedded_io::Write>(h: &mut H2<S>, sid: u32, max: usize) -> Result<Vec<u8>> {
    let mut status = 0;
    let mut body = Vec::new();
    loop {
        match h.next()? {
            Event::Headers { sid: s, status: st, end } if s == sid => {
                if status == 0 {
                    status = st;
                }
                if end {
                    break;
                }
            }
            Event::Data { sid: s, end } if s == sid => {
                if body.len() + h.data().len() > max {
                    return Err(Error::Size);
                }
                body.extend_from_slice(h.data());
                if end {
                    break;
                }
            }
            Event::Reset { sid: s } if s == sid => return Err(Error::Proto("stream reset")),
            _ => {}
        }
    }
    h.done(sid);
    if status != 200 {
        return Err(Error::Http(status));
    }
    Ok(body)
}

/// A map stream: `/machine/map` with `Stream: true`, read response by
/// response.
pub struct MapStream {
    pub sid: u32,
    buf: Vec<u8>,
    status: u16,
    /// The first response in `buf` was handed out.
    taken: bool,
}

/// Starts the map stream. `endpoints` are this node's UDP endpoints
/// (`ip:port`), `derp` its home region (0: not chosen yet).
pub fn map<S: embedded_io::Read + embedded_io::Write>(
    h: &mut H2<S>,
    keys: &Keys,
    hostname: &str,
    derp: i32,
    endpoints: &[String],
) -> Result<MapStream> {
    let req = json!({
        "Version": VERSION,
        "NodeKey": keys::to_hex("nodekey:", &keys.node.public),
        "DiscoKey": keys::to_hex("discokey:", &keys.disco.public),
        "Stream": true,
        "Compress": "",
        "KeepAlive": true,
        "IncludeIPv6": false,
        "OmitPeers": false,
        "Hostinfo": hostinfo(hostname, derp),
        "Endpoints": endpoints,
    });
    let body = serde_json::to_vec(&req).map_err(|_| Error::Json)?;
    let sid = h.post("/machine/map", &body)?;
    Ok(MapStream { sid, buf: Vec::new(), status: 0, taken: false })
}

/// Tells the control server this node's home DERP region and endpoints
/// on a stream of its own (`Stream: false`, `OmitPeers: true`: a "lite"
/// update, as the Go client sends when Hostinfo changes). Its response is
/// left unread: `MapStream` skips other streams' frames.
pub fn update<S: embedded_io::Read + embedded_io::Write>(
    h: &mut H2<S>,
    keys: &Keys,
    hostname: &str,
    derp: i32,
    endpoints: &[String],
) -> Result<u32> {
    let req = json!({
        "Version": VERSION,
        "NodeKey": keys::to_hex("nodekey:", &keys.node.public),
        "DiscoKey": keys::to_hex("discokey:", &keys.disco.public),
        "Stream": false,
        "OmitPeers": true,
        "Compress": "",
        "Hostinfo": hostinfo(hostname, derp),
        "Endpoints": endpoints,
    });
    let body = serde_json::to_vec(&req).map_err(|_| Error::Json)?;
    h.post("/machine/map", &body)
}

impl MapStream {
    /// The next response's JSON (keep-alives included), waiting for it.
    pub fn next<'a, S: embedded_io::Read + embedded_io::Write>(&'a mut self, h: &mut H2<S>) -> Result<&'a [u8]> {
        loop {
            if self.poll(h)? {
                return Ok(self.take());
            }
        }
    }

    /// A whole response is waiting (`take` it).
    pub fn ready(&self) -> bool {
        self.whole().is_some() && !self.taken
    }

    /// The waiting response's JSON; the next `poll` drops it.
    pub fn take(&mut self) -> &[u8] {
        let n = self.whole().unwrap();
        self.taken = true;
        &self.buf[4..4 + n]
    }

    /// Reads at most one frame; whether a whole response is waiting.
    pub fn poll<S: embedded_io::Read + embedded_io::Write>(&mut self, h: &mut H2<S>) -> Result<bool> {
        if self.taken {
            let n = self.whole().unwrap();
            self.buf.drain(..4 + n);
            self.taken = false;
        }
        if self.whole().is_some() {
            return Ok(true);
        }
        match h.step()? {
            Some(Event::Headers { sid, status, end }) if sid == self.sid => {
                if self.status == 0 {
                    self.status = status;
                }
                if self.status != 200 {
                    return Err(Error::Http(self.status));
                }
                if end {
                    return Err(Error::Io);
                }
            }
            Some(Event::Data { sid, end }) if sid == self.sid => {
                self.buf.extend_from_slice(h.data());
                if self.buf.len() >= 4 && self.len() > MAP_MAX {
                    return Err(Error::Size);
                }
                if end && self.whole().is_none() {
                    return Err(Error::Io);
                }
            }
            Some(Event::Reset { sid }) if sid == self.sid => return Err(Error::Proto("map reset")),
            _ => {}
        }
        Ok(self.whole().is_some())
    }

    fn len(&self) -> usize {
        u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize
    }

    fn whole(&self) -> Option<usize> {
        (self.buf.len() >= 4 && self.buf.len() >= 4 + self.len()).then(|| self.len())
    }
}

/// Standard base64 with padding.
pub fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;

    struct Bytes<'a>(&'a [u8]);
    impl embedded_io::ErrorType for Bytes<'_> {
        type Error = core::convert::Infallible;
    }
    impl embedded_io::Read for Bytes<'_> {
        fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, Self::Error> {
            let n = buf.len().min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn bodies() {
        let mut r = Bytes(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n");
        let (st, f) = http_head(&mut r).unwrap();
        assert_eq!((st, f), (200, Framing::Chunked));
        assert_eq!(http_body(&mut r, f, 100).unwrap(), b"{\"a\":1}");
        let mut r = Bytes(b"HTTP/1.1 101 Switching Protocols\r\ncontent-length: 2\r\n\r\nok");
        let (st, f) = http_head(&mut r).unwrap();
        assert_eq!((st, f), (101, Framing::Length(2)));
        assert_eq!(http_body(&mut r, f, 1).unwrap_err(), Error::Size);
        let mut r = Bytes(b"HTTP/1.0 200 OK\r\n\r\nrest");
        let (_, f) = http_head(&mut r).unwrap();
        assert_eq!(http_body(&mut r, f, 100).unwrap(), b"rest");
    }
}
