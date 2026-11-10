//! A tailnet node without I/O: the netmap, WireGuard for its peers, and
//! the IP inside (`tun`, smoltcp). Packets through DERP go to WireGuard
//! and what they carry to smoltcp; `poll` runs smoltcp and gives what to
//! send to which peer (handshake responses, initiations, transport
//! messages). A packet to a peer without a session waits for the
//! handshake it starts. The caller runs the connections and the sockets.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use rand_core::CryptoRngCore;

use crate::Result;
use crate::keys::{KeyPair, LEN};
use crate::netmap::{MapResponse, NetMap, unix_ms};
use crate::tun::Tun;
use crate::wg::{Device, Received};

/// Disco messages (path discovery) start with "TS💬".
const DISCO_MAGIC: &[u8; 6] = b"TS\xF0\x9F\x92\xAC";

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub handshakes: u32,
    pub packets_in: u32,
    pub packets_out: u32,
    pub echo_replies: u32,
    pub disco: u32,
    pub dropped: u32,
    pub initiations: u32,
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
    /// To send: (peer node key, message).
    out: Vec<([u8; LEN], Vec<u8>)>,
    /// Packets for peers whose handshake is out: (destination, packet).
    waiting: VecDeque<([u8; 4], Vec<u8>)>,
    /// Sees every IP packet (true: outgoing), for a trace.
    pub tap: Option<fn(bool, &[u8])>,
}

impl Node {
    pub fn new(node_key: KeyPair, rng: &mut impl CryptoRngCore, now_ms: u64) -> Node {
        let wg = Device::new(node_key, rng);
        let tun = Tun::new(rng.next_u64(), now_ms);
        Node { netmap: NetMap::default(), wg, tun, stats: Stats::default(), last_drop: "", clock: None, out: Vec::new(), waiting: VecDeque::new(), tap: None }
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
        if data.starts_with(DISCO_MAGIC) {
            // Path discovery: not answered yet (doc/71 T5); peers keep
            // sending through DERP.
            self.stats.disco += 1;
            return;
        }
        match self.wg.receive(src, data, rng, now_ms) {
            Received::Reply(r) => {
                self.stats.handshakes += 1;
                self.out.push((*src, r));
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

    /// Runs smoltcp (the packets received, its timers, what the sockets
    /// wrote): the messages to send, by peer node key.
    pub fn poll(&mut self, rng: &mut impl CryptoRngCore, now_ms: u64) -> Vec<([u8; LEN], Vec<u8>)> {
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
        core::mem::take(&mut self.out)
    }

    /// Sends `packet` to peer `peer`, starting a handshake if one is due;
    /// without a session it waits for the handshake.
    fn send(&mut self, peer: usize, packet: &[u8], rng: &mut impl CryptoRngCore, now_ms: u64) {
        if self.wg.wants_handshake(peer, now_ms) {
            match self.tai64n(now_ms).and_then(|t| self.wg.initiate(peer, t, rng, now_ms)) {
                Some(m) => {
                    self.stats.initiations += 1;
                    self.out.push((self.wg.peers[peer].key, m));
                }
                None => self.drop("no clock to initiate"),
            }
        }
        if self.wg.can_send(peer, now_ms) {
            if let Some(m) = self.wg.encrypt(peer, packet, now_ms) {
                self.stats.packets_out += 1;
                self.out.push((self.wg.peers[peer].key, m));
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
        let mut n = Node::new(KeyPair::from_private([1; 32]), &mut Zero, 0);
        n.apply_map(br#"{"Peers":[{"ID":2,"Key":"nodekey:0202020202020202020202020202020202020202020202020202020202020202","Addresses":["100.64.0.6/32"]}]}"#, 0).unwrap();
        assert_eq!(n.wg.peers.len(), 1);
        assert_eq!(n.wg.peers[0].ip, [100, 64, 0, 6]);
        n.apply_map(br#"{"PeersRemoved":[2]}"#, 0).unwrap();
        assert_eq!(n.wg.peers.len(), 0);
        assert_eq!(n.tai64n(0), None);
        n.apply_map(br#"{"ControlTime":"2026-11-11T05:31:49.25Z"}"#, 1000).unwrap();
        // 1,794,375,109 s after 1970, and 2.25 s later.
        let t = n.tai64n(3000).unwrap();
        assert_eq!(u64::from_be_bytes(t[..8].try_into().unwrap()), (1 << 62) + 10 + 1_794_375_111);
        assert_eq!(u32::from_be_bytes(t[8..].try_into().unwrap()), 250_000_000);
    }
}

