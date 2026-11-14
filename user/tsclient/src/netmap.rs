// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The map responses (tailcfg.MapResponse, the fields the reference's
//! `tailscale_netmap.c` uses) and the netmap they build up: this node,
//! the peers, the DERP regions, the DNS settings (MagicDNS, doc/71
//! §14.16).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use serde::Deserialize;

use crate::keys;

#[derive(Deserialize, Default, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct MapResponse {
    pub node: Option<Node>,
    /// The whole list (the first response, or after a reset).
    pub peers: Option<Vec<Node>>,
    pub peers_changed: Option<Vec<Node>>,
    pub peers_removed: Option<Vec<i64>>,
    #[serde(rename = "DERPMap")]
    pub derp_map: Option<DerpMap>,
    pub keep_alive: bool,
    /// The control server's clock (RFC 3339), for WireGuard's timestamps.
    pub control_time: Option<String>,
    /// Sent when it changes (tailcfg.DNSConfig).
    #[serde(rename = "DNSConfig")]
    pub dns_config: Option<DnsConfig>,
    /// The ACL for packets in, whole, when it changes (doc/72 B3).
    pub packet_filter: Option<Vec<crate::filter::FilterRule>>,
    /// Newer servers: named parts (`*` set: the earlier parts go).
    pub packet_filters: Option<BTreeMap<String, Option<Vec<crate::filter::FilterRule>>>>,
}

/// The DNS settings (tailcfg.DNSConfig), the parts used here.
#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct DnsConfig {
    /// Search domains, the tailnet's own (`example.ts.net`) first.
    pub domains: Vec<String>,
    /// Names the admin added (tailcfg.DNSRecord).
    pub extra_records: Vec<DnsRecord>,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct DnsRecord {
    pub name: String,
    /// "" or "A" or "AAAA" (by the value).
    #[serde(rename = "Type")]
    pub kind: String,
    pub value: String,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct Node {
    #[serde(rename = "ID")]
    pub id: i64,
    pub name: String,
    /// `nodekey:…`
    pub key: String,
    /// `discokey:…`
    pub disco_key: String,
    /// This node's tailnet addresses (`100.x.y.z/32`, `fd7a:…/128`).
    pub addresses: Vec<String>,
    #[serde(rename = "AllowedIPs")]
    pub allowed_ips: Vec<String>,
    /// UDP endpoints to try directly (`ip:port`).
    pub endpoints: Vec<String>,
    /// The home DERP region as `127.3.3.40:N` (older servers).
    #[serde(rename = "DERP")]
    pub derp: String,
    /// The home DERP region (newer servers).
    #[serde(rename = "HomeDERP")]
    pub home_derp: i32,
    pub online: Option<bool>,
}

impl Node {
    pub fn node_key(&self) -> Option<[u8; keys::LEN]> {
        keys::from_hex(&self.key)
    }

    pub fn disco_key(&self) -> Option<[u8; keys::LEN]> {
        keys::from_hex(&self.disco_key)
    }

    /// The first IPv4 address (`a.b.c.d`, no prefix length).
    pub fn ipv4(&self) -> Option<[u8; 4]> {
        self.addresses.iter().find_map(|a| parse_v4(a.split('/').next()?))
    }

    /// The home DERP region, 0 if none.
    pub fn home_region(&self) -> i32 {
        if self.home_derp > 0 {
            return self.home_derp;
        }
        self.derp.strip_prefix("127.3.3.40:").and_then(|r| r.parse().ok()).unwrap_or(0)
    }
}

pub fn parse_v4(s: &str) -> Option<[u8; 4]> {
    let mut ip = [0u8; 4];
    let mut parts = s.split('.');
    for b in &mut ip {
        *b = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(ip)
}

/// RFC 3339 in UTC (`2026-11-11T05:31:49.123Z`, as the control server
/// writes `ControlTime`) as ms since 1970.
pub fn unix_ms(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || !s.ends_with('Z') {
        return None;
    }
    let n = |r: core::ops::Range<usize>| s.get(r)?.parse::<u64>().ok();
    let (y, mo, d, h, mi, sec) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let mut ms = 0;
    if b[19] == b'.' {
        let frac = &s[20..s.len() - 1];
        for (k, c) in frac.bytes().take(3).enumerate() {
            ms += (c.checked_sub(b'0')? as u64) * [100, 10, 1][k];
        }
    }
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let (y, mo) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * mo + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + sec * 1000 + ms)
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct DerpMap {
    /// By region id (a JSON object with numbers as keys).
    pub regions: BTreeMap<String, DerpRegion>,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct DerpRegion {
    #[serde(rename = "RegionID")]
    pub region_id: i32,
    pub region_code: String,
    pub nodes: Vec<DerpNode>,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "PascalCase", default)]
pub struct DerpNode {
    pub name: String,
    #[serde(rename = "RegionID")]
    pub region_id: i32,
    pub host_name: String,
    #[serde(rename = "IPv4")]
    pub ipv4: String,
    /// 0: 443.
    #[serde(rename = "DERPPort")]
    pub derp_port: u16,
    #[serde(rename = "STUNPort")]
    pub stun_port: i32,
    #[serde(rename = "STUNOnly")]
    pub stun_only: bool,
}

/// What the map responses said so far.
#[derive(Default, Debug)]
pub struct NetMap {
    pub node: Option<Node>,
    pub peers: Vec<Node>,
    pub derp: DerpMap,
    pub dns: DnsConfig,
    /// The ACL's rules (`PacketFilter`, or the parts of `PacketFilters`
    /// joined); None until the control server sends them.
    pub filter: Option<Vec<crate::filter::FilterRule>>,
    filter_parts: BTreeMap<String, Vec<crate::filter::FilterRule>>,
    /// Counts the responses that changed `filter`.
    pub filter_version: u32,
    /// Counts the responses that brought `DNSConfig`.
    pub dns_version: u32,
    /// Counts the responses that changed `peers` (or `node`).
    pub peers_version: u32,
}

impl NetMap {
    /// Parses one response and applies it.
    pub fn apply_json(&mut self, json: &[u8]) -> crate::Result<MapResponse> {
        let mut r: MapResponse = serde_json::from_slice(json).map_err(|_| crate::Error::Json)?;
        if r.node.is_some() || r.peers.is_some() || r.peers_changed.is_some() || r.peers_removed.is_some() {
            self.peers_version = self.peers_version.wrapping_add(1);
        }
        if let Some(n) = r.node.take() {
            self.node = Some(n);
        }
        if let Some(p) = r.peers.take() {
            self.peers = p;
        }
        if let Some(ch) = r.peers_changed.take() {
            for n in ch {
                match self.peers.iter_mut().find(|p| p.id == n.id) {
                    Some(p) => *p = n,
                    None => self.peers.push(n),
                }
            }
        }
        if let Some(rm) = &r.peers_removed {
            self.peers.retain(|p| !rm.contains(&p.id));
        }
        if let Some(d) = r.derp_map.take() {
            self.derp = d;
        }
        if let Some(d) = r.dns_config.take() {
            self.dns = d;
            self.dns_version = self.dns_version.wrapping_add(1);
        }
        if let Some(f) = r.packet_filter.take() {
            self.filter = Some(f);
            self.filter_version = self.filter_version.wrapping_add(1);
        }
        if let Some(parts) = r.packet_filters.take() {
            if parts.contains_key("*") {
                self.filter_parts.clear();
            }
            for (name, rules) in parts {
                match rules {
                    Some(rules) if name != "*" => {
                        self.filter_parts.insert(name, rules);
                    }
                    _ => {
                        self.filter_parts.remove(&name);
                    }
                }
            }
            self.filter = Some(self.filter_parts.values().flatten().cloned().collect());
            self.filter_version = self.filter_version.wrapping_add(1);
        }
        Ok(r)
    }

    /// The region to call home: the lowest id with a node that relays (the
    /// reference's choice; `netcheck` finds the nearest).
    pub fn home_derp(&self) -> Option<(&DerpRegion, &DerpNode)> {
        self.derp
            .regions
            .values()
            .filter_map(|r| Some((r, r.nodes.iter().find(|n| !n.stun_only)?)))
            .min_by_key(|(r, _)| r.region_id)
    }

    /// Region `id` and its first node that relays.
    pub fn derp_region(&self, id: i32) -> Option<(&DerpRegion, &DerpNode)> {
        let r = self.derp.regions.values().find(|r| r.region_id == id)?;
        Some((r, r.nodes.iter().find(|n| !n.stun_only)?))
    }

    pub fn self_ipv4(&self) -> Option<[u8; 4]> {
        self.node.as_ref()?.ipv4()
    }

    /// Whether IPv4 `dst` is this node's to take: its address, or in a
    /// subnet it routes that the control server approved (its own
    /// `AllowedIPs`, doc/72 B5).
    pub fn routes_to(&self, dst: [u8; 4]) -> bool {
        let d = u32::from_be_bytes(dst);
        self.node.as_ref().is_some_and(|n| {
            n.addresses.iter().chain(n.allowed_ips.iter()).filter_map(|a| crate::filter::parse_range(a)).any(|(lo, hi)| (lo..=hi).contains(&d))
        })
    }

    /// Every name `resolve` answers, with its address, in `resolve`'s
    /// order (the first of the same name wins): the admin's records and
    /// the nodes' whole names, each also without a search domain it ends
    /// in, then the nodes' short names. For `net`, which looks names up in
    /// this list (form B, doc/72 §4).
    pub fn names(&self) -> Vec<(String, [u8; 4])> {
        let mut v: Vec<(String, [u8; 4])> = Vec::new();
        let add = |n: &str, ip: [u8; 4], v: &mut Vec<(String, [u8; 4])>| {
            let n = n.trim_end_matches('.');
            if !n.is_empty() && !v.iter().any(|(m, _)| m.eq_ignore_ascii_case(n)) {
                v.push((n.to_ascii_lowercase(), ip));
            }
        };
        let nodes = || self.node.iter().chain(self.peers.iter());
        let records = self.dns.extra_records.iter().filter(|r| matches!(r.kind.as_str(), "" | "A")).filter_map(|r| Some((r.name.as_str(), parse_v4(&r.value)?)));
        let whole: Vec<(&str, [u8; 4])> = records.chain(nodes().filter_map(|p| Some((p.name.as_str(), p.ipv4()?)))).collect();
        for &(n, ip) in &whole {
            add(n, ip, &mut v);
        }
        for d in &self.dns.domains {
            let d = d.trim_end_matches('.');
            for &(n, ip) in &whole {
                let n = n.trim_end_matches('.');
                if n.len() > d.len() + 1 && n[n.len() - d.len()..].eq_ignore_ascii_case(d) && n.as_bytes()[n.len() - d.len() - 1] == b'.' {
                    add(&n[..n.len() - d.len() - 1], ip, &mut v);
                }
            }
        }
        for p in nodes() {
            if let (Some(short), Some(ip)) = (p.name.split('.').next(), p.ipv4()) {
                add(short, ip, &mut v);
            }
        }
        v
    }

    /// `name`'s IPv4 address as MagicDNS gives it (doc/71 §14.16): an
    /// admin's record (`ExtraRecords`), a node's whole name (a final dot
    /// or not, any case), the name with each search domain, or a node's
    /// short name (a name without dots: the first label).
    pub fn resolve(&self, name: &str) -> Option<[u8; 4]> {
        let name = name.trim_end_matches('.');
        if name.is_empty() {
            return None;
        }
        let same = |a: &str, b: &str| a.trim_end_matches('.').eq_ignore_ascii_case(b);
        let nodes = || self.node.iter().chain(self.peers.iter());
        let whole = |n: &str| {
            let rec = self.dns.extra_records.iter().filter(|r| matches!(r.kind.as_str(), "" | "A")).find(|r| same(&r.name, n)).and_then(|r| parse_v4(&r.value));
            rec.or_else(|| nodes().find(|p| same(&p.name, n)).and_then(|p| p.ipv4()))
        };
        if let Some(ip) = whole(name) {
            return Some(ip);
        }
        for d in &self.dns.domains {
            let d = d.trim_end_matches('.');
            let mut fq = String::with_capacity(name.len() + 1 + d.len());
            fq.push_str(name);
            fq.push('.');
            fq.push_str(d);
            if let Some(ip) = whole(&fq) {
                return Some(ip);
            }
        }
        if name.contains('.') {
            return None;
        }
        nodes().find(|p| p.name.split('.').next().is_some_and(|s| s.eq_ignore_ascii_case(name))).and_then(|p| p.ipv4())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply() {
        let mut nm = NetMap::default();
        let first = br#"{"Node":{"ID":1,"Name":"tab5.example.ts.net.","Key":"nodekey:0101010101010101010101010101010101010101010101010101010101010101","Addresses":["100.64.0.5/32","fd7a:115c:a1e0::5/128"]},
            "Peers":[{"ID":2,"Key":"nodekey:0202020202020202020202020202020202020202020202020202020202020202","DiscoKey":"discokey:0303030303030303030303030303030303030303030303030303030303030303","Addresses":["100.64.0.6/32"],"DERP":"127.3.3.40:7","Endpoints":["192.0.2.1:41641"]},
                     {"ID":3,"Addresses":["100.64.0.7/32"],"HomeDERP":2,"Unknown":{"x":[1,2]}}],
            "DERPMap":{"Regions":{"2":{"RegionID":2,"RegionCode":"sfo","Nodes":[{"Name":"2a","RegionID":2,"HostName":"derp2.example","IPv4":"192.0.2.2"}]},
                                  "1":{"RegionID":1,"RegionCode":"nyc","Nodes":[{"Name":"1s","RegionID":1,"HostName":"stun1.example","STUNOnly":true},{"Name":"1a","RegionID":1,"HostName":"derp1.example","DERPPort":8443}]}}}}"#;
        nm.apply_json(first).unwrap();
        assert_eq!(nm.self_ipv4(), Some([100, 64, 0, 5]));
        assert_eq!(nm.peers.len(), 2);
        assert_eq!(nm.peers[0].home_region(), 7);
        assert_eq!(nm.peers[1].home_region(), 2);
        assert_eq!(nm.peers[0].disco_key(), Some([3; 32]));
        let (r, n) = nm.home_derp().unwrap();
        assert_eq!((r.region_id, n.host_name.as_str(), n.derp_port), (1, "derp1.example", 8443));
        let r = nm.apply_json(br#"{"PeersChanged":[{"ID":3,"Addresses":["100.64.0.8/32"]},{"ID":4}],"PeersRemoved":[2]}"#).unwrap();
        assert!(!r.keep_alive);
        assert_eq!(nm.peers.iter().map(|p| p.id).collect::<Vec<_>>(), [3, 4]);
        assert_eq!(nm.peers[0].ipv4(), Some([100, 64, 0, 8]));
        let v = nm.peers_version;
        assert!(nm.apply_json(br#"{"KeepAlive":true}"#).unwrap().keep_alive);
        assert_eq!(nm.peers_version, v);
        assert_eq!(nm.peers.len(), 2);
    }

    /// The node's address and its approved subnet routes are its to take.
    #[test]
    fn routes_to() {
        let mut nm = NetMap::default();
        assert!(!nm.routes_to([100, 64, 0, 5]));
        nm.apply_json(br#"{"Node":{"ID":1,"Addresses":["100.64.0.5/32","fd7a:115c:a1e0::5/128"],"AllowedIPs":["100.64.0.5/32","192.168.50.0/24","fd7a:115c:a1e0::5/128"]}}"#).unwrap();
        assert!(nm.routes_to([100, 64, 0, 5]));
        assert!(nm.routes_to([192, 168, 50, 2]));
        assert!(!nm.routes_to([192, 168, 51, 2]));
        assert!(!nm.routes_to([100, 64, 0, 6]));
    }

    /// `PacketFilter` whole; `PacketFilters` by parts, `*` clearing.
    #[test]
    fn filters() {
        let mut nm = NetMap::default();
        assert!(nm.filter.is_none());
        nm.apply_json(br#"{"PacketFilter":[{"SrcIPs":["*"],"DstPorts":[{"IP":"*","Ports":{"First":0,"Last":65535}}]}]}"#).unwrap();
        assert_eq!((nm.filter.as_ref().unwrap().len(), nm.filter_version), (1, 1));
        nm.apply_json(br#"{"KeepAlive":true}"#).unwrap();
        assert_eq!(nm.filter_version, 1);
        let rule = r#"{"SrcIPs":["100.64.0.2"],"DstPorts":[{"IP":"*","Ports":{"First":22,"Last":22}}]}"#;
        nm.apply_json(alloc::format!(r#"{{"PacketFilters":{{"*":null,"base":[{rule}],"extra":[{rule},{rule}]}}}}"#).as_bytes()).unwrap();
        assert_eq!(nm.filter.as_ref().unwrap().len(), 3);
        nm.apply_json(br#"{"PacketFilters":{"extra":null}}"#).unwrap();
        assert_eq!((nm.filter.as_ref().unwrap().len(), nm.filter_version), (1, 3));
    }

    #[test]
    fn resolve() {
        let mut nm = NetMap::default();
        nm.apply_json(br#"{"Node":{"ID":1,"Name":"tab5.example.ts.net.","Addresses":["100.64.0.5/32"]},
            "Peers":[{"ID":2,"Name":"Laptop.example.ts.net.","Addresses":["fd7a:115c:a1e0::6/128","100.64.0.6/32"]},
                     {"ID":3,"Name":"nas.example.ts.net.","Addresses":["100.64.0.7/32"]}],
            "DNSConfig":{"Resolvers":[{"Addr":"192.0.2.53"}],"Domains":["example.ts.net","corp.example"],"Proxied":true,
                         "ExtraRecords":[{"Name":"printer.corp.example.","Value":"100.64.0.9"},{"Name":"v6.corp.example","Type":"AAAA","Value":"fd7a::9"},
                                         {"Name":"nas.example.ts.net","Value":"100.64.0.10"}]}}"#).unwrap();
        assert_eq!(nm.resolve("laptop"), Some([100, 64, 0, 6]));
        assert_eq!(nm.resolve("LAPTOP.example.ts.net."), Some([100, 64, 0, 6]));
        assert_eq!(nm.resolve("tab5.example.ts.net"), Some([100, 64, 0, 5]));
        // An admin's record before the node's own name.
        assert_eq!(nm.resolve("nas"), Some([100, 64, 0, 10]));
        // Through the second search domain.
        assert_eq!(nm.resolve("printer"), Some([100, 64, 0, 9]));
        assert_eq!(nm.resolve("printer.corp.example"), Some([100, 64, 0, 9]));
        assert_eq!(nm.resolve("v6.corp.example"), None);
        // Dotted names are not cut to their first label.
        assert_eq!(nm.resolve("laptop.other.example"), None);
        assert_eq!(nm.resolve("example.com"), None);
        assert_eq!(nm.resolve(""), None);
        assert_eq!(nm.resolve("."), None);
        // A later response without DNSConfig keeps it.
        nm.apply_json(br#"{"KeepAlive":true}"#).unwrap();
        assert_eq!(nm.resolve("printer"), Some([100, 64, 0, 9]));
        assert_eq!(nm.dns_version, 1);
        // The list for `net` answers as `resolve` does.
        let names = nm.names();
        for (n, ip) in &names {
            assert_eq!(nm.resolve(n), Some(*ip), "{n}");
        }
        for n in ["laptop", "nas", "printer", "tab5", "laptop.example.ts.net", "printer.corp.example"] {
            assert!(names.iter().any(|(m, _)| m == n), "{n}");
        }
    }
}
