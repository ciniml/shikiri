//! A DNS relay for a downstream link (doc/29 §2): it runs with the DHCP
//! server, which gives the link's own address as the DNS server. Queries
//! that come to UDP 53 there go to the DNS server Wi-Fi's DHCP gave, with
//! an identifier of the relay's own; the answer goes back to the asker with
//! its identifier. Nothing is cached. While Wi-Fi has no DNS server,
//! queries are dropped (the asker retries).

use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::udp;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint};
use t5_rt::event;

const PORT: u16 = 53;
/// The relay's own port upstream.
const UP_PORT: u16 = 53053;
const PENDING: usize = 16;
const TIMEOUT_NS: u64 = 5_000_000_000;
const MSG_MAX: usize = 512;
const PACKETS: usize = 4;
const BUF: usize = 2048;

static mut DOWN_RX: [u8; BUF] = [0; BUF];
static mut DOWN_TX: [u8; BUF] = [0; BUF];
static mut UP_RX: [u8; BUF] = [0; BUF];
static mut UP_TX: [u8; BUF] = [0; BUF];
static mut META: [[udp::PacketMetadata; PACKETS]; 4] = [[udp::PacketMetadata::EMPTY; PACKETS]; 4];

/// A UDP socket on static buffers `k` (0, 1: downstream; 2, 3: upstream).
fn socket(rx: *mut [u8; BUF], tx: *mut [u8; BUF], k: usize) -> udp::Socket<'static> {
    unsafe {
        let meta = &mut *core::ptr::addr_of_mut!(META);
        let (m_rx, m_tx) = meta.split_at_mut(k + 1);
        let rx = udp::PacketBuffer::new(&mut m_rx[k][..], &mut (&mut *rx)[..]);
        let tx = udp::PacketBuffer::new(&mut m_tx[0][..], &mut (&mut *tx)[..]);
        udp::Socket::new(rx, tx)
    }
}

#[derive(Clone, Copy)]
struct Query {
    /// The asker and its identifier; the identifier upstream.
    from: IpEndpoint,
    id: u16,
    up_id: u16,
    at: u64,
}

pub struct Relay {
    pub link: u8,
    down: SocketHandle,
    up: SocketHandle,
    pending: [Option<Query>; PENDING],
    next_id: u16,
    relayed: u32,
    dropped: u32,
}

impl Relay {
    /// Listens on `link` (its sockets in `down_set`) and asks from Wi-Fi
    /// (`up_set`). One relay at a time: the buffers are static.
    pub fn new(link: u8, down_set: &mut SocketSet<'static>, up_set: &mut SocketSet<'static>) -> Relay {
        let down = down_set.add(socket(core::ptr::addr_of_mut!(DOWN_RX), core::ptr::addr_of_mut!(DOWN_TX), 0));
        let _ = down_set.get_mut::<udp::Socket>(down).bind(IpListenEndpoint { addr: None, port: PORT });
        let up = up_set.add(socket(core::ptr::addr_of_mut!(UP_RX), core::ptr::addr_of_mut!(UP_TX), 2));
        let _ = up_set.get_mut::<udp::Socket>(up).bind(UP_PORT);
        event!(Info, "net.dns_relay", link = crate::fwd::LINK_NAMES[link as usize]);
        Relay { link, down, up, pending: [None; PENDING], next_id: (crate::clock_ns() as u16) | 1, relayed: 0, dropped: 0 }
    }

    pub fn remove(self, down_set: &mut SocketSet<'static>, up_set: &mut SocketSet<'static>) {
        down_set.remove(self.down);
        up_set.remove(self.up);
    }

    /// Moves queries up and answers down. `server`: Wi-Fi's DNS server
    /// (DHCP's, else the RA's, doc/46).
    pub fn poll(&mut self, down_set: &mut SocketSet<'static>, up_set: &mut SocketSet<'static>, server: Option<IpAddress>) {
        let now = crate::clock_ns();
        for q in self.pending.iter_mut() {
            if q.is_some_and(|q| now - q.at > TIMEOUT_NS) {
                *q = None;
                self.dropped += 1;
            }
        }
        let mut msg = [0u8; MSG_MAX];
        let down = down_set.get_mut::<udp::Socket>(self.down);
        let up = up_set.get_mut::<udp::Socket>(self.up);
        while let Ok((n, meta)) = down.recv_slice(&mut msg) {
            let (Some(server), true) = (server, n >= 12) else {
                self.dropped += 1;
                continue;
            };
            let Some(slot) = self.pending.iter_mut().find(|q| q.is_none()) else {
                self.dropped += 1;
                continue;
            };
            let id = u16::from_be_bytes([msg[0], msg[1]]);
            let up_id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            msg[..2].copy_from_slice(&up_id.to_be_bytes());
            if up.send_slice(&msg[..n], IpEndpoint::new(server, PORT)).is_ok() {
                *slot = Some(Query { from: meta.endpoint, id, up_id, at: now });
            } else {
                self.dropped += 1;
            }
        }
        while let Ok((n, _)) = up.recv_slice(&mut msg) {
            if n < 12 {
                continue;
            }
            let up_id = u16::from_be_bytes([msg[0], msg[1]]);
            let Some(q) = self.pending.iter_mut().find(|q| q.is_some_and(|q| q.up_id == up_id)) else { continue };
            let query = q.take().unwrap();
            msg[..2].copy_from_slice(&query.id.to_be_bytes());
            if down.send_slice(&msg[..n], query.from).is_ok() {
                self.relayed += 1;
                if self.relayed.is_power_of_two() {
                    event!(Info, "net.dns_relayed", count = self.relayed, dropped = self.dropped);
                }
            }
        }
    }
}
