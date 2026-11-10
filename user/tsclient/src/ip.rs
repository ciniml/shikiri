//! The little IP this node speaks over the tailnet so far: it answers ICMP
//! echo requests (ping) to its own IPv4 address.

use alloc::vec::Vec;

/// The Internet checksum of `data`.
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    for c in data.chunks(2) {
        sum += u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32;
    }
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// The echo reply to `packet` if it is an ICMP echo request to `me`.
pub fn echo_reply(packet: &[u8], me: [u8; 4]) -> Option<Vec<u8>> {
    let ihl = (*packet.first()? & 0x0F) as usize * 4;
    if packet[0] >> 4 != 4 || ihl < 20 || packet.len() < ihl + 8 || packet[9] != 1 || packet[16..20] != me {
        return None;
    }
    // Not a fragment.
    if u16::from_be_bytes([packet[6], packet[7]]) & 0x3FFF != 0 {
        return None;
    }
    if packet[ihl] != 8 || packet[ihl + 1] != 0 {
        return None;
    }
    let mut r = packet.to_vec();
    r.copy_within(12..16, 16);
    r[12..16].copy_from_slice(&me);
    r[8] = 64;
    r[10..12].fill(0);
    let c = checksum(&r[..ihl]);
    r[10..12].copy_from_slice(&c.to_be_bytes());
    r[ihl] = 0;
    r[ihl + 2..ihl + 4].fill(0);
    let c = checksum(&r[ihl..]);
    r[ihl + 2..ihl + 4].copy_from_slice(&c.to_be_bytes());
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo() {
        let me = [100, 64, 0, 1];
        let mut p = alloc::vec![0x45, 0, 0, 32, 0x12, 0x34, 0x40, 0, 63, 1, 0, 0, 100, 64, 0, 2, 100, 64, 0, 1, 8, 0, 0, 0, 0, 9, 0, 1, b'p', b'i', b'n', b'g'];
        let c = checksum(&p[..20]);
        p[10..12].copy_from_slice(&c.to_be_bytes());
        let c = checksum(&p[20..]);
        p[22..24].copy_from_slice(&c.to_be_bytes());
        let r = echo_reply(&p, me).unwrap();
        assert_eq!(&r[12..16], &me);
        assert_eq!(&r[16..20], &[100, 64, 0, 2]);
        assert_eq!(r[20], 0);
        assert_eq!(checksum(&r[..20]), 0);
        assert_eq!(checksum(&r[20..]), 0);
        assert_eq!(&r[24..], &p[24..]);
        assert!(echo_reply(&p, [100, 64, 0, 9]).is_none());
        assert!(echo_reply(&r, [100, 64, 0, 2]).is_none());
    }
}
