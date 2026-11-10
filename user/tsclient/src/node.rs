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
//! open the way through NATs).

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use rand_core::CryptoRngCore;

use crate::Result;
use crate::disco::{self, Message};
use crate::keys::{KeyPair, LEN};
use crate::netmap::{MapResponse, NetMap, unix_ms};
use crate::tun::Tun;
use crate::wg::{Device, Received};

/// A peer is sent to over UDP while its messages came that way this
/// recently.
const DIRECT_MS: u64 = 15_000;
/// The DERP region's address in a pong (Tailscale's magic IP and the
/// region as the port).
const DERP_MAGIC_IP: [u8; 4] = [127, 3, 3, 40];

/// Where a message goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dest {
    /// Through DERP to a node key.
    Derp([u8; LEN]),
    /// Over UDP to an IPv4 address and port.
    Udp([u8; 4], u16),
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
    pub pongs: u32,
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
    direct: Vec<([u8; LEN], [u8; 4], u16, u64)>,
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
            self.disco(Dest::Derp(*src), data, rng);
            return;
        }
        let r = self.wg.receive(Some(src), data, rng, now_ms);
        self.received(r, Dest::Derp(*src), rng, now_ms);
    }

    /// A datagram on the UDP port from `ip`:`port`.
    pub fn from_udp(&mut self, ip: [u8; 4], port: u16, data: &[u8], rng: &mut impl CryptoRngCore, now_ms: u64) {
        self.stats.udp_in += 1;
        if data.starts_with(disco::MAGIC) {
            self.disco(Dest::Udp(ip, port), data, rng);
            return;
        }
        let r = self.wg.receive(None, data, rng, now_ms);
        // An authenticated message: the peer can be reached there.
        if let Received::Packet { peer, .. } | Received::Established { peer } = r {
            let key = self.wg.peers[peer].key;
            self.direct.retain(|d| d.0 != key);
            self.direct.push((key, ip, port, now_ms));
        }
        self.received(r, Dest::Udp(ip, port), rng, now_ms);
    }

    fn received(&mut self, r: Received, from: Dest, rng: &mut impl CryptoRngCore, now_ms: u64) {
        match r {
            Received::Reply(r) => {
                self.stats.handshakes += 1;
                self.out.push((from, r));
            }
            Received::Packet { packet, .. } => {
                self.stats.packets_in += 1;
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
    fn disco(&mut self, from: Dest, data: &[u8], rng: &mut impl CryptoRngCore) {
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
                    Dest::Udp(ip, port) => disco::v4(ip, port),
                    Dest::Derp(_) => disco::v4(DERP_MAGIC_IP, self.home_region as u16),
                };
                rng.fill_bytes(&mut nonce);
                let pong = disco::seal(&self.disco, &sender, nonce, &Message::Pong { tx, src });
                self.stats.pongs += 1;
                self.out.push((from, pong));
            }
            Message::CallMeMaybe { endpoints } => {
                for (a, port) in endpoints {
                    // IPv4 only (IPv4-mapped).
                    if a[..12] != [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF] {
                        continue;
                    }
                    let mut tx = [0u8; disco::TXID];
                    rng.fill_bytes(&mut tx);
                    rng.fill_bytes(&mut nonce);
                    let ping = Message::Ping { tx, node_key: Some(self.wg.public()) };
                    self.out.push((Dest::Udp(a[12..].try_into().unwrap(), port), disco::seal(&self.disco, &sender, nonce, &ping)));
                }
            }
            Message::Pong { .. } | Message::Other(_) => {}
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
        let out = core::mem::take(&mut self.out);
        self.stats.udp_out += out.iter().filter(|(d, _)| matches!(d, Dest::Udp(..))).count() as u32;
        out
    }

    /// Where peer `peer` is sent to now.
    fn dest(&self, peer: usize, now_ms: u64) -> Dest {
        let key = self.wg.peers[peer].key;
        match self.direct.iter().find(|d| d.0 == key) {
            Some(&(_, ip, port, at)) if now_ms.saturating_sub(at) < DIRECT_MS => Dest::Udp(ip, port),
            _ => Dest::Derp(key),
        }
    }

    /// Peers sent to over UDP now.
    pub fn direct_peers(&self, now_ms: u64) -> usize {
        self.direct.iter().filter(|d| now_ms.saturating_sub(d.3) < DIRECT_MS).count()
    }

    /// Sends `packet` to peer `peer`, starting a handshake if one is due;
    /// without a session it waits for the handshake.
    fn send(&mut self, peer: usize, packet: &[u8], rng: &mut impl CryptoRngCore, now_ms: u64) {
        if self.wg.wants_handshake(peer, now_ms) {
            match self.tai64n(now_ms).and_then(|t| self.wg.initiate(peer, t, rng, now_ms)) {
                Some(m) => {
                    self.stats.initiations += 1;
                    self.out.push((self.dest(peer, now_ms), m));
                }
                None => self.drop("no clock to initiate"),
            }
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

    fn drop(&mut self, why: &'static str) {
        self.stats.dropped += 1;
        self.last_drop = why;
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
        n.from_udp([192, 0, 2, 9], 41641, &ping, &mut Zero, 0);
        let out = n.poll(&mut Zero, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, Dest::Udp([192, 0, 2, 9], 41641));
        let pong = disco::open(&peer_disco, &KeyPair::from_private([3; 32]).public, &out[0].1).unwrap();
        assert_eq!(pong, Message::Pong { tx: [9; 12], src: disco::v4([192, 0, 2, 9], 41641) });
        n.apply_map(br#"{"ControlTime":"2026-11-11T05:31:49.25Z"}"#, 1000).unwrap();
        // 1,794,375,109 s after 1970, and 2.25 s later.
        let t = n.tai64n(3000).unwrap();
        assert_eq!(u64::from_be_bytes(t[..8].try_into().unwrap()), (1 << 62) + 10 + 1_794_375_111);
        assert_eq!(u32::from_be_bytes(t[8..].try_into().unwrap()), 250_000_000);
    }
}

