// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! Which DERP region is nearest (the reference took the lowest id): a STUN
//! binding request to one node of each region that relays, a few rounds,
//! the fastest answer per region. The answers also say this node's
//! address as the Internet sees it. The caller sends and receives.

use alloc::vec::Vec;
use rand_core::RngCore;

use crate::netmap::{DerpMap, parse_v4};
use crate::stun;

struct Target {
    region: i32,
    ip: [u8; 4],
    port: u16,
    tx: [u8; stun::TXID_LEN],
    sent_ms: u64,
    best_ms: Option<u32>,
}

pub struct Netcheck {
    targets: Vec<Target>,
    /// This node's address and port as a STUN server saw them.
    pub endpoint: Option<([u8; 4], u16)>,
}

impl Netcheck {
    /// One target per region that relays: its first node with an IPv4
    /// address and STUN (a port of -1 means none).
    pub fn new(map: &DerpMap) -> Netcheck {
        let targets = map
            .regions
            .values()
            .filter(|r| r.nodes.iter().any(|n| !n.stun_only))
            .filter_map(|r| {
                let n = r.nodes.iter().find(|n| n.stun_port >= 0 && parse_v4(&n.ipv4).is_some())?;
                let port = if n.stun_port == 0 { stun::PORT } else { n.stun_port as u16 };
                Some(Target { region: r.region_id, ip: parse_v4(&n.ipv4)?, port, tx: [0; stun::TXID_LEN], sent_ms: 0, best_ms: None })
            })
            .collect();
        Netcheck { targets, endpoint: None }
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    /// A round: a request (address, port, datagram) per region, each with
    /// a new transaction id.
    pub fn round(&mut self, rng: &mut impl RngCore, now_ms: u64) -> Vec<([u8; 4], u16, [u8; stun::REQUEST_LEN])> {
        self.targets
            .iter_mut()
            .map(|t| {
                rng.fill_bytes(&mut t.tx);
                t.sent_ms = now_ms;
                (t.ip, t.port, stun::request(&t.tx))
            })
            .collect()
    }

    /// A datagram that came back.
    pub fn answer(&mut self, data: &[u8], now_ms: u64) {
        let Some((tx, ip, port)) = stun::response(data) else { return };
        let Some(t) = self.targets.iter_mut().find(|t| t.tx == tx) else { return };
        let ms = now_ms.saturating_sub(t.sent_ms) as u32;
        t.best_ms = Some(t.best_ms.map_or(ms, |b| b.min(ms)));
        self.endpoint = Some((ip, port));
    }

    /// Each region that answered and its best round trip (ms).
    pub fn latencies(&self) -> impl Iterator<Item = (i32, u32)> + '_ {
        self.targets.iter().filter_map(|t| Some((t.region, t.best_ms?)))
    }

    /// The region with the shortest round trip.
    pub fn best(&self) -> Option<(i32, u32)> {
        self.latencies().min_by_key(|&(_, ms)| ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netmap::NetMap;

    #[test]
    fn nearest() {
        let mut nm = NetMap::default();
        nm.apply_json(br#"{"DERPMap":{"Regions":{
            "1":{"RegionID":1,"Nodes":[{"Name":"1a","HostName":"derp1.example","IPv4":"192.0.2.1"}]},
            "7":{"RegionID":7,"Nodes":[{"Name":"7a","HostName":"derp7.example","IPv4":"192.0.2.7","STUNPort":3479}]},
            "8":{"RegionID":8,"Nodes":[{"Name":"8a","HostName":"derp8.example","IPv4":"192.0.2.8","STUNPort":-1}]},
            "9":{"RegionID":9,"Nodes":[{"Name":"9s","HostName":"stun9.example","IPv4":"192.0.2.9","STUNOnly":true}]}}}}"#)
            .unwrap();
        let mut nc = Netcheck::new(&nm.derp);
        assert_eq!(nc.len(), 2);
        struct Count(u8);
        impl RngCore for Count {
            fn next_u32(&mut self) -> u32 {
                0
            }
            fn next_u64(&mut self) -> u64 {
                0
            }
            fn fill_bytes(&mut self, d: &mut [u8]) {
                self.0 += 1;
                d.fill(self.0);
            }
            fn try_fill_bytes(&mut self, d: &mut [u8]) -> Result<(), rand_core::Error> {
                self.fill_bytes(d);
                Ok(())
            }
        }
        let q = nc.round(&mut Count(0), 1000);
        assert_eq!((q[0].0, q[0].1, q[1].0, q[1].1), ([192, 0, 2, 1], 3478, [192, 0, 2, 7], 3479));
        // Region 7 answers after 30 ms, region 1 after 120.
        let reply = |q: &[u8; stun::REQUEST_LEN]| {
            let mut r = alloc::vec![0x01, 0x01, 0, 12];
            r.extend_from_slice(&q[4..20]);
            r.extend_from_slice(&[0, 0x01, 0, 8, 0, 1, 0xA2, 0xA9, 198, 51, 100, 9]);
            r
        };
        nc.answer(&reply(&q[1].2), 1030);
        nc.answer(&reply(&q[0].2), 1120);
        assert_eq!(nc.best(), Some((7, 30)));
        assert_eq!(nc.latencies().count(), 2);
        assert_eq!(nc.endpoint, Some(([198, 51, 100, 9], 41641)));
    }
}
