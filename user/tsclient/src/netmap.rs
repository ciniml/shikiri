//! The map responses (tailcfg.MapResponse, the fields the reference's
//! `tailscale_netmap.c` uses) and the netmap they build up: this node,
//! the peers, the DERP regions.

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
}
