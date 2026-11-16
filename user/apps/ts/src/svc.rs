// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! `svc:ts` (doc/71 §13): what only `ts` knows of the tailnet, in
//! `svc:net`'s protocol (`t5_abi::net`), so `Net::connect_to("ts")` serves
//! it unchanged. Of its requests: RESOLVE / RESOLVE_RESULT for the
//! netmap's names as MagicDNS gives them (`NetMap::resolve`: whole names,
//! search domains, short names, IPv4), PEER_INFO (this node and the peers:
//! name, address, path; doc/71 §14.8), LOGIN_INFO (whether on the tailnet,
//! the login URL while a person has to approve the node; §14.15). The rest
//! answer BAD_REQUEST: the tailnet's sockets are `svc:net`'s, on its link
//! `ts0` (doc/72; `ts`'s own smoltcp went in §17).
//!
//! Served between sessions too (connecting, waiting for the login, the
//! nap before a retry): then LOGIN_INFO answers, PEER_INFO finds nothing,
//! RESOLVE answers NOT_CONNECTED.
//!
//! Grants (`svc:ts/GRANT`): `resolve`, `info` (PEER_INFO, LOGIN_INFO).
//!
//! The shared memory also holds `net`'s link `ts0` (`link_at`).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use t5_rt::net::{self as proto, op, status};
use t5_rt::{Channel, Grants, Handle, Listener, Shm, event};
use tsclient::node::Node;

/// Clients at a time, each with `proto::SLICE` of shared memory.
pub const CLIENTS: usize = 2;

struct Client {
    chan: Channel,
    grants: Grants,
    /// RESOLVE's results, by socket number - 1.
    resolved: Vec<Option<Option<[u8; 4]>>>,
}

/// Where the node is in joining the tailnet (LOGIN_INFO).
pub enum Login {
    Connecting,
    /// A person has to open the URL and approve the node.
    Url(String),
    Joined,
}

pub struct Service {
    pub login: Login,
    listener: Listener,
    pool_shm: Shm,
    pool: &'static mut [u8],
    /// `net`'s link `ts0` (doc/72) after the clients' slices: one window
    /// for both (a process maps two; `svc:net`'s is the other).
    pub link_at: usize,
    clients: [Option<Client>; CLIENTS],
}

fn reply(st: u32, v0: u32, v1: u32) -> [u8; proto::REPLY_SIZE] {
    let mut r = [0u8; proto::REPLY_SIZE];
    r[0..4].copy_from_slice(&st.to_le_bytes());
    r[4..8].copy_from_slice(&v0.to_le_bytes());
    r[8..12].copy_from_slice(&v1.to_le_bytes());
    r
}

impl Service {
    pub fn open() -> Option<Service> {
        let link_at = CLIENTS * proto::SLICE;
        let pool_shm = Shm::alloc(link_at + t5_rt::netlink::SIZE).ok()?;
        let pool = pool_shm.map().ok()?;
        let listener = Listener::open("ts").ok()?;
        Some(Service { login: Login::Connecting, listener, pool_shm, pool, link_at, clients: [const { None }; CLIENTS] })
    }

    /// Grants `ts0`'s rings to `net` on `c` (`svc:netlink`); their address
    /// here.
    pub fn grant_link(&self, c: &Channel) -> Option<usize> {
        self.pool_shm.grant(c, self.link_at, t5_rt::netlink::SIZE).ok()?;
        Some(self.pool.as_ptr() as usize + self.link_at)
    }

    /// The handles to `wait` on: the listener, then the clients.
    pub fn handles(&self) -> Vec<&Handle> {
        let mut h = vec![&self.listener.0];
        h.extend(self.clients.iter().flatten().map(|c| &c.chan.0));
        h
    }

    /// `wait` on `handles()` said the handle at `index` closed: its client
    /// went away (a read only says there is nothing).
    pub fn closed(&mut self, index: usize) {
        let Some(i) = (0..CLIENTS).filter(|&i| self.clients[i].is_some()).nth(index.wrapping_sub(1)) else { return };
        self.clients[i] = None;
    }

    /// Takes new clients and answers every request waiting; `node` is
    /// None between sessions.
    pub fn serve(&mut self, node: Option<&Node>) -> bool {
        let mut busy = false;
        while let Ok(c) = self.listener.accept() {
            busy = true;
            let Some(i) = self.clients.iter().position(Option::is_none) else { continue };
            if self.pool_shm.grant(&c, i * proto::SLICE, proto::SLICE).is_ok() {
                self.clients[i] = Some(Client { grants: c.peer_grants(), chan: c, resolved: Vec::new() });
            }
        }
        let mut msg = [0u8; proto::REQUEST_SIZE_V6];
        for i in 0..CLIENTS {
            loop {
                let Some(c) = self.clients[i].as_ref() else { break };
                let len = match c.chan.0.read(&mut msg) {
                    Ok(n) => n,
                    Err(t5_rt::Error::Busy) => break,
                    Err(_) => {
                        // The client is gone.
                        self.clients[i] = None;
                        break;
                    }
                };
                busy = true;
                let r = self.answer(i, &msg[..len], node);
                let _ = self.clients[i].as_ref().unwrap().chan.send(&r);
            }
        }
        busy
    }

    fn answer(&mut self, i: usize, m: &[u8], node: Option<&Node>) -> [u8; proto::REPLY_SIZE] {
        if m.len() < proto::REQUEST_SIZE || m[1] == proto::FAMILY_V6 {
            return reply(status::BAD_REQUEST, 0, 0);
        }
        let u = |o: usize| u32::from_le_bytes(m[o..o + 4].try_into().unwrap());
        let (code, sock, a, b) = (m[0], u16::from_le_bytes([m[2], m[3]]), u(4), u(8));
        if let Some(what) = self.refused(i, code) {
            let mut name = [0u8; t5_rt::t5a::MAX_NAME];
            let c = self.clients[i].as_ref().unwrap();
            let n = c.chan.peer_name(&mut name);
            event!(Warn, "ts.denied", app = core::str::from_utf8(&name[..n]).unwrap_or("?"), op = what);
            return reply(status::DENIED, 0, 0);
        }
        let mem = &mut self.pool[i * proto::SLICE..(i + 1) * proto::SLICE];
        let c = self.clients[i].as_mut().unwrap();
        match code {
            op::LOGIN_INFO => match &self.login {
                Login::Connecting => reply(status::OK, proto::login::CONNECTING, 0),
                Login::Url(url) => {
                    let n = url.len().min(proto::SLICE);
                    mem[..n].copy_from_slice(&url.as_bytes()[..n]);
                    reply(status::OK, proto::login::WAITING, n as u32)
                }
                Login::Joined => reply(status::OK, proto::login::JOINED, 0),
            },
            op::RESOLVE => {
                let Some(node) = node else { return reply(status::NOT_CONNECTED, 0, 0) };
                let n = (b as usize).min(proto::NAME_MAX);
                let ip = node.netmap.resolve(core::str::from_utf8(&mem[..n]).unwrap_or(""));
                let k = match c.resolved.iter().position(Option::is_none) {
                    Some(k) => k,
                    None => {
                        c.resolved.push(None);
                        c.resolved.len() - 1
                    }
                };
                c.resolved[k] = Some(ip);
                reply(status::OK, k as u32 + 1, 0)
            }
            op::RESOLVE_RESULT => match c.resolved.get((sock as usize).wrapping_sub(1)) {
                Some(Some(Some(ip))) => reply(status::OK, 1, u32::from_be_bytes(*ip)),
                Some(Some(None)) => reply(status::UNREACHABLE, 0, 0),
                _ => reply(status::BAD_REQUEST, 0, 0),
            },
            op::CLOSE => match c.resolved.get_mut((sock as usize).wrapping_sub(1)).and_then(Option::take) {
                Some(_) => reply(status::OK, 0, 0),
                None => reply(status::BAD_REQUEST, 0, 0),
            },
            op::PEER_INFO => match node.is_some_and(|node| peer_info(node, a as usize, &mut mem[..proto::peer::RECORD])) {
                true => reply(status::OK, 1, proto::peer::RECORD as u32),
                false => reply(status::NO_ADDRESS, 0, 0),
            },
            _ => reply(status::BAD_REQUEST, 0, 0),
        }
    }

    /// What the client's grants do not allow; None if they do.
    fn refused(&self, i: usize, code: u8) -> Option<&'static str> {
        let g = &self.clients[i].as_ref()?.grants;
        let (ok, what) = match code {
            op::RESOLVE | op::RESOLVE_RESULT => (g.has(b"resolve"), "resolve"),
            op::PEER_INFO | op::LOGIN_INFO => (g.has(b"info"), "info"),
            _ => (true, ""),
        };
        (!ok).then_some(what)
    }
}

/// PEER_INFO: the record (`net::peer`) of this node (`k` = 0) or peer
/// `k` - 1 in `r`; false past the last.
fn peer_info(node: &Node, k: usize, r: &mut [u8]) -> bool {
    use proto::peer;
    let Some(p) = (if k == 0 { node.netmap.node.as_ref() } else { node.netmap.peers.get(k - 1) }) else { return false };
    let now = crate::now_ms();
    r.fill(0);
    let name = p.name.split('.').next().unwrap_or("").as_bytes();
    let n = name.len().min(peer::NAME);
    r[..n].copy_from_slice(&name[..n]);
    r[48..52].copy_from_slice(&u32::from_be_bytes(p.ipv4().unwrap_or([0; 4])).to_le_bytes());
    let mut flags = match p.online {
        Some(true) => peer::ONLINE,
        Some(false) => peer::OFFLINE,
        None => 0,
    };
    if k == 0 {
        flags |= peer::SELF;
    }
    let key = p.node_key();
    if let Some(((ip, port), _)) = key.and_then(|key| node.last_path(&key)) {
        flags |= if key.and_then(|key| node.direct_path(&key, now)).is_some() { peer::DIRECT } else { peer::WAS_DIRECT };
        r[54..56].copy_from_slice(&port.to_le_bytes());
        match core::net::Ipv6Addr::from(ip).to_ipv4_mapped() {
            Some(v4) => r[56..60].copy_from_slice(&v4.to_bits().to_le_bytes()),
            None => r[72..88].copy_from_slice(&ip),
        }
    }
    r[52] = flags;
    let region = if k == 0 { node.home_region } else { p.home_region() };
    if let Some((reg, _)) = node.netmap.derp_region(region) {
        let code = reg.region_code.as_bytes();
        let n = code.len().min(8);
        r[60..60 + n].copy_from_slice(&code[..n]);
    }
    let idle = key.and_then(|key| node.last_active(&key)).map_or(u32::MAX, |t| (now.saturating_sub(t) / 1000) as u32);
    r[68..72].copy_from_slice(&idle.to_le_bytes());
    true
}

