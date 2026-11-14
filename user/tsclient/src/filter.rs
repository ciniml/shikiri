// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The tailnet's ACL for packets that come in from peers (the netmap's
//! `PacketFilter`, doc/72 B3), as Tailscale's own filter
//! (`wgengine/filter`) decides, IPv4 only:
//!
//! - TCP: a connection's first packet (SYN without ACK) must match a rule;
//!   the rest pass (they belong to connections either side began).
//! - UDP: replies to datagrams this node sent (the same addresses and
//!   ports, within `FLOW_MS`) pass; others must match a rule.
//! - ICMP: echo replies and errors pass; others must match a rule by
//!   address alone (no ports).
//! - Other protocols must match a rule that names them (`IPProto`).
//!
//! A rule matches when the source is in `SrcIPs` and the destination in
//! one of `DstPorts` (address and port range). Without a filter from the
//! control server nothing passes.

use alloc::vec::Vec;
use serde::Deserialize;

/// tailcfg.FilterRule, the parts used here.
#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct FilterRule {
    /// `*`, an address, `a.b.c.d/n` or `a.b.c.d-e.f.g.h` (IPv6 ones are
    /// skipped).
    #[serde(rename = "SrcIPs")]
    pub src_ips: Vec<alloc::string::String>,
    pub dst_ports: Vec<NetPortRange>,
    /// IP protocol numbers; empty: TCP, UDP, ICMP (and ICMPv6).
    #[serde(rename = "IPProto")]
    pub ip_proto: Vec<u8>,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct NetPortRange {
    #[serde(rename = "IP")]
    pub ip: alloc::string::String,
    pub ports: PortRange,
}

#[derive(Deserialize, Default, Debug, Clone, Copy)]
#[serde(rename_all = "PascalCase", default)]
pub struct PortRange {
    pub first: u16,
    pub last: u16,
}

const TCP: u8 = 6;
const UDP: u8 = 17;
const ICMP: u8 = 1;
const ICMPV6: u8 = 58;
/// A UDP flow this node began stays open to replies this long.
pub const FLOW_MS: u64 = 120_000;
/// UDP flows remembered at most (the oldest go).
const FLOWS: usize = 64;

/// An inclusive IPv4 range.
type Range = (u32, u32);

struct Rule {
    src: Vec<Range>,
    dst: Vec<(Range, u16, u16)>,
    protos: Vec<u8>,
}

/// The rules in force and the UDP flows this node began.
#[derive(Default)]
pub struct Filter {
    /// None until the control server sends one: nothing passes.
    rules: Option<Vec<Rule>>,
    /// (our port, peer address, peer port, when last sent).
    flows: Vec<(u16, u32, u16, u64)>,
}

/// `*`, an IPv4 address, CIDR or `a-b` range as an inclusive range.
pub(crate) fn parse_range(s: &str) -> Option<Range> {
    let v4 = |s: &str| s.parse::<core::net::Ipv4Addr>().ok().map(u32::from);
    if s == "*" {
        return Some((0, u32::MAX));
    }
    if let Some((a, b)) = s.split_once('-') {
        return Some((v4(a)?, v4(b)?));
    }
    if let Some((a, n)) = s.split_once('/') {
        let (a, n) = (v4(a)?, n.parse::<u32>().ok().filter(|&n| n <= 32)?);
        let mask = if n == 0 { 0 } else { u32::MAX << (32 - n) };
        return Some((a & mask, a | !mask));
    }
    v4(s).map(|a| (a, a))
}

/// What an IPv4 packet is, for the rules.
struct Packet {
    proto: u8,
    src: u32,
    dst: u32,
    src_port: u16,
    dst_port: u16,
    /// TCP SYN without ACK; ICMP echo reply or error.
    first: bool,
    reply: bool,
}

fn parse(p: &[u8]) -> Option<Packet> {
    if p.len() < 20 || p[0] >> 4 != 4 {
        return None;
    }
    let ihl = (p[0] as usize & 0xF) * 4;
    let be32 = |o: usize| u32::from_be_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]);
    let (src, dst, proto) = (be32(12), be32(16), p[9]);
    let l4 = p.get(ihl..)?;
    // Later fragments have no header: they pass with their first.
    let fragment = u16::from_be_bytes([p[6], p[7]]) & 0x1FFF != 0;
    let port = |o: usize| l4.get(o..o + 2).map_or(0, |b| u16::from_be_bytes([b[0], b[1]]));
    let mut pk = Packet { proto, src, dst, src_port: 0, dst_port: 0, first: false, reply: fragment };
    match proto {
        TCP | UDP => {
            (pk.src_port, pk.dst_port) = (port(0), port(2));
            if proto == TCP {
                let flags = *l4.get(13).unwrap_or(&0);
                pk.first = flags & 0x02 != 0 && flags & 0x10 == 0;
                pk.reply |= !pk.first;
            }
        }
        // Echo reply, destination unreachable, source quench, redirect,
        // time exceeded, parameter problem.
        ICMP => pk.reply |= matches!(l4.first(), Some(0 | 3 | 4 | 5 | 11 | 12)),
        _ => {}
    }
    Some(pk)
}

impl Filter {
    /// Takes the control server's rules (a whole `PacketFilter`).
    pub fn set(&mut self, rules: &[FilterRule]) {
        let rules = rules
            .iter()
            .filter(|r| !r.dst_ports.is_empty())
            .map(|r| Rule {
                src: r.src_ips.iter().filter_map(|s| parse_range(s)).collect(),
                dst: r.dst_ports.iter().filter_map(|d| Some((parse_range(&d.ip)?, d.ports.first, d.ports.last))).collect(),
                protos: if r.ip_proto.is_empty() { alloc::vec![TCP, UDP, ICMP, ICMPV6] } else { r.ip_proto.clone() },
            })
            .collect();
        self.rules = Some(rules);
    }

    pub fn has_rules(&self) -> bool {
        self.rules.is_some()
    }

    /// A packet this node sends to a peer: a UDP one opens its flow to
    /// replies.
    pub fn note_out(&mut self, p: &[u8], now_ms: u64) {
        let Some(pk) = parse(p).filter(|pk| pk.proto == UDP && !pk.reply) else { return };
        let key = (pk.src_port, pk.dst, pk.dst_port);
        match self.flows.iter_mut().find(|f| (f.0, f.1, f.2) == key) {
            Some(f) => f.3 = now_ms,
            None => {
                if self.flows.len() == FLOWS {
                    let oldest = (0..FLOWS).min_by_key(|&i| self.flows[i].3).unwrap();
                    self.flows.swap_remove(oldest);
                }
                self.flows.push((key.0, key.1, key.2, now_ms));
            }
        }
    }

    /// Whether packet `p` from a peer may come in.
    pub fn allows(&self, p: &[u8], now_ms: u64) -> bool {
        let Some(rules) = &self.rules else { return false };
        let Some(pk) = parse(p) else { return false };
        if pk.reply && matches!(pk.proto, TCP | ICMP) {
            return true;
        }
        if pk.proto == UDP && self.flows.iter().any(|f| f.0 == pk.dst_port && f.1 == pk.src && f.2 == pk.src_port && now_ms.saturating_sub(f.3) < FLOW_MS) {
            return true;
        }
        let ports = matches!(pk.proto, TCP | UDP);
        rules.iter().any(|r| {
            r.protos.contains(&pk.proto)
                && r.src.iter().any(|&(a, b)| (a..=b).contains(&pk.src))
                && r.dst.iter().any(|&((a, b), first, last)| (a..=b).contains(&pk.dst) && (!ports || (first..=last).contains(&pk.dst_port)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An IPv4 packet `src` → `dst` of `proto` with transport bytes `l4`.
    fn ip(proto: u8, src: [u8; 4], dst: [u8; 4], l4: &[u8]) -> Vec<u8> {
        let mut p = alloc::vec![0x45, 0, 0, 0, 0, 0, 0, 0, 64, proto, 0, 0];
        p[2..4].copy_from_slice(&((20 + l4.len()) as u16).to_be_bytes());
        p.extend_from_slice(&src);
        p.extend_from_slice(&dst);
        p.extend_from_slice(l4);
        p
    }

    fn tcp(src: [u8; 4], dst: [u8; 4], dport: u16, flags: u8) -> Vec<u8> {
        let mut t = [0u8; 20];
        t[0..2].copy_from_slice(&40000u16.to_be_bytes());
        t[2..4].copy_from_slice(&dport.to_be_bytes());
        t[12] = 0x50;
        t[13] = flags;
        ip(TCP, src, dst, &t)
    }

    fn udp(src: [u8; 4], sport: u16, dst: [u8; 4], dport: u16) -> Vec<u8> {
        let mut u = [0u8; 8];
        u[0..2].copy_from_slice(&sport.to_be_bytes());
        u[2..4].copy_from_slice(&dport.to_be_bytes());
        ip(UDP, src, dst, &u)
    }

    const ME: [u8; 4] = [100, 64, 0, 1];
    const LAPTOP: [u8; 4] = [100, 64, 0, 2];
    const OTHER: [u8; 4] = [100, 64, 0, 3];

    #[test]
    fn acl() {
        let mut f = Filter::default();
        // Nothing passes before the first filter.
        assert!(!f.allows(&tcp(LAPTOP, ME, 22, 0x10), 0));
        let rules: Vec<FilterRule> = serde_json::from_str(
            r#"[{"SrcIPs":["100.64.0.2","fd7a:115c:a1e0::2"],"DstPorts":[{"IP":"100.64.0.0/10","Ports":{"First":22,"Last":22}},{"IP":"*","Ports":{"First":8000,"Last":8099}}]},
                {"SrcIPs":["100.64.0.3-100.64.0.4"],"DstPorts":[{"IP":"100.64.0.1","Ports":{"First":0,"Last":65535}}],"IPProto":[17]},
                {"SrcIPs":["*"],"CapGrant":[{"Dsts":["*"]}]}]"#,
        )
        .unwrap();
        f.set(&rules);
        // TCP: a SYN by the rules; the rest of a connection passes.
        assert!(f.allows(&tcp(LAPTOP, ME, 22, 0x02), 0));
        assert!(f.allows(&tcp(LAPTOP, ME, 8080, 0x02), 0));
        assert!(!f.allows(&tcp(LAPTOP, ME, 23, 0x02), 0));
        assert!(!f.allows(&tcp(OTHER, ME, 22, 0x02), 0));
        assert!(f.allows(&tcp(OTHER, ME, 22, 0x12), 0));
        assert!(f.allows(&tcp(OTHER, ME, 22, 0x10), 0));
        // ICMP: a ping by the address (the first rule names TCP, UDP and
        // ICMP by default); a reply always.
        assert!(f.allows(&ip(ICMP, LAPTOP, ME, &[8, 0, 0, 0, 0, 0, 0, 0]), 0));
        assert!(!f.allows(&ip(ICMP, OTHER, ME, &[8, 0, 0, 0, 0, 0, 0, 0]), 0));
        assert!(f.allows(&ip(ICMP, OTHER, ME, &[0, 0, 0, 0, 0, 0, 0, 0]), 0));
        // UDP: the second rule (only UDP) for OTHER; replies to our flows.
        assert!(f.allows(&udp(OTHER, 5000, ME, 9), 0));
        assert!(!f.allows(&udp(LAPTOP, 53, ME, 50000), 0));
        f.note_out(&udp(ME, 50000, LAPTOP, 53), 1_000);
        assert!(f.allows(&udp(LAPTOP, 53, ME, 50000), 2_000));
        assert!(!f.allows(&udp(LAPTOP, 54, ME, 50000), 2_000));
        assert!(!f.allows(&udp(LAPTOP, 53, ME, 50000), 1_000 + FLOW_MS));
        // Other protocols need a rule naming them.
        assert!(!f.allows(&ip(47, LAPTOP, ME, &[0; 8]), 0));
        assert!(!f.allows(&[0x60; 40], 0));
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("*"), Some((0, u32::MAX)));
        assert_eq!(parse_range("100.64.0.0/10"), Some((0x6440_0000, 0x647F_FFFF)));
        assert_eq!(parse_range("10.0.0.5"), Some((0x0A00_0005, 0x0A00_0005)));
        assert_eq!(parse_range("fd7a::/48"), None);
        assert_eq!(parse_range("1.2.3.4/33"), None);
    }
}
