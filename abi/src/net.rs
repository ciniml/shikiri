// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The network service's protocol (N1, doc/research/net-stack.md §3.4). A
//! client connects to `svc:net` and is granted `SLICE` bytes of shared
//! memory for data; requests and replies are channel messages:
//!
//! - request: `u8 op | u8 family | u16 socket | u32 a | u32 b`, and for
//!   family 6 the IPv6 address (16 bytes) after it, in place of `a`
//!   (`REQUEST_SIZE_V6`; doc/26 §4)
//! - reply: `u32 status | u32 v0 | u32 v1`, and an IPv6 address after it
//!   where the request was IPv6 and the reply carries an address
//!   (UDP_RECV's source, RESOLVE_RESULT's answer; `REPLY_SIZE_V6`)
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
//! | RESOLVE | – | name bytes from the slice (≤ `NAME_MAX`) | socket (one DNS lookup: an A record, AAAA with family 6) |
//! | RESOLVE_RESULT | – | – | 1 once answered / IPv4 (`UNREACHABLE`: failed or no record); IPv6 after the reply |
//! | IF_INFO | interface (0, 1, …) | – | 1 if it is up (0: not there now) / bytes put in the slice (`iface`; doc/41) |
//! | UDP_JOIN | multicast group (IPv4, or IPv6 with family 6) | – | – (the UDP socket's links join the group; doc/49) |
//! | UDP_WAIT | – | – | 1 once a datagram waits on one of the client's UDP sockets / – (doc/49 §18) |
//! | LINK_RENEW | 1 if another network (0: the same one again) | `link` (WLAN0) | – (the Wi-Fi service associated: multicast memberships are reported again, doc/49 §22; with 1 DHCP also starts over, §20) |
//!
//! UDP_WAIT is the one request whose reply can come later: when a datagram
//! waits on any of the client's UDP sockets (at once if one already does),
//! or, with 0, just before the reply to the client's next request. So the
//! replies stay in the order of the requests, and a client can `wait` on
//! its channel (with its other handles) instead of polling.
//!
//! TCP_LISTEN and UDP_BIND (doc/23 §5): with address 0 and `link::ANY` the
//! socket listens on every link (a TCP listener becomes the connection that
//! comes first, on whichever link). A local address limits it to the link
//! that has that address (`NO_ADDRESS` if none); a link limits it to that
//! link.
//!
//! IF_INFO answers `NO_ADDRESS` for an interface number past the last.
//!
//! PING and RESOLVE sockets are freed with CLOSE like the others. Names are
//! resolved on the Wi-Fi link only, with the DNS servers DHCP gave.

pub const SERVICE: &str = "net";
pub const SLICE: usize = 64 << 10;
pub const REQUEST_SIZE: usize = 12;
pub const REPLY_SIZE: usize = 12;
pub const REQUEST_SIZE_V6: usize = 28;
pub const REPLY_SIZE_V6: usize = 28;
/// A request's family byte: IPv6 (anything else is IPv4).
pub const FAMILY_V6: u8 = 6;
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
    pub const IF_INFO: u8 = 14;
    pub const UDP_JOIN: u8 = 15;
    pub const UDP_WAIT: u8 = 16;
    pub const LINK_RENEW: u8 = 17;
    /// `svc:ts` only (doc/71 §14.8): `a` = 0 this node, 1.. the peers.
    pub const PEER_INFO: u8 = 18;
}

/// IF_INFO's record in the slice (doc/41): `name[8]` (NUL-padded) | `mac[6]`
/// (zero for a link without one) | `u8` IPv4 prefix | `u8` IPv6 addresses
/// | `u32` IPv4 address | `u32` router | `u32` DNS server (0: none) | `u32`
/// frames received | `u32` frames sent | then per IPv6 address: `[u8; 16]`
/// address | `u8` prefix | 3 zero bytes.
pub mod iface {
    pub const HEADER: usize = 36;
    pub const V6_ENTRY: usize = 20;
    /// IPv6 addresses a record holds at most.
    pub const V6_MAX: usize = 4;
    pub const RECORD_MAX: usize = HEADER + V6_ENTRY * V6_MAX;
}

/// PEER_INFO's record in the slice: `name[48]` (the netmap's name up to
/// the first dot, NUL-padded) | `u32` IPv4 address | `u8` flags | `u8` 0 |
/// `u16` direct path's port | `u32` direct path's IPv4 address | `region[8]`
/// (home DERP region's code, NUL-padded) | `u32` seconds since packets
/// last went or came (`u32::MAX`: never) | `[u8; 16]` direct path's IPv6
/// address (zero for an IPv4 path, whose address is the `u32` before).
/// Numbers little-endian, addresses as IF_INFO's. `NO_ADDRESS` past the
/// last peer.
pub mod peer {
    pub const RECORD: usize = 88;
    pub const NAME: usize = 48;
    pub const SELF: u8 = 1 << 0;
    /// The control server says it is online.
    pub const ONLINE: u8 = 1 << 1;
    /// The control server says it is offline.
    pub const OFFLINE: u8 = 1 << 2;
    /// Sent to over UDP now (else through DERP).
    pub const DIRECT: u8 = 1 << 3;
    /// Not `DIRECT`, but the path fields hold the last direct path, which
    /// lapsed (no traffic: doc/71 §14.5).
    pub const WAS_DIRECT: u8 = 1 << 4;
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
    /// The client's grants do not allow the request (doc/52 §3.3).
    pub const DENIED: u32 = 8;
}

/// What a client's grants (`svc:net/GRANT`, doc/52 §3.3) allow: `udp:PORT`
/// or `udp:any` (UDP_BIND), `tcp-listen:PORT` or `tcp-listen:any`,
/// `tcp-connect` (any port) or `tcp-connect:PORT`, `mcast` (UDP_JOIN),
/// `ping`, `resolve`, `info` (IF_INFO), `link` (LINK_RENEW, system key
/// only). Requests on the client's own sockets need nothing more.
pub mod grant {}

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
    /// The wired Ethernet (doc/58 R2).
    pub const ETH0: u8 = 5;
    /// The Wi-Fi chip's soft-AP (doc/58 R4).
    pub const AP0: u8 = 6;
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

/// An IPv6 request (`a` is the address).
pub fn request6(op: u8, socket: u16, addr: [u8; 16], b: u32) -> [u8; REQUEST_SIZE_V6] {
    let mut r = [0u8; REQUEST_SIZE_V6];
    r[..REQUEST_SIZE].copy_from_slice(&request(op, socket, 0, b));
    r[1] = FAMILY_V6;
    r[REQUEST_SIZE..].copy_from_slice(&addr);
    r
}

pub fn request(op: u8, socket: u16, a: u32, b: u32) -> [u8; REQUEST_SIZE] {
    let mut r = [0u8; REQUEST_SIZE];
    r[0] = op;
    r[2..4].copy_from_slice(&socket.to_le_bytes());
    r[4..8].copy_from_slice(&a.to_le_bytes());
    r[8..12].copy_from_slice(&b.to_le_bytes());
    r
}

/// The network service's control channel (N4, doc/24 §4): `svc:netctl`,
/// for the filter's rules. One client at a time; needs `--permit
/// svc:netctl`. A request is `u8 op` and, for LINE, one line of rule text
/// (doc/24 §3); the reply is `u32 status | u32 v0 | u32 v1` as for `svc:net`.
///
/// | op | reply v0 |
/// |---|---|
/// | BEGIN | – (starts a new rule set) |
/// | LINE | – |
/// | COMMIT | rules in the set; `BAD_REQUEST` with v0 = the first bad line |
/// | CLEAR | – (no rules, every policy pass) |
/// | STATS | – (the service logs each rule's count) |
///
/// COMMIT puts the whole set in force between two packets.
pub mod ctl {
    pub const SERVICE: &str = "netctl";
    pub const BEGIN: u8 = 1;
    pub const LINE: u8 = 2;
    pub const COMMIT: u8 = 3;
    pub const CLEAR: u8 = 4;
    pub const STATS: u8 = 5;
}
