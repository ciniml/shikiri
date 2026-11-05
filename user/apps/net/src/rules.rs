//! The rule text (doc/24 §3), one rule or policy per line:
//!
//! ```text
//! # comment
//! policy in drop
//! pass in on host
//! pass in state established,related
//! pass forward on host out test0
//! reject in on wlan0 proto tcp port 22 log
//! drop forward from 192.168.50.0/24 to any
//! ```
//!
//! `ACTION HOOK [on LINK] [out LINK] [proto tcp|udp|icmp] [from CIDR]
//! [to CIDR] [port N[-M]] [state S[,S]...] [log]`, ACTION one of pass, drop,
//! reject and HOOK one of in, forward, out. On `out`, `on` names the link
//! the packet leaves by. `policy HOOK pass|drop` sets what happens when no
//! rule matches; `policy HOOK drop log` also logs what it drops. Links:
//! host, wlan0, test0. States: new, established,
//! related, invalid. CIDR: `any`, A.B.C.D[/LEN] or an IPv6 address
//! [/LEN] (doc/26 §3); a rule with an address only matches its family.
//! Protocols: tcp, udp, icmp, icmp6. `port` is the destination port.
//!
//! Besides the filter (N5, doc/25):
//! `masquerade on LINK [from CIDR]` (at most 4) and
//! `dhcp-server on LINK range FIRST-LAST [dns ADDR]` (one), and
//! `ndp-proxy on UP to DOWN` (IPv6, doc/27; one).

use crate::filter::{self, Action, DhcpServer, Masq, OUT, Rule, Ruleset, st};

/// What one line holds.
pub enum Line {
    Empty,
    Rule(Rule),
    Policy(u8, Action, bool),
    Masq(Masq),
    Dhcp(DhcpServer),
    Ndp(u8, u8),
    /// `address on LINK A.B.C.D/N` (doc/58 R3): a fixed address in place of
    /// the link's DHCP client.
    Addr(u8, u32, u8),
}

fn hook(s: &str) -> Option<u8> {
    filter::HOOK_NAMES.iter().position(|h| *h == s).map(|h| h as u8)
}

fn action(s: &str) -> Option<Action> {
    match s {
        "pass" => Some(Action::Pass),
        "drop" => Some(Action::Drop),
        "reject" => Some(Action::Reject),
        _ => None,
    }
}

fn number<T: core::str::FromStr>(s: &str) -> Option<T> {
    s.parse().ok()
}

fn ipv4(s: &str) -> Option<u32> {
    let mut v = 0u32;
    let mut parts = 0;
    for p in s.split('.') {
        v = v << 8 | number::<u8>(p)? as u32;
        parts += 1;
    }
    (parts == 4).then_some(v)
}

/// (family (0 for any), address, prefix length).
fn cidr(s: &str) -> Option<(u8, u128, u8)> {
    if s == "any" {
        return Some((0, 0, 0));
    }
    let (a, len) = match s.split_once('/') {
        Some((a, l)) => (a, Some(number::<u8>(l)?)),
        None => (s, None),
    };
    if let Some(v4) = ipv4(a) {
        let len = len.unwrap_or(32);
        return (len <= 32).then_some((4, v4 as u128, len));
    }
    let v6: core::net::Ipv6Addr = a.parse().ok()?;
    let len = len.unwrap_or(128);
    (len <= 128).then_some((6, v6.to_bits(), len))
}

/// An IPv4 CIDR (masquerade).
fn cidr4(s: &str) -> Option<(u32, u8)> {
    match cidr(s)? {
        (6, ..) => None,
        (_, a, len) => Some((a as u32, len)),
    }
}

/// Sets the rule's family from an address; false if it disagrees.
fn family(r: &mut Rule, f: u8) -> bool {
    if f == 0 || r.family == 0 || r.family == f {
        if f != 0 {
            r.family = f;
        }
        true
    } else {
        false
    }
}

fn states(s: &str) -> Option<u8> {
    let mut m = 0;
    for p in s.split(',') {
        m |= match p {
            "new" => st::NEW,
            "established" => st::ESTABLISHED,
            "related" => st::RELATED,
            "invalid" => st::INVALID,
            _ => return None,
        };
    }
    Some(m)
}

pub fn parse_line(line: &str) -> Option<Line> {
    let line = line.split('#').next().unwrap_or("");
    let mut w = line.split_ascii_whitespace();
    let Some(first) = w.next() else { return Some(Line::Empty) };
    if first == "masquerade" {
        if w.next()? != "on" {
            return None;
        }
        let mut m = Masq { link: filter::link_named(w.next()?)?, src: 0, src_len: 0 };
        match w.next() {
            None => {}
            Some("from") => (m.src, m.src_len) = cidr4(w.next()?)?,
            Some(_) => return None,
        }
        return w.next().is_none().then_some(Line::Masq(m));
    }
    if first == "ndp-proxy" {
        if w.next()? != "on" {
            return None;
        }
        let up = filter::link_named(w.next()?)?;
        if w.next()? != "to" {
            return None;
        }
        let down = filter::link_named(w.next()?)?;
        let ok = up != down && up != 0 && down != 0 && w.next().is_none();
        return ok.then_some(Line::Ndp(up, down));
    }
    if first == "address" {
        if w.next()? != "on" {
            return None;
        }
        let link = filter::link_named(w.next()?)?;
        let (a, len) = cidr4(w.next()?)?;
        let ok = link != 0 && len > 0 && len < 32 && w.next().is_none();
        return ok.then_some(Line::Addr(link, a, len));
    }
    if first == "dhcp-server" {
        if w.next()? != "on" {
            return None;
        }
        let link = filter::link_named(w.next()?)?;
        if w.next()? != "range" {
            return None;
        }
        let (a, b) = w.next()?.split_once('-')?;
        let mut d = DhcpServer { link, first: ipv4(a)?, last: ipv4(b)?, dns: 0 };
        match w.next() {
            None => {}
            Some("dns") => d.dns = ipv4(w.next()?)?,
            Some(_) => return None,
        }
        let ok = d.first <= d.last && d.last - d.first < 256 && w.next().is_none();
        return ok.then_some(Line::Dhcp(d));
    }
    if first == "policy" {
        let h = hook(w.next()?)?;
        let a = action(w.next()?).filter(|&a| a != Action::Reject)?;
        let log = match w.next() {
            None => false,
            Some("log") => true,
            Some(_) => return None,
        };
        return w.next().is_none().then_some(Line::Policy(h, a, log));
    }
    let mut r = Rule::new(action(first)?, hook(w.next()?)?);
    while let Some(word) = w.next() {
        match word {
            "on" => {
                let l = filter::link_named(w.next()?)?;
                if r.hook == OUT { r.out_link = l } else { r.in_link = l }
            }
            "out" => r.out_link = filter::link_named(w.next()?)?,
            "proto" => {
                r.proto = match w.next()? {
                    "tcp" => filter::PROTO_TCP,
                    "udp" => filter::PROTO_UDP,
                    "icmp" => filter::PROTO_ICMP,
                    "icmp6" => filter::PROTO_ICMPV6,
                    _ => return None,
                }
            }
            "from" | "to" => {
                let (f, a, len) = cidr(w.next()?)?;
                if !family(&mut r, f) {
                    return None;
                }
                if word == "from" { (r.src, r.src_len) = (a, len) } else { (r.dst, r.dst_len) = (a, len) }
            }
            "port" => {
                let p = w.next()?;
                (r.port_lo, r.port_hi) = match p.split_once('-') {
                    Some((lo, hi)) => (number(lo)?, number(hi)?),
                    None => (number(p)?, number(p)?),
                };
                if r.port_lo > r.port_hi {
                    return None;
                }
            }
            "state" => r.states = states(w.next()?)?,
            "log" => r.log = true,
            _ => return None,
        }
    }
    Some(Line::Rule(r))
}

/// A rule set being sent line by line (`svc:netctl`).
pub struct Staging {
    pub set: Ruleset,
    pub lines: u32,
    /// The first line that could not be read, or the set is full.
    pub error: Option<u32>,
}

impl Staging {
    pub const fn new() -> Staging {
        Staging { set: Ruleset::OPEN, lines: 0, error: None }
    }

    pub fn line(&mut self, text: &str) {
        self.lines += 1;
        if self.error.is_some() {
            return;
        }
        match parse_line(text) {
            Some(Line::Empty) => {}
            Some(Line::Policy(h, a, log)) => {
                self.set.policy[h as usize] = a;
                self.set.policy_log[h as usize] = log;
            }
            Some(Line::Masq(m)) if self.set.masq.iter().any(Option::is_none) => {
                let i = self.set.masq.iter().position(Option::is_none).unwrap();
                self.set.masq[i] = Some(m);
            }
            Some(Line::Dhcp(d)) if self.set.dhcp.is_none() => self.set.dhcp = Some(d),
            Some(Line::Ndp(up, down)) if self.set.ndp.is_none() => self.set.ndp = Some((up, down)),
            Some(Line::Addr(link, a, len)) if self.set.addr.is_none() => self.set.addr = Some((link, a, len)),
            Some(Line::Rule(r)) if self.set.len < filter::MAX_RULES => {
                self.set.rules[self.set.len] = r;
                self.set.len += 1;
            }
            _ => self.error = Some(self.lines),
        }
    }
}
