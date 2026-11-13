//! A tailnet node without I/O: the netmap, WireGuard for its peers, and
//! the IP inside (`tun`, smoltcp). Packets through DERP or UDP go to
//! WireGuard and what they carry to smoltcp; `poll` runs smoltcp and gives
//! what to send where (handshake responses, initiations, transport
//! messages, disco). A packet to a peer without a session waits for the
//! handshake it starts. The caller runs the connections and the sockets.
//!
//! Paths (doc/71 §14): disco pings are answered by the way they came. A
//! peer whose WireGuard messages arrive over UDP is sent to over UDP at
//! that address while they keep coming (`DIRECT_MS`); otherwise through
//! DERP. A CallMeMaybe gets a ping to each of the peer's endpoints (to
//! open the way through NATs). Sending to a peer through DERP also tries
//! for a direct path, at most every `PROBE_MS`: pings to the peer's
//! endpoints, and a CallMeMaybe through DERP with ours; a pong over UDP
//! makes its source the peer's path. While packets go to or come from a
//! peer on a direct path (`ACTIVE_MS`), a ping goes there every
//! `HEARTBEAT_MS` (Tailscale's heartbeat): its pong keeps the path, and the
//! NATs on the way keep the mapping, through gaps in the traffic.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use rand_core::CryptoRngCore;

use crate::Result;
use crate::disco::{self, Addr, Message};
use crate::keys::{KeyPair, LEN};
use crate::netmap::{MapResponse, NetMap, unix_ms};
use crate::tun::Tun;
use crate::wg::{Device, Received};

/// A peer is sent to over UDP while its messages came that way this
/// recently.
const DIRECT_MS: u64 = 15_000;
/// Trying for a direct path to a peer, at most this often.
const PROBE_MS: u64 = 5_000;
/// A peer with packets this recently is active: its direct path is kept
/// with heartbeats (Tailscale's sessionActiveTimeout).
const ACTIVE_MS: u64 = 45_000;
/// A ping on an active peer's direct path this often (Tailscale's
/// heartbeatInterval).
const HEARTBEAT_MS: u64 = 3_000;
/// Pings that may be waiting for pongs, in all.
const PINGS_MAX: usize = 16;

/// The DERP region's address in a pong (Tailscale's magic IP and the
/// region as the port).
const DERP_MAGIC_IP: [u8; 4] = [127, 3, 3, 40];
/// The start of an IPv4-mapped IPv6 address (::ffff:a.b.c.d).
const V4_MAPPED: [u8; 12] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF];

/// Where a message goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dest {
    /// Through DERP to a node key.
    Derp([u8; LEN]),
    /// Over UDP to an address (IPv4-mapped for IPv4) and port.
    Udp(Addr),
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub handshakes: u32,
    pub packets_in: u32,
    pub packets_out: u32,
    pub echo_replies: u32,
    pub disco: u32,
    pub dropped: u32,
    pub initiations: u32,
    pub udp_in: u32,
    pub udp_out: u32,
    /// Of `udp_in`, from IPv6 addresses.
    pub udp6_in: u32,
    pub pongs: u32,
    pub heartbeats: u32,
}

/// Packets that may wait for handshakes, in all.
const WAITING_MAX: usize = 16;

pub struct Node {
    pub netmap: NetMap,
    pub wg: Device,
    pub tun: Tun,
    pub stats: Stats,
    /// Why the last packet was dropped.
    pub last_drop: &'static str,
    /// The control server's clock: (Unix ms, our ms then).
    pub clock: Option<(u64, u64)>,
    /// To send: (where, message).
    out: Vec<(Dest, Vec<u8>)>,
    /// This node's disco key.
    disco: KeyPair,
    /// The home DERP region (for pongs to pings that came through it).
    pub home_region: i32,
    /// Peers heard over UDP: (node key, address, port, when).
    direct: Vec<([u8; LEN], Addr, u64)>,
    /// This node's endpoints (for CallMeMaybe), set by the caller.
    pub endpoints: Vec<Addr>,
    /// Whether to try peers' global IPv6 endpoints (the caller sets it when
    /// a router offers a way out): ULA ones are tried always, as they may
    /// be on our link.
    pub global_v6: bool,
    /// When a direct path to a peer was last tried: (node key, ms).
    probed: Vec<([u8; LEN], u64)>,
    /// Packets last went to or came from a peer: (node key, ms).
    active: Vec<([u8; LEN], u64)>,
    /// Our last ping over UDP to a peer: (node key, ms).
    pinged: Vec<([u8; LEN], u64)>,
    /// Our pings out: (transaction id, peer node key).
    pings: VecDeque<([u8; disco::TXID], [u8; LEN])>,
    /// Packets for peers whose handshake is out: (destination, packet).
    waiting: VecDeque<([u8; 4], Vec<u8>)>,
    /// Sees every IP packet (true: outgoing), for a trace.
    pub tap: Option<fn(bool, &[u8])>,
}

impl Node {
    pub fn new(node_key: KeyPair, disco: KeyPair, rng: &mut impl CryptoRngCore, now_ms: u64) -> Node {
        let wg = Device::new(node_key, rng);
        let tun = Tun::new(rng.next_u64(), now_ms);
        Node {
            netmap: NetMap::default(),
            wg,
            tun,
            stats: Stats::default(),
            last_drop: "",
            clock: None,
            out: Vec::new(),
            disco,
            home_region: 0,
            direct: Vec::new(),
            endpoints: Vec::new(),
            global_v6: false,
            probed: Vec::new(),
            active: Vec::new(),
            pinged: Vec::new(),
            pings: VecDeque::new(),
            waiting: VecDeque::new(),
            tap: None,
        }
    }

    /// Applies a map response; the peers WireGuard knows follow it.
    pub fn apply_map(&mut self, json: &[u8], now_ms: u64) -> Result<MapResponse> {
        let before = self.netmap.peers_version;
        let r = self.netmap.apply_json(json)?;
        if self.netmap.peers_version != before {
            let peers = self.netmap.peers.iter().filter_map(|p| Some((p.node_key()?, p.ipv4()?)));
            self.wg.set_peers(peers);
        }
        if let Some(ip) = self.netmap.self_ipv4() {
            self.tun.set_address(ip);
        }
        if let Some(t) = r.control_time.as_deref().and_then(unix_ms) {
            self.clock = Some((t, now_ms));
        }
        Ok(r)
    }

    /// Now as TAI64N (WireGuard's timestamp), from the control server's
    /// clock.
    pub fn tai64n(&self, now_ms: u64) -> Option<[u8; 12]> {
        let (unix, at) = self.clock?;
        let ms = unix + now_ms.saturating_sub(at);
        let mut t = [0u8; 12];
        t[..8].copy_from_slice(&((1u64 << 62) + 10 + ms / 1000).to_be_bytes());
        t[8..].copy_from_slice(&(((ms % 1000) * 1_000_000) as u32).to_be_bytes());
        Some(t)
    }

    /// A packet from node key `src` through DERP. A handshake response
    /// goes out with the next `poll`; a packet inside goes to smoltcp.
    pub fn from_derp(&mut self, src: &[u8; LEN], data: &[u8], rng: &mut impl CryptoRngCore, now_ms: u64) {
        if data.starts_with(disco::MAGIC) {
            self.disco(Dest::Derp(*src), data, rng, now_ms);
            return;
        }
        let r = self.wg.receive(Some(src), data, rng, now_ms);
        self.received(r, Dest::Derp(*src), rng, now_ms);
    }

    /// A datagram on the UDP port from `from` (IPv4-mapped for IPv4).
    pub fn from_udp(&mut self, from: Addr, data: &[u8], rng: &mut impl CryptoRngCore, now_ms: u64) {
        self.stats.udp_in += 1;
        if from.0[..12] != V4_MAPPED {
            self.stats.udp6_in += 1;
        }
        if data.starts_with(disco::MAGIC) {
            self.disco(Dest::Udp(from), data, rng, now_ms);
            return;
        }
        let r = self.wg.receive(None, data, rng, now_ms);
        // An authenticated message: the peer can be reached there.
        if let Received::Packet { peer, .. } | Received::Established { peer } = r {
            let key = self.wg.peers[peer].key;
            self.direct.retain(|d| d.0 != key);
            self.direct.push((key, from, now_ms));
        }
        self.received(r, Dest::Udp(from), rng, now_ms);
    }

    fn received(&mut self, r: Received, from: Dest, rng: &mut impl CryptoRngCore, now_ms: u64) {
        match r {
            Received::Reply(r) => {
                self.stats.handshakes += 1;
                self.out.push((from, r));
            }
            Received::Packet { packet, peer } => {
                self.stats.packets_in += 1;
                mark(&mut self.active, self.wg.peers[peer].key, now_ms);
                if let Some(t) = self.tap {
                    t(false, &packet);
                }
                self.tun.dev.rx.push_back(packet);
            }
            Received::Established { peer } => {
                self.stats.handshakes += 1;
                let ip = self.wg.peers[peer].ip;
                let (now, later): (VecDeque<_>, VecDeque<_>) = core::mem::take(&mut self.waiting).into_iter().partition(|(d, _)| *d == ip);
                self.waiting = later;
                for (_, p) in now {
                    self.send(peer, &p, rng, now_ms);
                }
            }
            Received::Nothing("keep-alive") => {}
            Received::Nothing(why) => self.drop(why),
        }
    }

    /// A disco message that came by `from`: pings are answered the same
    /// way; a CallMeMaybe gets pings to the endpoints.
    fn disco(&mut self, from: Dest, data: &[u8], rng: &mut impl CryptoRngCore, now_ms: u64) {
        self.stats.disco += 1;
        let Some(sender) = disco::sender(data) else { return self.drop("disco") };
        if !self.netmap.peers.iter().any(|p| p.disco_key() == Some(sender)) {
            return self.drop("disco from an unknown key");
        }
        let Some(m) = disco::open(&self.disco, &sender, data) else { return self.drop("disco box") };
        let mut nonce = [0u8; 24];
        match m {
            Message::Ping { tx, .. } => {
                let src = match from {
                    Dest::Udp(a) => a,
                    Dest::Derp(_) => disco::v4(DERP_MAGIC_IP, self.home_region as u16),
                };
                rng.fill_bytes(&mut nonce);
                let pong = disco::seal(&self.disco, &sender, nonce, &Message::Pong { tx, src });
                self.stats.pongs += 1;
                self.out.push((from, pong));
            }
            Message::CallMeMaybe { endpoints } => {
                for a in endpoints {
                    if !self.worth_trying(&a) {
                        continue;
                    }
                    let mut tx = [0u8; disco::TXID];
                    rng.fill_bytes(&mut tx);
                    rng.fill_bytes(&mut nonce);
                    let ping = Message::Ping { tx, node_key: Some(self.wg.public()) };
                    self.out.push((Dest::Udp(a), disco::seal(&self.disco, &sender, nonce, &ping)));
                }
            }
            Message::Pong { tx, .. } => {
                // An answer to our ping straight from the peer: its path.
                if let (Dest::Udp(a), Some(i)) = (from, self.pings.iter().position(|p| p.0 == tx)) {
                    let (_, key) = self.pings.remove(i).unwrap();
                    self.direct.retain(|d| d.0 != key);
                    self.direct.push((key, a, now_ms));
                }
            }
            Message::Other(_) => {}
        }
    }

    /// Runs smoltcp (the packets received, its timers, what the sockets
    /// wrote): the messages to send, by peer node key.
    pub fn poll(&mut self, rng: &mut impl CryptoRngCore, now_ms: u64) -> Vec<(Dest, Vec<u8>)> {
        self.tun.poll(now_ms);
        while let Some(p) = self.tun.dev.tx.pop_front() {
            let Some(dst) = p.get(16..20) else { continue };
            if let Some(t) = self.tap {
                t(true, &p);
            }
            let Some(peer) = self.wg.peers.iter().position(|q| q.ip == dst) else {
                self.drop("no peer for destination");
                continue;
            };
            // An ICMP echo reply (smoltcp answers pings itself).
            if p[9] == 1 && p.get(((p[0] & 0x0F) as usize) * 4) == Some(&0) {
                self.stats.echo_replies += 1;
            }
            self.send(peer, &p, rng, now_ms);
        }
        self.heartbeat(rng, now_ms);
        let out = core::mem::take(&mut self.out);
        self.stats.udp_out += out.iter().filter(|(d, _)| matches!(d, Dest::Udp(..))).count() as u32;
        out
    }

    /// Where peer `peer` is sent to now.
    fn dest(&self, peer: usize, now_ms: u64) -> Dest {
        let key = self.wg.peers[peer].key;
        match self.direct.iter().find(|d| d.0 == key) {
            Some(&(_, a, at)) if now_ms.saturating_sub(at) < DIRECT_MS => Dest::Udp(a),
            _ => Dest::Derp(key),
        }
    }

    /// Peers sent to over UDP now.
    pub fn direct_peers(&self, now_ms: u64) -> usize {
        self.direct.iter().filter(|d| now_ms.saturating_sub(d.2) < DIRECT_MS).count()
    }

    /// The direct path to the peer with node key `key` (address, port), if
    /// one is in use now.
    pub fn direct_path(&self, key: &[u8; LEN], now_ms: u64) -> Option<Addr> {
        self.direct.iter().find(|d| d.0 == *key && now_ms.saturating_sub(d.2) < DIRECT_MS).map(|d| d.1)
    }

    /// Whether to ping a peer's endpoint `a`: IPv4; IPv6 unicast but
    /// link-local (it needs a scope), global ones only with `global_v6`.
    fn worth_trying(&self, a: &Addr) -> bool {
        let ip = &a.0;
        if ip[..12] == V4_MAPPED {
            return true;
        }
        let ula = ip[0] & 0xFE == 0xFC;
        let global = ip[0] & 0xE0 == 0x20;
        ula || global && self.global_v6
    }

    /// The last direct path to the peer with node key `key`, in use now or
    /// lapsed, and when it was last confirmed.
    pub fn last_path(&self, key: &[u8; LEN]) -> Option<(Addr, u64)> {
        self.direct.iter().find(|d| d.0 == *key).map(|d| (d.1, d.2))
    }

    /// The home DERP region of the peer with node key `key` (0: unknown).
    pub fn peer_region(&self, key: &[u8; LEN]) -> i32 {
        self.netmap.peers.iter().find(|p| p.node_key() == Some(*key)).map_or(0, |p| p.home_region())
    }

    /// When packets last went to or came from the peer with node key `key`.
    pub fn last_active(&self, key: &[u8; LEN]) -> Option<u64> {
        self.active.iter().find(|a| a.0 == *key).map(|a| a.1)
    }

    /// Sends `packet` to peer `peer`, starting a handshake if one is due;
    /// without a session it waits for the handshake.
    fn send(&mut self, peer: usize, packet: &[u8], rng: &mut impl CryptoRngCore, now_ms: u64) {
        mark(&mut self.active, self.wg.peers[peer].key, now_ms);
        if self.wg.wants_handshake(peer, now_ms) {
            match self.tai64n(now_ms).and_then(|t| self.wg.initiate(peer, t, rng, now_ms)) {
                Some(m) => {
                    self.stats.initiations += 1;
                    self.out.push((self.dest(peer, now_ms), m));
                }
                None => self.drop("no clock to initiate"),
            }
        }
        if matches!(self.dest(peer, now_ms), Dest::Derp(_)) {
            self.probe(peer, rng, now_ms);
        }
        if self.wg.can_send(peer, now_ms) {
            if let Some(m) = self.wg.encrypt(peer, packet, now_ms) {
                self.stats.packets_out += 1;
                self.out.push((self.dest(peer, now_ms), m));
            }
            return;
        }
        if self.waiting.len() == WAITING_MAX {
            self.waiting.pop_front();
            self.drop("too many waiting for handshakes");
        }
        self.waiting.push_back((self.wg.peers[peer].ip, packet.to_vec()));
    }

    /// Tries for a direct path to peer `peer` (not more often than
    /// `PROBE_MS`): pings to its endpoints, a CallMeMaybe with ours
    /// through DERP.
    fn probe(&mut self, peer: usize, rng: &mut impl CryptoRngCore, now_ms: u64) {
        let key = self.wg.peers[peer].key;
        if self.probed.iter().any(|&(k, at)| k == key && now_ms.saturating_sub(at) < PROBE_MS) {
            return;
        }
        self.probed.retain(|p| p.0 != key);
        self.probed.push((key, now_ms));
        let Some(p) = self.netmap.peers.iter().find(|p| p.node_key() == Some(key)) else { return };
        let Some(their_disco) = p.disco_key() else { return };
        let eps: Vec<Addr> = p.endpoints.iter().filter_map(|e| parse_endpoint(e)).filter(|a| self.worth_trying(a)).collect();
        for a in eps {
            self.ping(key, &their_disco, a, rng, now_ms);
        }
        let mut nonce = [0u8; 24];
        if !self.endpoints.is_empty() {
            rng.fill_bytes(&mut nonce);
            let cmm = Message::CallMeMaybe { endpoints: self.endpoints.clone() };
            self.out.push((Dest::Derp(key), disco::seal(&self.disco, &their_disco, nonce, &cmm)));
        }
    }

    /// A disco ping over UDP to peer `key` at `to`; its pong makes (or
    /// keeps) the path.
    fn ping(&mut self, key: [u8; LEN], their_disco: &[u8; LEN], to: Addr, rng: &mut impl CryptoRngCore, now_ms: u64) {
        let (mut tx, mut nonce) = ([0u8; disco::TXID], [0u8; 24]);
        rng.fill_bytes(&mut tx);
        rng.fill_bytes(&mut nonce);
        if self.pings.len() == PINGS_MAX {
            self.pings.pop_front();
        }
        self.pings.push_back((tx, key));
        mark(&mut self.pinged, key, now_ms);
        let ping = Message::Ping { tx, node_key: Some(self.wg.public()) };
        self.out.push((Dest::Udp(to), disco::seal(&self.disco, their_disco, nonce, &ping)));
    }

    /// Pings the direct paths of the active peers every `HEARTBEAT_MS`.
    fn heartbeat(&mut self, rng: &mut impl CryptoRngCore, now_ms: u64) {
        let since = |v: &[([u8; LEN], u64)], k: [u8; LEN]| v.iter().find(|e| e.0 == k).map(|e| now_ms.saturating_sub(e.1));
        let due: Vec<([u8; LEN], Addr)> = self
            .direct
            .iter()
            .filter(|d| now_ms.saturating_sub(d.2) < DIRECT_MS)
            .filter(|d| since(&self.active, d.0).is_some_and(|t| t < ACTIVE_MS))
            .filter(|d| since(&self.pinged, d.0).is_none_or(|t| t >= HEARTBEAT_MS))
            .map(|d| (d.0, d.1))
            .collect();
        for (key, a) in due {
            let Some(their_disco) = self.netmap.peers.iter().find(|p| p.node_key() == Some(key)).and_then(|p| p.disco_key()) else { continue };
            self.stats.heartbeats += 1;
            self.ping(key, &their_disco, a, rng, now_ms);
        }
    }

    fn drop(&mut self, why: &'static str) {
        self.stats.dropped += 1;
        self.last_drop = why;
    }
}

/// An endpoint as the netmap writes it: `198.51.100.7:41641` or
/// `[2001:db8::7]:41641`.
pub fn parse_endpoint(e: &str) -> Option<Addr> {
    match e.parse::<core::net::SocketAddr>().ok()? {
        core::net::SocketAddr::V4(a) => Some(disco::v4(a.ip().octets(), a.port())),
        core::net::SocketAddr::V6(a) => Some((a.ip().octets(), a.port())),
    }
}

/// Notes `key` at `now_ms` in a list of (node key, ms).
fn mark(v: &mut Vec<([u8; LEN], u64)>, key: [u8; LEN], now_ms: u64) {
    match v.iter_mut().find(|e| e.0 == key) {
        Some(e) => e.1 = now_ms,
        None => v.push((key, now_ms)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_reach_wireguard() {
        struct Zero;
        impl rand_core::RngCore for Zero {
            fn next_u32(&mut self) -> u32 {
                0
            }
            fn next_u64(&mut self) -> u64 {
                0
            }
            fn fill_bytes(&mut self, d: &mut [u8]) {
                d.fill(0)
            }
            fn try_fill_bytes(&mut self, d: &mut [u8]) -> core::result::Result<(), rand_core::Error> {
                d.fill(0);
                Ok(())
            }
        }
        impl rand_core::CryptoRng for Zero {}
        let mut n = Node::new(KeyPair::from_private([1; 32]), KeyPair::from_private([3; 32]), &mut Zero, 0);
        n.apply_map(br#"{"Peers":[{"ID":2,"Key":"nodekey:0202020202020202020202020202020202020202020202020202020202020202","Addresses":["100.64.0.6/32"]}]}"#, 0).unwrap();
        assert_eq!(n.wg.peers.len(), 1);
        assert_eq!(n.wg.peers[0].ip, [100, 64, 0, 6]);
        n.apply_map(br#"{"PeersRemoved":[2]}"#, 0).unwrap();
        assert_eq!(n.wg.peers.len(), 0);
        assert_eq!(n.tai64n(0), None);

        // A disco ping over UDP from a peer's disco key: a pong back there.
        let peer_disco = KeyPair::from_private([4; 32]);
        let json = alloc::format!(r#"{{"Peers":[{{"ID":5,"Key":"nodekey:{}","DiscoKey":"discokey:{}","Addresses":["100.64.0.7/32"]}}]}}"#, "05".repeat(32), crate::keys::to_hex("", &peer_disco.public));
        n.apply_map(json.as_bytes(), 0).unwrap();
        let ping = disco::seal(&peer_disco, &KeyPair::from_private([3; 32]).public, [6; 24], &Message::Ping { tx: [9; 12], node_key: None });
        n.from_udp(disco::v4([192, 0, 2, 9], 41641), &ping, &mut Zero, 0);
        let out = n.poll(&mut Zero, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, Dest::Udp(disco::v4([192, 0, 2, 9], 41641)));
        let pong = disco::open(&peer_disco, &KeyPair::from_private([3; 32]).public, &out[0].1).unwrap();
        assert_eq!(pong, Message::Pong { tx: [9; 12], src: disco::v4([192, 0, 2, 9], 41641) });

        // Sending to a peer through DERP tries for a direct path: a ping to
        // its endpoint, a CallMeMaybe with ours; its pong over UDP makes
        // the path.
        let json = alloc::format!(r#"{{"PeersChanged":[{{"ID":5,"Key":"nodekey:{}","DiscoKey":"discokey:{}","Addresses":["100.64.0.7/32"],"Endpoints":["198.51.100.7:41641"]}}]}}"#, "05".repeat(32), crate::keys::to_hex("", &peer_disco.public));
        n.apply_map(json.as_bytes(), 20_000).unwrap();
        n.endpoints = alloc::vec![disco::v4([192, 0, 2, 1], 41641)];
        n.tun.dev.tx.push_back(alloc::vec![0x45, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0, 100, 64, 0, 1, 100, 64, 0, 7]);
        let out = n.poll(&mut Zero, 20_000);
        let ping = out.iter().find(|o| o.0 == Dest::Udp(disco::v4([198, 51, 100, 7], 41641))).unwrap();
        let Message::Ping { tx, .. } = disco::open(&peer_disco, &KeyPair::from_private([3; 32]).public, &ping.1).unwrap() else { panic!() };
        let cmm = out.iter().find(|o| o.0 == Dest::Derp([5; 32])).unwrap();
        assert_eq!(disco::open(&peer_disco, &KeyPair::from_private([3; 32]).public, &cmm.1), Some(Message::CallMeMaybe { endpoints: alloc::vec![disco::v4([192, 0, 2, 1], 41641)] }));
        assert_eq!(n.direct_peers(20_000), 0);
        let pong = disco::seal(&peer_disco, &KeyPair::from_private([3; 32]).public, [7; 24], &Message::Pong { tx, src: disco::v4([192, 0, 2, 1], 41641) });
        n.from_udp(disco::v4([198, 51, 100, 7], 41641), &pong, &mut Zero, 20_010);
        assert_eq!(n.direct_peers(20_010), 1);
        // Not tried again within PROBE_MS.
        n.tun.dev.tx.push_back(alloc::vec![0x45, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0, 100, 64, 0, 1, 100, 64, 0, 7]);
        assert!(n.poll(&mut Zero, 20_020).iter().all(|o| !matches!(o.0, Dest::Derp(_))));
        // While packets went to the peer, a heartbeat ping on its path
        // every HEARTBEAT_MS; its pong keeps the path past DIRECT_MS.
        assert!(n.poll(&mut Zero, 22_000).is_empty());
        let out = n.poll(&mut Zero, 23_100);
        assert_eq!((out.len(), out[0].0), (1, Dest::Udp(disco::v4([198, 51, 100, 7], 41641))));
        let Message::Ping { tx, .. } = disco::open(&peer_disco, &KeyPair::from_private([3; 32]).public, &out[0].1).unwrap() else { panic!() };
        let pong = disco::seal(&peer_disco, &KeyPair::from_private([3; 32]).public, [8; 24], &Message::Pong { tx, src: disco::v4([192, 0, 2, 1], 41641) });
        n.from_udp(disco::v4([198, 51, 100, 7], 41641), &pong, &mut Zero, 23_110);
        assert_eq!(n.direct_peers(36_000), 1);
        assert_eq!(n.direct_path(&[5; 32], 36_000), Some(disco::v4([198, 51, 100, 7], 41641)));
        assert_eq!(n.last_active(&[5; 32]), Some(20_020));
        // Idle past ACTIVE_MS: no heartbeat, and the path lapses.
        let mut t = 23_100;
        while t < 90_000 {
            t += 3_000;
            for (d, m) in n.poll(&mut Zero, t) {
                if let Some(Message::Ping { tx, .. }) = disco::open(&peer_disco, &KeyPair::from_private([3; 32]).public, &m) {
                    assert!(t < 20_020 + ACTIVE_MS);
                    let pong = disco::seal(&peer_disco, &KeyPair::from_private([3; 32]).public, [8; 24], &Message::Pong { tx, src: disco::v4([192, 0, 2, 1], 41641) });
                    let Dest::Udp(a) = d else { panic!() };
                    n.from_udp(a, &pong, &mut Zero, t);
                }
            }
        }
        assert_eq!((n.direct_peers(t), n.stats.heartbeats), (0, 14));
        assert_eq!(n.direct_path(&[5; 32], t), None);
        assert_eq!(n.last_path(&[5; 32]).map(|p| p.0), Some(disco::v4([198, 51, 100, 7], 41641)));
        // IPv6 endpoints: ULA ones get pings, global ones only with
        // global_v6, link-local ones never; a pong from the ULA one makes
        // the path, and a ping from there gets a pong there.
        assert_eq!(parse_endpoint("198.51.100.7:41641"), Some(disco::v4([198, 51, 100, 7], 41641)));
        assert_eq!(parse_endpoint("fd00::7:41641"), None);
        let ula = parse_endpoint("[fd00::7]:41641").unwrap();
        let global = parse_endpoint("[2001:db8::7]:41641").unwrap();
        let json = alloc::format!(r#"{{"PeersChanged":[{{"ID":5,"Key":"nodekey:{}","DiscoKey":"discokey:{}","Addresses":["100.64.0.7/32"],"Endpoints":["[fd00::7]:41641","[2001:db8::7]:41641","[fe80::7]:41641"]}}]}}"#, "05".repeat(32), crate::keys::to_hex("", &peer_disco.public));
        n.apply_map(json.as_bytes(), t).unwrap();
        n.tun.dev.tx.push_back(alloc::vec![0x45, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0, 100, 64, 0, 1, 100, 64, 0, 7]);
        let pings: Vec<Dest> = n.poll(&mut Zero, t).into_iter().map(|o| o.0).filter(|d| matches!(d, Dest::Udp(_))).collect();
        assert_eq!(pings, alloc::vec![Dest::Udp(ula)]);
        n.global_v6 = true;
        t += PROBE_MS;
        n.tun.dev.tx.push_back(alloc::vec![0x45, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0, 100, 64, 0, 1, 100, 64, 0, 7]);
        let out = n.poll(&mut Zero, t);
        let pings: Vec<Dest> = out.iter().map(|o| o.0).filter(|d| matches!(d, Dest::Udp(_))).collect();
        assert_eq!(pings, alloc::vec![Dest::Udp(ula), Dest::Udp(global)]);
        let ping = out.iter().find(|o| o.0 == Dest::Udp(ula)).unwrap();
        let Message::Ping { tx, .. } = disco::open(&peer_disco, &KeyPair::from_private([3; 32]).public, &ping.1).unwrap() else { panic!() };
        let pong = disco::seal(&peer_disco, &KeyPair::from_private([3; 32]).public, [8; 24], &Message::Pong { tx, src: disco::v4([192, 0, 2, 1], 41641) });
        n.from_udp(ula, &pong, &mut Zero, t);
        assert_eq!(n.direct_path(&[5; 32], t), Some(ula));
        let ping = disco::seal(&peer_disco, &KeyPair::from_private([3; 32]).public, [6; 24], &Message::Ping { tx: [9; 12], node_key: None });
        n.from_udp(ula, &ping, &mut Zero, t);
        let out = n.poll(&mut Zero, t);
        let pong = out.iter().find(|o| o.0 == Dest::Udp(ula)).unwrap();
        assert_eq!(disco::open(&peer_disco, &KeyPair::from_private([3; 32]).public, &pong.1), Some(Message::Pong { tx: [9; 12], src: ula }));
        n.apply_map(br#"{"ControlTime":"2026-11-11T05:31:49.25Z"}"#, 1000).unwrap();
        // 1,794,375,109 s after 1970, and 2.25 s later.
        let t = n.tai64n(3000).unwrap();
        assert_eq!(u64::from_be_bytes(t[..8].try_into().unwrap()), (1 << 62) + 10 + 1_794_375_111);
        assert_eq!(u32::from_be_bytes(t[8..].try_into().unwrap()), 250_000_000);
    }
}

