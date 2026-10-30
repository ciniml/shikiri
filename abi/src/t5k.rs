//! T5K: a signed kernel package (doc/16 §2). An ESP app image as espflash
//! writes it, followed by a small header and the same signature footer as
//! T5A. The whole package is written to a kernel slot as is; the
//! bootloader reads only the ESP image.
//!
//! ```text
//! +------------------------+ 0
//! | ESP app image          | L bytes (a multiple of 16)
//! +------------------------+ L
//! | header (16 bytes)      | "T5K1", version, header size, signed_size, total_size
//! +------------------------+ signed_size = L + 16   <- the signature covers [0, signed_size)
//! | footer: Signature TLV  |
//! +------------------------+ total_size
//! ```
//!
//! The version and `secure_version` come from the image's
//! `esp_app_desc_t`, so they are signed with it.

use crate::t5a::{self, FOOTER_SIZE, KEY_ID_SIZE, SIGNATURE_SIZE, Signed};

pub const MAGIC: [u8; 4] = *b"T5K1";
pub const HEADER_VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 16;

/// ESP32-P4 in `esp_image_header_t.chip_id` (esp_app_format.h:28-46).
pub const CHIP_ID_ESP32P4: u16 = 18;
const ESP_MAGIC: u8 = 0xE9;
const ESP_HEADER_SIZE: usize = 24;
const ESP_MAX_SEGMENTS: usize = 16;
const APP_DESC_MAGIC: u32 = 0xABCD_5432;
const APP_DESC_SIZE: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not an ESP app image, or one this reader cannot walk.
    BadImage(&'static str),
    BadHeader,
    BadFooter,
    /// The image does not run on this chip revision (`rev`, major * 100 +
    /// minor).
    WrongChip { chip_id: u16, min_rev: u16, max_rev: u16 },
}

/// What an ESP app image says about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EspImage<'a> {
    /// Length including the checksum padding and the appended SHA-256.
    pub len: usize,
    pub entry: u32,
    pub segments: u8,
    pub chip_id: u16,
    pub min_rev: u16,
    pub max_rev: u16,
    pub version: &'a str,
    pub project: &'a str,
    pub secure_version: u32,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn cstr(b: &[u8]) -> &str {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    core::str::from_utf8(&b[..n]).unwrap_or("?")
}

/// Walks the ESP app image at the start of `b` (esp_app_format.h:76-122;
/// the length rule of esp_image_format.c: the checksum byte ends a 16-byte
/// block after the segments, then the 32-byte SHA-256 if appended). The
/// app descriptor is at the start of the first segment.
pub fn esp_image(b: &[u8]) -> Result<EspImage<'_>, Error> {
    if b.len() < ESP_HEADER_SIZE || b[0] != ESP_MAGIC {
        return Err(Error::BadImage("magic"));
    }
    let segments = b[1];
    if segments == 0 || segments as usize > ESP_MAX_SEGMENTS {
        return Err(Error::BadImage("segment count"));
    }
    let mut at = ESP_HEADER_SIZE;
    let mut first = None;
    for _ in 0..segments {
        let len = b.get(at..at + 8).map(|h| u32_at(h, 4) as usize).ok_or(Error::BadImage("truncated"))?;
        let data = at + 8;
        let end = data.checked_add(len).filter(|&e| e <= b.len()).ok_or(Error::BadImage("segment length"))?;
        first.get_or_insert(data..end);
        at = end;
    }
    let mut len = (at + 1).next_multiple_of(16);
    if b[23] == 1 {
        len += 32;
    } else {
        return Err(Error::BadImage("no appended hash"));
    }
    if len > b.len() {
        return Err(Error::BadImage("truncated"));
    }
    let desc = first.and_then(|r| b.get(r.start..r.start + APP_DESC_SIZE)).ok_or(Error::BadImage("no app descriptor"))?;
    if u32_at(desc, 0) != APP_DESC_MAGIC {
        return Err(Error::BadImage("no app descriptor"));
    }
    Ok(EspImage {
        len,
        entry: u32_at(b, 4),
        segments,
        chip_id: u16_at(b, 12),
        min_rev: u16_at(b, 15),
        max_rev: u16_at(b, 17),
        version: cstr(&desc[16..48]),
        project: cstr(&desc[48..80]),
        secure_version: u32_at(desc, 4),
    })
}

impl EspImage<'_> {
    /// Whether the image runs on an ESP32-P4 of revision `rev`.
    pub fn check_chip(&self, rev: u16) -> Result<(), Error> {
        // max_rev 0xFFFF means "no upper limit" (esp_image_format.c).
        let ok = self.chip_id == CHIP_ID_ESP32P4 && self.min_rev <= rev && (self.max_rev == 0xFFFF || rev <= self.max_rev);
        if ok { Ok(()) } else { Err(Error::WrongChip { chip_id: self.chip_id, min_rev: self.min_rev, max_rev: self.max_rev }) }
    }
}

/// The T5K header for an image of `image_len` bytes.
pub fn header(image_len: usize) -> [u8; HEADER_SIZE] {
    let signed = (image_len + HEADER_SIZE) as u32;
    let mut h = [0u8; HEADER_SIZE];
    h[0..4].copy_from_slice(&MAGIC);
    h[4..6].copy_from_slice(&HEADER_VERSION.to_le_bytes());
    h[6..8].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
    h[8..12].copy_from_slice(&signed.to_le_bytes());
    h[12..16].copy_from_slice(&(signed + FOOTER_SIZE as u32).to_le_bytes());
    h
}

/// A package before its signature is checked.
pub struct Package<'a> {
    pub image: EspImage<'a>,
    pub signed: Signed<'a>,
    pub total_size: usize,
}

/// Splits a package (`b` may extend past it, as a flash slot does). Only
/// the structure is read here; trust nothing before the signature checks
/// out.
pub fn split(b: &[u8]) -> Result<Package<'_>, Error> {
    let image = esp_image(b)?;
    let h = b.get(image.len..image.len + HEADER_SIZE).ok_or(Error::BadHeader)?;
    let signed = u32_at(h, 8) as usize;
    let total = u32_at(h, 12) as usize;
    if h[0..4] != MAGIC
        || u16_at(h, 4) != HEADER_VERSION
        || u16_at(h, 6) as usize != HEADER_SIZE
        || signed != image.len + HEADER_SIZE
        || total != signed + FOOTER_SIZE
        || total > b.len()
    {
        return Err(Error::BadHeader);
    }
    let f = &b[signed..total];
    if u16_at(f, 0) != t5a::tlv::SIGNATURE || u16_at(f, 2) as usize != KEY_ID_SIZE + SIGNATURE_SIZE {
        return Err(Error::BadFooter);
    }
    let mut key_id = [0u8; KEY_ID_SIZE];
    key_id.copy_from_slice(&f[4..4 + KEY_ID_SIZE]);
    let mut signature = [0u8; SIGNATURE_SIZE];
    signature.copy_from_slice(&f[4 + KEY_ID_SIZE..]);
    Ok(Package { image, signed: Signed { message: &b[..signed], key_id, signature }, total_size: total })
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    /// A minimal ESP image: two segments, the first holding an app
    /// descriptor.
    fn esp(version: &str, secure_version: u32) -> std::vec::Vec<u8> {
        let mut desc = [0u8; APP_DESC_SIZE];
        desc[0..4].copy_from_slice(&APP_DESC_MAGIC.to_le_bytes());
        desc[4..8].copy_from_slice(&secure_version.to_le_bytes());
        desc[16..16 + version.len()].copy_from_slice(version.as_bytes());
        desc[48..55].copy_from_slice(b"tab5-os");
        let mut b = std::vec![0u8; ESP_HEADER_SIZE];
        b[0] = ESP_MAGIC;
        b[1] = 2;
        b[4..8].copy_from_slice(&0x4000_2380u32.to_le_bytes());
        b[12..14].copy_from_slice(&CHIP_ID_ESP32P4.to_le_bytes());
        b[15..17].copy_from_slice(&100u16.to_le_bytes());
        b[17..19].copy_from_slice(&199u16.to_le_bytes());
        b[23] = 1;
        for data in [&desc[..], &[1, 2, 3, 4][..]] {
            b.extend_from_slice(&0x4000_0020u32.to_le_bytes());
            b.extend_from_slice(&(data.len() as u32).to_le_bytes());
            b.extend_from_slice(data);
        }
        b.resize((b.len() + 1).next_multiple_of(16) + 32, 0xAA);
        b
    }

    #[test]
    fn walks_an_image() {
        let b = esp("0.2.0", 3);
        let i = esp_image(&b).unwrap();
        assert_eq!(i.len, b.len());
        assert_eq!((i.version, i.project, i.secure_version), ("0.2.0", "tab5-os", 3));
        assert!(i.check_chip(100).is_ok());
        assert!(i.check_chip(300).is_err());
        assert_eq!(esp_image(&b[..b.len() - 1]), Err(Error::BadImage("truncated")));
    }

    #[test]
    fn package_roundtrip() {
        let mut p = esp("0.1.0", 0);
        let l = p.len();
        p.extend_from_slice(&header(l));
        let signed = p.len();
        p.extend_from_slice(&t5a::tlv::SIGNATURE.to_le_bytes());
        p.extend_from_slice(&((KEY_ID_SIZE + SIGNATURE_SIZE) as u16).to_le_bytes());
        p.extend_from_slice(&[7; KEY_ID_SIZE + SIGNATURE_SIZE]);
        p.extend_from_slice(&[0xFF; 100]); // the rest of the slot
        let pkg = split(&p).unwrap();
        assert_eq!(pkg.signed.message.len(), signed);
        assert_eq!(pkg.total_size, signed + FOOTER_SIZE);
        assert_eq!(pkg.signed.key_id, [7; KEY_ID_SIZE]);
        p[l] = b'X';
        assert_eq!(split(&p).err(), Some(Error::BadHeader));
    }
}
