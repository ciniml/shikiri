//! A tailnet node without I/O: the netmap, WireGuard for its peers, and
//! the IP inside (`tun`, smoltcp). A packet through DERP may need a
//! handshake response back; the decrypted packets go to smoltcp, and
//! `poll` gives what to send to which peer. The caller runs the
//! connections and the sockets.

use alloc::vec::Vec;
use rand_core::CryptoRngCore;

use crate::Result;
use crate::keys::{KeyPair, LEN};
use crate::netmap::{MapResponse, NetMap};
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
}

pub struct Node {
    pub netmap: NetMap,
    pub wg: Device,
    pub tun: Tun,
    pub stats: Stats,
    /// Why the last packet was dropped.
    pub last_drop: &'static str,
}

impl Node {
    pub fn new(node_key: KeyPair, rng: &mut impl CryptoRngCore, now_ms: u64) -> Node {
        let wg = Device::new(node_key, rng);
        Node { netmap: NetMap::default(), wg, tun: Tun::new(rng.next_u64(), now_ms), stats: Stats::default(), last_drop: "" }
    }

    /// Applies a map response; the peers WireGuard knows follow it.
    pub fn apply_map(&mut self, json: &[u8]) -> Result<MapResponse> {
        let before = self.netmap.peers_version;
        let r = self.netmap.apply_json(json)?;
        if self.netmap.peers_version != before {
            let peers = self.netmap.peers.iter().filter_map(|p| Some((p.node_key()?, p.ipv4()?)));
            self.wg.set_peers(peers);
        }
        if let Some(ip) = self.netmap.self_ipv4() {
            self.tun.set_address(ip);
        }
        Ok(r)
    }

    /// A packet from node key `src` through DERP: what to send back to it
    /// now (a handshake response). A packet inside goes to smoltcp.
    pub fn from_derp(&mut self, src: &[u8; LEN], data: &[u8], rng: &mut impl CryptoRngCore, now_ms: u64) -> Option<Vec<u8>> {
        if data.starts_with(DISCO_MAGIC) {
            // Path discovery: not answered yet (doc/71 T5); peers keep
            // sending through DERP.
            self.stats.disco += 1;
            return None;
        }
        match self.wg.receive(src, data, rng, now_ms) {
            Received::Reply(r) => {
                self.stats.handshakes += 1;
                Some(r)
            }
            Received::Packet { packet, .. } => {
                self.stats.packets_in += 1;
                self.tun.dev.rx.push_back(packet);
                None
            }
            Received::Nothing("keep-alive") => None,
            Received::Nothing(why) => self.drop(why),
        }
    }

    /// Runs smoltcp (the packets received, its timers, what the sockets
    /// wrote): the transport messages to send, by peer node key.
    pub fn poll(&mut self, now_ms: u64) -> Vec<([u8; LEN], Vec<u8>)> {
        self.tun.poll(now_ms);
        let mut out = Vec::new();
        while let Some(p) = self.tun.dev.tx.pop_front() {
            let Some(dst) = p.get(16..20) else { continue };
            let Some(peer) = self.wg.peers.iter().position(|q| q.ip == dst) else {
                self.drop("no peer for destination");
                continue;
            };
            // An ICMP echo reply (smoltcp answers pings itself).
            if p[9] == 1 && p.get(((p[0] & 0x0F) as usize) * 4) == Some(&0) {
                self.stats.echo_replies += 1;
            }
            match self.wg.encrypt(peer, &p, now_ms) {
                Some(m) => {
                    self.stats.packets_out += 1;
                    out.push((self.wg.peers[peer].key, m));
                }
                None => {
                    // No session: WireGuard here only answers (doc/71 §10).
                    self.drop("no session to destination");
                }
            }
        }
        out
    }

    fn drop(&mut self, why: &'static str) -> Option<Vec<u8>> {
        self.stats.dropped += 1;
        self.last_drop = why;
        None
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
        n.apply_map(br#"{"Peers":[{"ID":2,"Key":"nodekey:0202020202020202020202020202020202020202020202020202020202020202","Addresses":["100.64.0.6/32"]}]}"#).unwrap();
        assert_eq!(n.wg.peers.len(), 1);
        assert_eq!(n.wg.peers[0].ip, [100, 64, 0, 6]);
        n.apply_map(br#"{"PeersRemoved":[2]}"#).unwrap();
        assert_eq!(n.wg.peers.len(), 0);
    }
}

