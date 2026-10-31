//! The network service's protocol (N1, doc/research/net-stack.md §3.4). A
//! client connects to `svc:net` and is granted `SLICE` bytes of shared
//! memory for data; requests and replies are channel messages:
//!
//! - request: `u8 op | u8 0 | u16 socket | u32 a | u32 b`
//! - reply: `u32 status | u32 v0 | u32 v1`
//!
//! Nothing blocks in the service: a SEND or RECV that cannot proceed now
//! answers 0 bytes, and the client tries again.
//!
//! | op | a | b | v0 / v1 |
//! |---|---|---|---|
//! | TCP_LISTEN | local IPv4 (0: any) | port \| `link` << 16 | socket |
//! | TCP_CONNECT | IPv4 | port | socket |
//! | TCP_STATE | – | – | `state` / bytes waiting to be read |
//! | SEND | – | bytes from the slice | bytes taken |
//! | RECV | – | at most | bytes put in the slice / 1 if the peer closed and nothing is left |
//! | CLOSE | – | – | – |
//! | UDP_BIND | local IPv4 (0: any) | port \| `link` << 16 | socket |
//! | UDP_SEND | IPv4 | port << 16 \| bytes | bytes sent (0: queue full) |
//! | UDP_RECV | – | – | bytes at slice + `UDP_HEADER` / source IPv4; the source port is the slice's first u16 |
//! | PING | IPv4 | echo data bytes from the slice (≤ 64) | socket (one echo request sent) |
//! | PING_RESULT | – | – | 1 once the reply came / round trip in µs |
//! | RESOLVE | – | name bytes from the slice (≤ `NAME_MAX`) | socket (one DNS lookup for an A record) |
//! | RESOLVE_RESULT | – | – | 1 once answered / IPv4 (`UNREACHABLE`: failed or no A record) |
//!
//! TCP_LISTEN and UDP_BIND (doc/23 §5): with address 0 and `link::ANY` the
//! socket listens on every link (a TCP listener becomes the connection that
//! comes first, on whichever link). A local address limits it to the link
//! that has that address (`NO_ADDRESS` if none); a link limits it to that
//! link.
//!
//! PING and RESOLVE sockets are freed with CLOSE like the others. Names are
//! resolved on the Wi-Fi link only, with the DNS servers DHCP gave.

pub const SERVICE: &str = "net";
pub const SLICE: usize = 64 << 10;
pub const REQUEST_SIZE: usize = 12;
pub const REPLY_SIZE: usize = 12;
/// UDP_RECV puts the source port here, the payload after it.
pub const UDP_HEADER: usize = 4;
/// Longest name RESOLVE takes.
pub const NAME_MAX: usize = 255;

pub mod op {
    pub const TCP_LISTEN: u8 = 1;
    pub const TCP_CONNECT: u8 = 2;
    pub const TCP_STATE: u8 = 3;
    pub const SEND: u8 = 4;
    pub const RECV: u8 = 5;
    pub const CLOSE: u8 = 6;
    pub const UDP_BIND: u8 = 7;
    pub const UDP_SEND: u8 = 8;
    pub const UDP_RECV: u8 = 9;
    pub const PING: u8 = 10;
    pub const PING_RESULT: u8 = 11;
    pub const RESOLVE: u8 = 12;
    pub const RESOLVE_RESULT: u8 = 13;
}

pub mod status {
    pub const OK: u32 = 0;
    pub const BAD_REQUEST: u32 = 1;
    pub const NO_SOCKETS: u32 = 2;
    pub const ADDR_IN_USE: u32 = 3;
    pub const NOT_CONNECTED: u32 = 4;
    pub const UNREACHABLE: u32 = 5;
    pub const OTHER: u32 = 6;
    /// No link has the local address asked for, or the link is not there.
    pub const NO_ADDRESS: u32 = 7;
}

/// Links for TCP_LISTEN and UDP_BIND.
pub mod link {
    pub const ANY: u8 = 0;
    /// 127.0.0.1.
    pub const LO: u8 = 1;
    /// The host link to the PC (10.55.0.2).
    pub const HOST: u8 = 2;
    pub const WLAN0: u8 = 3;
    /// The test LAN inside the service (doc/23 §4).
    pub const TEST0: u8 = 4;
}

/// TCP_STATE values.
pub mod state {
    pub const CLOSED: u32 = 0;
    pub const LISTEN: u32 = 1;
    pub const CONNECTING: u32 = 2;
    pub const ESTABLISHED: u32 = 3;
    pub const CLOSING: u32 = 4;
}

pub const fn ipv4(a: u8, b: u8, c: u8, d: u8) -> u32 {
    u32::from_be_bytes([a, b, c, d])
}

pub fn request(op: u8, socket: u16, a: u32, b: u32) -> [u8; REQUEST_SIZE] {
    let mut r = [0u8; REQUEST_SIZE];
    r[0] = op;
    r[2..4].copy_from_slice(&socket.to_le_bytes());
    r[4..8].copy_from_slice(&a.to_le_bytes());
    r[8..12].copy_from_slice(&b.to_le_bytes());
    r
}
