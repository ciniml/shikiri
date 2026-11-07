//! A DHCP server for a downstream Ethernet link (N5, doc/25 §3), set by
//! `dhcp-server on LINK range FIRST-LAST [dns ADDR]`: it answers DISCOVER
//! with an OFFER and REQUEST with an ACK (or a NAK), from the range, giving
//! the same address to the same MAC address while its lease lives. The
//! router is the link's own address; the DNS server is ADDR, else the one
//! the Wi-Fi link's DHCP gave, else the link's address. Answers are
//! broadcast (the client has no address yet).

use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::udp;
use smoltcp::wire::{IpEndpoint, IpListenEndpoint, Ipv4Address};
use t5_rt::event;

use crate::filter::DhcpServer;
use crate::fwd::be32;

const LEASES: usize = 64;
const LEASE_S: u32 = 3600;
const MSG_MAX: usize = 576;
const PACKETS: usize = 4;
const COOKIE: [u8; 4] = [99, 130, 83, 99];
// Message types (RFC 2132 §9.6).
const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;
const RELEASE: u8 = 7;
// Options.
const OPT_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_REQUESTED: u8 = 50;
const OPT_LEASE: u8 = 51;
const OPT_TYPE: u8 = 53;
const OPT_SERVER: u8 = 54;
const OPT_END: u8 = 255;

static mut RX: [u8; 2048] = [0; 2048];
static mut TX: [u8; 2048] = [0; 2048];
static mut RX_META: [udp::PacketMetadata; PACKETS] = [udp::PacketMetadata::EMPTY; PACKETS];
static mut TX_META: [udp::PacketMetadata; PACKETS] = [udp::PacketMetadata::EMPTY; PACKETS];

#[derive(Clone, Copy)]
struct Lease {
    mac: [u8; 6],
    ip: u32,
    expires: u64,
}

pub struct Server {
    pub cfg: DhcpServer,
    sock: SocketHandle,
    leases: [Option<Lease>; LEASES],
}

impl Server {
    /// The server's UDP socket on its link's interface (one server at a
    /// time: the buffers are static).
    pub fn new(cfg: DhcpServer, sockets: &mut SocketSet<'static>) -> Server {
        let s = unsafe {
            let rx = udp::PacketBuffer::new(&mut (&mut *core::ptr::addr_of_mut!(RX_META))[..], &mut (&mut *core::ptr::addr_of_mut!(RX))[..]);
            let tx = udp::PacketBuffer::new(&mut (&mut *core::ptr::addr_of_mut!(TX_META))[..], &mut (&mut *core::ptr::addr_of_mut!(TX))[..]);
            udp::Socket::new(rx, tx)
        };
        let sock = sockets.add(s);
        let _ = sockets.get_mut::<udp::Socket>(sock).bind(IpListenEndpoint { addr: None, port: 67 });
        event!(Info, "net.dhcp_server", link = crate::fwd::LINK_NAMES[cfg.link as usize], first = cfg.first, last = cfg.last);
        Server { cfg, sock, leases: [None; LEASES] }
    }

    pub fn remove(self, sockets: &mut SocketSet<'static>) {
        sockets.remove(self.sock);
    }

    /// Answers what has come. `own`: the link's address and prefix;
    /// `dns`: the DNS server to give.
    pub fn poll(&mut self, sockets: &mut SocketSet<'static>, own: (u32, u8), dns: u32) {
        let s = sockets.get_mut::<udp::Socket>(self.sock);
        // Nothing come: no buffers to clear (this runs every pass, doc/58
        // R1b).
        if !s.can_recv() {
            return;
        }
        let mut msg = [0u8; MSG_MAX];
        let mut out = [0u8; MSG_MAX];
        while let Ok((n, _)) = s.recv_slice(&mut msg) {
            if let Some(len) = self.answer(&msg[..n], &mut out, own, dns) {
                let to = IpEndpoint::new(Ipv4Address::BROADCAST.into(), 68);
                let _ = s.send_slice(&out[..len], to);
            }
        }
    }

    fn lease_of(&self, mac: &[u8; 6], now: u64) -> Option<u32> {
        self.leases.iter().flatten().find(|l| l.mac == *mac && l.expires > now).map(|l| l.ip)
    }

    fn free(&self, ip: u32, mac: &[u8; 6], own: u32, now: u64) -> bool {
        (self.cfg.first..=self.cfg.last).contains(&ip) && ip != own && !self.leases.iter().flatten().any(|l| l.ip == ip && l.mac != *mac && l.expires > now)
    }

    /// The reply to request `m`, built in `out`; its length.
    fn answer(&mut self, m: &[u8], out: &mut [u8], own: (u32, u8), dns: u32) -> Option<usize> {
        if m.len() < 240 || m[0] != 1 || m[1] != 1 || m[2] != 6 || m[236..240] != COOKIE {
            return None;
        }
        let mac: [u8; 6] = m[28..34].try_into().unwrap();
        let (mut kind, mut requested, mut server) = (0, None, None);
        let mut o = &m[240..];
        while let [code, rest @ ..] = o {
            match *code {
                0 => o = rest,
                OPT_END => break,
                _ => {
                    let [len, rest @ ..] = rest else { break };
                    let Some(v) = rest.get(..*len as usize) else { break };
                    match (*code, v.len()) {
                        (OPT_TYPE, 1) => kind = v[0],
                        (OPT_REQUESTED, 4) => requested = Some(be32(v)),
                        (OPT_SERVER, 4) => server = Some(be32(v)),
                        _ => {}
                    }
                    o = &rest[*len as usize..];
                }
            }
        }
        let now = crate::clock_ns();
        let reply = match kind {
            DISCOVER => {
                let ip = self.lease_of(&mac, now).or_else(|| (self.cfg.first..=self.cfg.last).find(|&ip| self.free(ip, &mac, own.0, now)))?;
                (OFFER, ip)
            }
            REQUEST => {
                if server.is_some_and(|s| s != own.0) {
                    return None; // the client took another server's offer
                }
                let ip = requested.unwrap_or(be32(&m[12..16]));
                if self.free(ip, &mac, own.0, now) {
                    let lease = Lease { mac, ip, expires: now + LEASE_S as u64 * 1_000_000_000 };
                    match self.leases.iter_mut().find(|l| l.is_none_or(|l| l.mac == mac || l.expires <= now)) {
                        Some(slot) => *slot = Some(lease),
                        None => return None,
                    }
                    event!(Info, "net.dhcp_lease", mac = &mac[..], addr = ip, seconds = LEASE_S);
                    (ACK, ip)
                } else {
                    (NAK, 0)
                }
            }
            RELEASE => {
                for l in self.leases.iter_mut() {
                    if l.is_some_and(|l| l.mac == mac) {
                        *l = None;
                    }
                }
                return None;
            }
            _ => return None,
        };
        let (kind, yiaddr) = reply;
        out[..240].fill(0);
        out[0] = 2;
        out[1] = 1;
        out[2] = 6;
        out[4..8].copy_from_slice(&m[4..8]); // xid
        out[10..12].copy_from_slice(&m[10..12]); // flags
        out[16..20].copy_from_slice(&yiaddr.to_be_bytes());
        out[20..24].copy_from_slice(&own.0.to_be_bytes());
        out[28..44].copy_from_slice(&m[28..44]); // chaddr
        out[236..240].copy_from_slice(&COOKIE);
        let mask = if own.1 == 0 { 0 } else { u32::MAX << (32 - own.1 as u32) };
        let mut n = 240;
        let mut opt = |code: u8, v: &[u8]| {
            out[n] = code;
            out[n + 1] = v.len() as u8;
            out[n + 2..n + 2 + v.len()].copy_from_slice(v);
            n += 2 + v.len();
        };
        opt(OPT_TYPE, &[kind]);
        opt(OPT_SERVER, &own.0.to_be_bytes());
        if kind != NAK {
            opt(OPT_LEASE, &LEASE_S.to_be_bytes());
            opt(OPT_MASK, &mask.to_be_bytes());
            opt(OPT_ROUTER, &own.0.to_be_bytes());
            opt(OPT_DNS, &(if self.cfg.dns != 0 { self.cfg.dns } else { dns }).to_be_bytes());
        }
        out[n] = OPT_END;
        Some(n + 1)
    }
}
