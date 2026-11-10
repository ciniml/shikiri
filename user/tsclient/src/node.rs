//! A tailnet node without I/O: the netmap, WireGuard for its peers, and
//! what to send back for a packet that came through DERP (a handshake
//! response, an echo reply). The caller runs the connections.

use alloc::vec::Vec;
use rand_core::CryptoRngCore;

use crate::keys::{KeyPair, LEN};
use crate::netmap::{MapResponse, NetMap};
use crate::wg::{Device, Received};
use crate::{Result, ip};

/// Disco messages (path discovery) start with "TS💬".
const DISCO_MAGIC: &[u8; 6] = b"TS\xF0\x9F\x92\xAC";

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub handshakes: u32,
    pub packets_in: u32,
    pub echo_replies: u32,
    pub disco: u32,
    pub dropped: u32,
}

pub struct Node {
    pub netmap: NetMap,
    pub wg: Device,
    pub stats: Stats,
    /// Why the last packet was dropped.
    pub last_drop: &'static str,
}

impl Node {
    pub fn new(node_key: KeyPair, rng: &mut impl CryptoRngCore) -> Node {
        Node { netmap: NetMap::default(), wg: Device::new(node_key, rng), stats: Stats::default(), last_drop: "" }
    }

    /// Applies a map response; the peers WireGuard knows follow it.
    pub fn apply_map(&mut self, json: &[u8]) -> Result<MapResponse> {
        let before = self.netmap.peers_version;
        let r = self.netmap.apply_json(json)?;
        if self.netmap.peers_version != before {
            let peers = self.netmap.peers.iter().filter_map(|p| Some((p.node_key()?, p.ipv4()?)));
            self.wg.set_peers(peers);
        }
        Ok(r)
    }

    /// A packet from node key `src` through DERP: what to send back to it.
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
            Received::Packet { peer, packet } => {
                self.stats.packets_in += 1;
                let me = self.netmap.self_ipv4()?;
                match ip::echo_reply(&packet, me) {
                    Some(reply) => {
                        self.stats.echo_replies += 1;
                        self.wg.encrypt(peer, &reply, now_ms)
                    }
                    None => self.drop("not an echo request"),
                }
            }
            Received::Nothing("keep-alive") => None,
            Received::Nothing(why) => self.drop(why),
        }
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
        let mut n = Node::new(KeyPair::from_private([1; 32]), &mut Zero);
        n.apply_map(br#"{"Peers":[{"ID":2,"Key":"nodekey:0202020202020202020202020202020202020202020202020202020202020202","Addresses":["100.64.0.6/32"]}]}"#).unwrap();
        assert_eq!(n.wg.peers.len(), 1);
        assert_eq!(n.wg.peers[0].ip, [100, 64, 0, 6]);
        n.apply_map(br#"{"PeersRemoved":[2]}"#).unwrap();
        assert_eq!(n.wg.peers.len(), 0);
    }
}

