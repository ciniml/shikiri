//! Masquerade (N5, doc/25 §2): a forwarded packet that leaves by a link
//! with a `masquerade` rule for its source gets the link's address as its
//! source, and a port of its own (TCP, UDP) or echo identifier (ICMP) from
//! `PORTS`. The answer, which comes to the link's address and that port,
//! gets the original destination back and is forwarded. Checksums are
//! adjusted, not recomputed (RFC 1624).
//!
//! The table is open addressing on a keyed hash of the inside host and the
//! far end (doc/58 R5); the port given out is `PORTS.start` + the slot, so
//! the answer finds its mapping without a search, and the key (random at
//! start) keeps the ports from being guessed.
//!
//! ICMP errors about a masqueraded packet (a router's Time Exceeded) are
//! not translated yet.


use crate::filter::{self, PROTO_ICMP, PROTO_TCP, PROTO_UDP};
use crate::fwd::be32;

const MAPS: usize = 2048;
/// Slots looked at from a mapping's hash.
const PROBE: usize = 16;
/// Ports (and echo identifiers) handed out, one per slot; below the
/// stack's own ephemeral ports (49152 and up).
const PORTS: core::ops::Range<u16> = 30000..30000 + MAPS as u16;
const S: u64 = 1_000_000_000;

#[derive(Clone, Copy)]
struct Map {
    proto: u8,
    link: u8,
    /// The inside host and its port.
    inside: (u32, u16),
    /// The far end.
    remote: (u32, u16),
    /// The link's address and the port given out.
    outside: (u32, u16),
    expires: u64,
    /// An answer came back.
    replied: bool,
    /// More went out after the answer (UDP: a stream, kept longer).
    stream: bool,
}

struct Nat {
    maps: [Option<Map>; MAPS],
    key: u64,
    pub mapped: u32,
    pub full: u32,
    pub evicted: u32,
}

/// In .bss, filled by `init` (as the filter's table).
static mut NAT: core::mem::MaybeUninit<Nat> = core::mem::MaybeUninit::uninit();

/// Empties the table, with `key` for the hash; once, at start.
pub fn init(key: u64) {
    unsafe {
        let p = (*core::ptr::addr_of_mut!(NAT)).as_mut_ptr();
        let maps = core::ptr::addr_of_mut!((*p).maps).cast::<Option<Map>>();
        for i in 0..MAPS {
            maps.add(i).write(None);
        }
        core::ptr::addr_of_mut!((*p).key).write(key);
        core::ptr::addr_of_mut!((*p).mapped).write(0);
        core::ptr::addr_of_mut!((*p).full).write(0);
        core::ptr::addr_of_mut!((*p).evicted).write(0);
    }
}

fn nat() -> &'static mut Nat {
    unsafe { (*core::ptr::addr_of_mut!(NAT)).assume_init_mut() }
}

/// As the filter's: UDP that only asked and was answered (DNS) goes in 30
/// s, as in Linux; a stream stays 120 s.
fn lifetime(m: &Map) -> u64 {
    match m.proto {
        PROTO_TCP => 600 * S,
        PROTO_UDP if m.stream => 120 * S,
        _ => 30 * S,
    }
}

impl Nat {
    fn window(&self, proto: u8, link: u8, inside: (u32, u16), remote: (u32, u16)) -> impl Iterator<Item = usize> + Clone + use<> {
        let mut h = self.key ^ (proto as u64) << 8 ^ link as u64;
        for v in [inside.0 as u64, inside.1 as u64, remote.0 as u64, remote.1 as u64] {
            h = (h ^ v).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29);
        }
        let h = (h >> 32) as usize;
        (0..PROBE).map(move |i| (h + i) % MAPS)
    }
}

/// RFC 1624 eqn. 3: the checksum `hc` after the 16-bit words `old` became
/// `new`.
fn adjust(hc: u16, old: &[u8], new: &[u8]) -> u16 {
    let mut sum = !hc as u32;
    for (o, n) in old.chunks(2).zip(new.chunks(2)) {
        sum += !u16::from_be_bytes([o[0], o[1]]) as u32;
        sum += u16::from_be_bytes([n[0], n[1]]) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Where the transport's port (or echo identifier) and checksum are, for
/// the source (`src` true) or destination side: (port offset, checksum
/// offset, whether the IP addresses are in the checksum).
fn fields(proto: u8, l4: &[u8], src: bool) -> Option<(usize, usize, bool)> {
    match proto {
        PROTO_TCP if l4.len() >= 20 => Some((if src { 0 } else { 2 }, 16, true)),
        PROTO_UDP if l4.len() >= 8 => Some((if src { 0 } else { 2 }, 6, true)),
        // Echo request out, echo reply back: the identifier either way.
        PROTO_ICMP if l4.len() >= 8 && l4[0] == if src { 8 } else { 0 } => Some((4, 2, false)),
        _ => None,
    }
}

/// Rewrites the address at `ip[at..at + 4]` and the port at `l4[port]`,
/// adjusting both checksums (the IP header's is recomputed by the caller).
fn rewrite(ip: &mut [u8], ihl: usize, at: usize, addr: u32, port_at: usize, csum_at: usize, pseudo: bool, port: u16) {
    let old_addr: [u8; 4] = ip[at..at + 4].try_into().unwrap();
    let new_addr = addr.to_be_bytes();
    ip[at..at + 4].copy_from_slice(&new_addr);
    let l4 = &mut ip[ihl..];
    let old_port: [u8; 2] = l4[port_at..port_at + 2].try_into().unwrap();
    let new_port = port.to_be_bytes();
    l4[port_at..port_at + 2].copy_from_slice(&new_port);
    let hc = u16::from_be_bytes([l4[csum_at], l4[csum_at + 1]]);
    // UDP without a checksum stays without one.
    if csum_at == 6 && hc == 0 {
        return;
    }
    let mut c = adjust(hc, &old_port, &new_port);
    if pseudo {
        c = adjust(c, &old_addr, &new_addr);
    }
    if c == 0 && csum_at == 6 {
        c = 0xFFFF;
    }
    l4[csum_at..csum_at + 2].copy_from_slice(&c.to_be_bytes());
}

/// A packet about to be forwarded out of `link` (whose address is
/// `addr`): masquerades it if a rule says so.
pub fn outbound(ip: &mut [u8], link: u8, addr: u32) {
    let (rules, _) = filter::active();
    let src = be32(&ip[12..16]);
    let prefix = |net: u32, len: u8| len == 0 || (src ^ net) >> (32 - len as u32) == 0;
    if !rules.masq.iter().flatten().any(|m| m.link == link && prefix(m.src, m.src_len)) {
        return;
    }
    let ihl = (ip[0] & 0xF) as usize * 4;
    let proto = ip[9];
    let Some((port_at, csum_at, pseudo)) = ip.get(ihl..).and_then(|l4| fields(proto, l4, true)) else { return };
    let l4 = &ip[ihl..];
    let sport = u16::from_be_bytes([l4[port_at], l4[port_at + 1]]);
    let dport = if proto == PROTO_ICMP { 0 } else { u16::from_be_bytes([l4[2], l4[3]]) };
    let (inside, remote) = ((src, sport), (be32(&ip[16..20]), dport));
    let n = nat();
    let now = crate::clock_ns();
    let window = n.window(proto, link, inside, remote);
    let found = window.clone().find(|&i| n.maps[i].is_some_and(|m| m.expires > now && m.proto == proto && m.link == link && m.inside == inside && m.remote == remote));
    let slot = match found {
        Some(i) => {
            let m = n.maps[i].as_mut().unwrap();
            if m.replied {
                m.stream = true;
            }
            m.expires = now + lifetime(m);
            i
        }
        None => {
            let free = window.clone().find(|&i| n.maps[i].is_none_or(|m| m.expires <= now));
            // No room: the unanswered mapping that ends first makes room
            // (as the filter's connections, doc/58 R5).
            let Some(i) = free.or_else(|| window.filter(|&i| n.maps[i].is_some_and(|m| !m.replied)).min_by_key(|&i| n.maps[i].map_or(0, |m| m.expires))) else {
                n.full += 1;
                return;
            };
            if free.is_none() {
                n.evicted += 1;
            }
            let mut m = Map { proto, link, inside, remote, outside: (addr, PORTS.start + i as u16), expires: 0, replied: false, stream: false };
            m.expires = now + lifetime(&m);
            n.maps[i] = Some(m);
            n.mapped += 1;
            i
        }
    };
    let port = PORTS.start + slot as u16;
    rewrite(ip, ihl, 12, addr, port_at, csum_at, pseudo, port);
}

/// A packet that came in on `link` to the Tab5: if it answers a
/// masqueraded one, gives it back its inside destination and returns true
/// (the caller forwards it).
pub fn inbound(ip: &mut [u8], link: u8) -> bool {
    if ip.len() < 20 || ip[0] >> 4 != 4 {
        return false;
    }
    let ihl = (ip[0] & 0xF) as usize * 4;
    let proto = ip[9];
    let Some((port_at, csum_at, pseudo)) = ip.get(ihl..).and_then(|l4| fields(proto, l4, false)) else { return false };
    let l4 = &ip[ihl..];
    let dport = u16::from_be_bytes([l4[port_at], l4[port_at + 1]]);
    let sport = if proto == PROTO_ICMP { 0 } else { u16::from_be_bytes([l4[0], l4[1]]) };
    let (dst, remote) = (be32(&ip[16..20]), (be32(&ip[12..16]), sport));
    let now = crate::clock_ns();
    if !PORTS.contains(&dport) {
        return false;
    }
    let Some(m) = nat().maps[(dport - PORTS.start) as usize].as_mut().filter(|m| m.expires > now && m.proto == proto && m.link == link && m.outside == (dst, dport) && m.remote == remote) else {
        return false;
    };
    m.replied = true;
    m.expires = now + lifetime(m);
    let (addr, port) = m.inside;
    rewrite(ip, ihl, 16, addr, port_at, csum_at, pseudo, port);
    true
}

/// (mappings made, times the table or the ports ran out, unanswered
/// mappings dropped to make room).
pub fn stats() -> (u32, u32, u32) {
    let n = nat();
    (n.mapped, n.full, n.evicted)
}
