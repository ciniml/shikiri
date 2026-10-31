//! Packet filter and connection tracking (N4, doc/24). Every IPv4 packet
//! passes one hook: `IN` (to the Tab5), `FORWARD` (through it) or `OUT`
//! (from it); 127/8 is not filtered. The active rule set is read top down
//! and the first rule that matches decides; otherwise the hook's policy.
//! A rule can ask for the packet's state, which the connection table gives:
//! a packet of a connection that has seen traffic both ways is
//! `ESTABLISHED`; an ICMP error about one is `RELATED`; a TCP packet without
//! SYN and without a connection is `INVALID`; the rest is `NEW`. Every
//! packet that passes records or refreshes its connection, so the replies
//! to what the Tab5 starts are known.
//!
//! DHCP's answers to the client (UDP 67 → 68) always pass: they come
//! before the link has an address and match no connection.

use t5_rt::{event, time_ns};

use crate::fwd::{IFACES, LINK_NAMES, be32};

pub const IN: u8 = 0;
pub const FORWARD: u8 = 1;
pub const OUT: u8 = 2;
pub const HOOK_NAMES: [&str; 3] = ["in", "forward", "out"];
/// A rule's link that matches every link.
pub const ANY_LINK: u8 = 0xFF;

/// Packet states (bits, a rule's `state` is a set).
pub mod st {
    pub const NEW: u8 = 1;
    pub const ESTABLISHED: u8 = 2;
    pub const RELATED: u8 = 4;
    pub const INVALID: u8 = 8;
}

pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

pub const MAX_RULES: usize = 64;
const CONNECTIONS: usize = 512;
/// Slots looked at from a key's hash.
const PROBE: usize = 8;
const LOG_PER_S: u32 = 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Pass,
    Drop,
    /// Drop and answer with ICMP Destination Unreachable, administratively
    /// prohibited (only on IN and FORWARD).
    Reject,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rule {
    pub action: Action,
    pub hook: u8,
    pub in_link: u8,
    pub out_link: u8,
    /// 0: any.
    pub proto: u8,
    pub src: u32,
    pub src_len: u8,
    pub dst: u32,
    pub dst_len: u8,
    /// Destination port range (TCP, UDP); 0..=65535 is any.
    pub port_lo: u16,
    pub port_hi: u16,
    /// 0: any state.
    pub states: u8,
    pub log: bool,
}

impl Rule {
    pub const fn new(action: Action, hook: u8) -> Rule {
        Rule { action, hook, in_link: ANY_LINK, out_link: ANY_LINK, proto: 0, src: 0, src_len: 0, dst: 0, dst_len: 0, port_lo: 0, port_hi: u16::MAX, states: 0, log: false }
    }

    fn matches(&self, hook: u8, in_link: u8, out_link: u8, p: &Pkt, state: u8) -> bool {
        let prefix = |addr: u32, net: u32, len: u8| len == 0 || (addr ^ net) >> (32 - len as u32) == 0;
        self.hook == hook
            && (self.in_link == ANY_LINK || self.in_link == in_link)
            && (self.out_link == ANY_LINK || self.out_link == out_link)
            && (self.proto == 0 || self.proto == p.proto)
            && prefix(p.src, self.src, self.src_len)
            && prefix(p.dst, self.dst, self.dst_len)
            && (self.port_lo == 0 && self.port_hi == u16::MAX || (p.proto == PROTO_TCP || p.proto == PROTO_UDP) && (self.port_lo..=self.port_hi).contains(&p.dport))
            && (self.states == 0 || self.states & state != 0)
    }
}

/// `masquerade on LINK [from CIDR]` (N5, doc/25): packets forwarded out
/// of `link` from `src/src_len` leave with the link's address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Masq {
    pub link: u8,
    pub src: u32,
    pub src_len: u8,
}

/// `dhcp-server on LINK range FIRST-LAST [dns ADDR]` (N5, doc/25).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DhcpServer {
    pub link: u8,
    pub first: u32,
    pub last: u32,
    /// 0: the link's own address.
    pub dns: u32,
}

pub const MAX_MASQ: usize = 4;

#[derive(Clone, Copy)]
pub struct Ruleset {
    pub rules: [Rule; MAX_RULES],
    pub len: usize,
    /// Per hook, and whether what the policy drops is logged.
    pub policy: [Action; 3],
    pub policy_log: [bool; 3],
    pub masq: [Option<Masq>; MAX_MASQ],
    pub dhcp: Option<DhcpServer>,
}

impl Ruleset {
    /// Everything passes.
    pub const OPEN: Ruleset = Ruleset { rules: [Rule::new(Action::Pass, IN); MAX_RULES], len: 0, policy: [Action::Pass; 3], policy_log: [false; 3], masq: [None; MAX_MASQ], dhcp: None };
}

struct Filter {
    rules: Ruleset,
    /// Packets each rule decided; the last three: the policies.
    hits: [u32; MAX_RULES + 3],
    table: [Option<Conn>; CONNECTIONS],
    tracked: u32,
    table_full: u32,
    log_left: u32,
    log_until: u64,
    log_suppressed: u32,
    /// Counts rule set changes (the DHCP server follows it).
    generation: u32,
}

static mut FILTER: Filter = Filter {
    rules: Ruleset::OPEN,
    hits: [0; MAX_RULES + 3],
    table: [None; CONNECTIONS],
    tracked: 0,
    table_full: 0,
    log_left: 0,
    log_until: 0,
    log_suppressed: 0,
    generation: 0,
};

fn f() -> &'static mut Filter {
    unsafe { &mut *core::ptr::addr_of_mut!(FILTER) }
}

/// Puts `r` in force at once, between two packets; connections stay.
pub fn replace(r: &Ruleset) {
    let f = f();
    f.rules = *r;
    f.hits = [0; MAX_RULES + 3];
    f.generation += 1;
    event!(Info, "net.rules", rules = r.len as u32, policy_in = r.policy[0] == Action::Pass, policy_forward = r.policy[1] == Action::Pass, policy_out = r.policy[2] == Action::Pass);
}

/// The rule set in force and its generation.
pub fn active() -> (&'static Ruleset, u32) {
    let f = f();
    (&f.rules, f.generation)
}

/// Logs each rule's count and the connection table's use.
pub fn log_stats() {
    let f = f();
    for i in 0..f.rules.len {
        event!(Info, "net.rule_hits", rule = i as u32, hits = f.hits[i]);
    }
    for h in 0..3 {
        event!(Info, "net.policy_hits", hook = HOOK_NAMES[h], hits = f.hits[MAX_RULES + h]);
    }
    let now = time_ns();
    let live = f.table.iter().flatten().filter(|c| c.expires > now).count() as u32;
    event!(Info, "net.conntrack", live = live, tracked = f.tracked, table_full = f.table_full);
    let (mapped, full) = crate::nat::stats();
    event!(Info, "net.nat", mapped = mapped, full = full);
}

// ---- packets ----

/// What the filter looks at in an IPv4 packet.
pub struct Pkt {
    pub proto: u8,
    pub src: u32,
    pub dst: u32,
    pub sport: u16,
    pub dport: u16,
    tcp_flags: u8,
    /// ICMP errors: the connection of the packet they quote.
    inner: Option<Key>,
}

const TCP_FIN: u8 = 0x01;
const TCP_SYN: u8 = 0x02;
const TCP_RST: u8 = 0x04;
const TCP_ACK: u8 = 0x10;

fn ports(proto: u8, l4: &[u8]) -> (u16, u16) {
    match proto {
        PROTO_TCP | PROTO_UDP if l4.len() >= 4 => (u16::from_be_bytes([l4[0], l4[1]]), u16::from_be_bytes([l4[2], l4[3]])),
        // Echo request and reply: both ways keyed by the identifier.
        PROTO_ICMP if l4.len() >= 6 && (l4[0] == 0 || l4[0] == 8) => {
            let id = u16::from_be_bytes([l4[4], l4[5]]);
            (id, id)
        }
        _ => (0, 0),
    }
}

pub fn parse(ip: &[u8]) -> Option<Pkt> {
    if ip.len() < 20 || ip[0] >> 4 != 4 {
        return None;
    }
    let ihl = (ip[0] & 0xF) as usize * 4;
    let l4 = ip.get(ihl..)?;
    let proto = ip[9];
    let (sport, dport) = ports(proto, l4);
    let tcp_flags = if proto == PROTO_TCP && l4.len() >= 14 { l4[13] } else { 0 };
    // Destination Unreachable, Time Exceeded, Parameter Problem: the
    // header of the packet they are about follows the first 8 bytes.
    let inner = if proto == PROTO_ICMP && l4.len() >= 8 + 20 && matches!(l4[0], 3 | 11 | 12) {
        let q = &l4[8..];
        let qihl = (q[0] & 0xF) as usize * 4;
        let (a, b) = ports(q[9], q.get(qihl..).unwrap_or(&[]));
        Some(Key::new(q[9], be32(&q[12..16]), a, be32(&q[16..20]), b))
    } else {
        None
    };
    Some(Pkt { proto, src: be32(&ip[12..16]), dst: be32(&ip[16..20]), sport, dport, tcp_flags, inner })
}

// ---- connections ----

/// A connection's addresses, the same both ways: the lower endpoint first.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    proto: u8,
    a: (u32, u16),
    b: (u32, u16),
}

impl Key {
    fn new(proto: u8, src: u32, sport: u16, dst: u32, dport: u16) -> Key {
        let (x, y) = ((src, sport), (dst, dport));
        if x <= y { Key { proto, a: x, b: y } } else { Key { proto, a: y, b: x } }
    }

    fn hash(&self) -> usize {
        let mut h = (self.proto as u32).wrapping_mul(0x9E37_79B9);
        for v in [self.a.0, self.a.1 as u32, self.b.0, self.b.1 as u32] {
            h = (h ^ v).wrapping_mul(0x85EB_CA6B).rotate_left(13);
        }
        h as usize
    }
}

#[derive(Clone, Copy)]
struct Conn {
    key: Key,
    /// The first packet's source: the other way is the reply.
    from: (u32, u16),
    replied: bool,
    expires: u64,
}

const S: u64 = 1_000_000_000;

fn lifetime(proto: u8, replied: bool, closing: bool) -> u64 {
    match proto {
        _ if closing => 10 * S,
        PROTO_TCP if replied => 600 * S,
        PROTO_TCP => 60 * S,
        PROTO_UDP if replied => 120 * S,
        _ => 30 * S,
    }
}

impl Filter {
    /// The slot of `k`'s live connection.
    fn find(&self, k: &Key, now: u64) -> Option<usize> {
        let h = k.hash();
        (0..PROBE).map(|i| (h + i) % CONNECTIONS).find(|&i| self.table[i].is_some_and(|c| c.key == *k && c.expires > now))
    }

    fn insert(&mut self, c: Conn, now: u64) {
        let h = c.key.hash();
        match (0..PROBE).map(|i| (h + i) % CONNECTIONS).find(|&i| self.table[i].is_none_or(|o| o.expires <= now)) {
            Some(i) => {
                self.table[i] = Some(c);
                self.tracked += 1;
            }
            None => self.table_full += 1,
        }
    }

    /// The packet's state, and its connection's slot.
    fn classify(&self, p: &Pkt, now: u64) -> (u8, Option<usize>) {
        let k = Key::new(p.proto, p.src, p.sport, p.dst, p.dport);
        if let Some(i) = self.find(&k, now) {
            let c = self.table[i].unwrap();
            let reply = (p.src, p.sport) != c.from;
            return (if c.replied || reply { st::ESTABLISHED } else { st::NEW }, Some(i));
        }
        if let Some(inner) = &p.inner {
            return (if self.find(inner, now).is_some() { st::RELATED } else { st::INVALID }, None);
        }
        if p.proto == PROTO_TCP && (p.tcp_flags & TCP_SYN == 0 || p.tcp_flags & TCP_ACK != 0) {
            return (st::INVALID, None);
        }
        (st::NEW, None)
    }

    /// A passed packet records or refreshes its connection.
    fn record(&mut self, p: &Pkt, state: u8, slot: Option<usize>, now: u64) {
        let closing = p.proto == PROTO_TCP && p.tcp_flags & (TCP_FIN | TCP_RST) != 0;
        match slot {
            Some(i) => {
                let c = self.table[i].as_mut().unwrap();
                if (p.src, p.sport) != c.from {
                    c.replied = true;
                }
                c.expires = now + lifetime(p.proto, c.replied, closing);
            }
            None if state == st::NEW => {
                let key = Key::new(p.proto, p.src, p.sport, p.dst, p.dport);
                self.insert(Conn { key, from: (p.src, p.sport), replied: false, expires: now + lifetime(p.proto, false, closing) }, now);
            }
            None => {}
        }
    }

    fn log_drop(&mut self, hook: u8, rule: Option<usize>, link: u8, action: Action, p: &Pkt, state: u8, now: u64) {
        if now >= self.log_until {
            if self.log_suppressed > 0 {
                event!(Info, "net.drop_suppressed", count = self.log_suppressed);
            }
            self.log_until = now + S;
            self.log_left = LOG_PER_S;
            self.log_suppressed = 0;
        }
        if self.log_left == 0 {
            self.log_suppressed += 1;
            return;
        }
        self.log_left -= 1;
        event!(
            Info,
            "net.drop",
            hook = HOOK_NAMES[hook as usize],
            rule = rule.map_or(u32::MAX, |r| r as u32),
            reject = action == Action::Reject,
            link = LINK_NAMES.get(link as usize).copied().unwrap_or("-"),
            proto = p.proto,
            saddr = p.src,
            daddr = p.dst,
            sport = p.sport,
            dport = p.dport,
            state = state
        );
    }
}

/// The filter's decision on IPv4 packet `ip` at `hook`, coming in on
/// `in_link` and leaving by `out_link` (`ANY_LINK` where there is none).
/// Packets that are not IPv4, or too short to read, pass (the stack drops
/// them).
pub fn check(hook: u8, in_link: u8, out_link: u8, ip: &[u8]) -> Action {
    let Some(p) = parse(ip) else { return Action::Pass };
    if p.dst >> 24 == 127 || p.src >> 24 == 127 {
        return Action::Pass;
    }
    // DHCP: answers to the client, and requests to the server (doc/25).
    if hook == IN && p.proto == PROTO_UDP && (p.sport == 67 && p.dport == 68 || p.sport == 68 && p.dport == 67) {
        return Action::Pass;
    }
    let f = f();
    let now = time_ns();
    let (state, slot) = f.classify(&p, now);
    let rule = (0..f.rules.len).find(|&i| f.rules.rules[i].matches(hook, in_link, out_link, &p, state));
    let (action, log) = match rule {
        Some(i) => {
            f.hits[i] += 1;
            (f.rules.rules[i].action, f.rules.rules[i].log)
        }
        None => {
            f.hits[MAX_RULES + hook as usize] += 1;
            (f.rules.policy[hook as usize], f.rules.policy_log[hook as usize])
        }
    };
    let action = if hook == OUT && action == Action::Reject { Action::Drop } else { action };
    if action == Action::Pass {
        f.record(&p, state, slot, now);
    } else if log {
        let link = if hook == OUT { out_link } else { in_link };
        f.log_drop(hook, rule, link, action, &p, state, now);
    }
    action
}

/// The link number of `name` (as in rules).
pub fn link_named(name: &str) -> Option<u8> {
    (0..IFACES).find(|&k| LINK_NAMES[k] == name).map(|k| k as u8)
}
