//! STUN binding requests as Tailscale's DERP servers take them
//! (tailscale.com/net/stun: the SOFTWARE `tailnode` and a FINGERPRINT,
//! else the server drops the request). The answer's round trip measures a
//! region (netcheck), and its XOR-MAPPED-ADDRESS is this node's address as
//! the Internet sees it.

/// The port when a DERP node says 0.
pub const PORT: u16 = 3478;
pub const TXID_LEN: usize = 12;
pub const REQUEST_LEN: usize = 20 + 12 + 8;

const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const MAPPED_ADDRESS: u16 = 0x0001;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;
const SOFTWARE: u16 = 0x8022;
const FINGERPRINT: u16 = 0x8028;

/// A binding request with transaction id `tx`.
pub fn request(tx: &[u8; TXID_LEN]) -> [u8; REQUEST_LEN] {
    let mut m = [0u8; REQUEST_LEN];
    m[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    m[2..4].copy_from_slice(&((REQUEST_LEN - 20) as u16).to_be_bytes());
    m[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    m[8..20].copy_from_slice(tx);
    m[20..22].copy_from_slice(&SOFTWARE.to_be_bytes());
    m[22..24].copy_from_slice(&8u16.to_be_bytes());
    m[24..32].copy_from_slice(b"tailnode");
    m[32..34].copy_from_slice(&FINGERPRINT.to_be_bytes());
    m[34..36].copy_from_slice(&4u16.to_be_bytes());
    let fp = crc32(&m[..32]) ^ 0x5354_554E;
    m[36..40].copy_from_slice(&fp.to_be_bytes());
    m
}

/// A binding success: its transaction id and the address it saw
/// (IPv4, port); None for anything else.
pub fn response(m: &[u8]) -> Option<([u8; TXID_LEN], [u8; 4], u16)> {
    if m.len() < 20 || u16::from_be_bytes([m[0], m[1]]) != BINDING_SUCCESS || m[4..8] != MAGIC.to_be_bytes() {
        return None;
    }
    let len = u16::from_be_bytes([m[2], m[3]]) as usize;
    let tx: [u8; TXID_LEN] = m[8..20].try_into().unwrap();
    let mut attrs = m.get(20..20 + len)?;
    let mut found = None;
    while attrs.len() >= 4 {
        let ty = u16::from_be_bytes([attrs[0], attrs[1]]);
        let n = u16::from_be_bytes([attrs[2], attrs[3]]) as usize;
        let a = attrs.get(4..4 + n)?;
        // IPv4 only: [0][family 1][port][address].
        if n == 8 && a[1] == 1 {
            let port = u16::from_be_bytes([a[2], a[3]]);
            let ip: [u8; 4] = a[4..8].try_into().unwrap();
            match ty {
                XOR_MAPPED_ADDRESS => {
                    let x = MAGIC.to_be_bytes();
                    found = Some(([ip[0] ^ x[0], ip[1] ^ x[1], ip[2] ^ x[2], ip[3] ^ x[3]], port ^ (MAGIC >> 16) as u16));
                }
                MAPPED_ADDRESS if found.is_none() => found = Some((ip, port)),
                _ => {}
            }
        }
        attrs = &attrs[(4 + n + 3) & !3..];
    }
    found.map(|(ip, port)| (tx, ip, port))
}

/// CRC-32 (IEEE), bit by bit: a request is 32 bytes.
fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let tx = [7u8; TXID_LEN];
        let q = request(&tx);
        assert_eq!(&q[8..20], &tx);
        assert_eq!(crc32(&q[..32]) ^ 0x5354_554E, u32::from_be_bytes(q[36..40].try_into().unwrap()));
        // 192.0.2.1:41641 as XOR-MAPPED-ADDRESS, after an attribute to skip.
        let mut r = alloc::vec![0x01, 0x01, 0, 20, 0x21, 0x12, 0xA4, 0x42];
        r.extend_from_slice(&tx);
        r.extend_from_slice(&[0x80, 0x22, 0, 3, b'a', b'b', b'c', 0]);
        let port = 41641u16 ^ 0x2112;
        r.extend_from_slice(&[0, 0x20, 0, 8, 0, 1]);
        r.extend_from_slice(&port.to_be_bytes());
        r.extend_from_slice(&[192 ^ 0x21, 0 ^ 0x12, 2 ^ 0xA4, 1 ^ 0x42]);
        assert_eq!(response(&r), Some((tx, [192, 0, 2, 1], 41641)));
        assert_eq!(response(&q), None);
    }
}
