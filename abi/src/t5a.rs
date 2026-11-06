//! T5A image format, version 1 (doc/14 §1): a signed static-PIE app.
//!
//! ```text
//! +------------------------+ 0
//! | Header (48 bytes)      |
//! +------------------------+ header_size
//! | TLVs                   | tlv_size bytes
//! +------------------------+ header_size + tlv_size   (4-byte aligned)
//! | image                  | image_size bytes, loaded at the process base
//! +------------------------+
//! | Reloc[]                | reloc_count entries
//! +------------------------+ signed_size   <- the signature covers [0, signed_size)
//! | footer: Signature TLV  |
//! +------------------------+ total_size
//! ```
//!
//! The image is linked at address 0. Once loaded, `[0, text_size)` is text +
//! rodata (R-X), `[text_size, mem_size)` is data + bss (RW-), followed by
//! `stack_size` bytes of stack (RW-).
//!
//! An XIP image (`XipData` TLV, doc/15) runs its text from flash where it
//! lies: its data is linked at `XIP_DELTA` instead of right after the text.
//! The image bytes are still the text followed by the data's initial bytes,
//! `[XIP_DELTA, XIP_DELTA + mem_size)` is data + bss and stack follows it,
//! and the image starts on a 128-byte boundary in the file (the PMP
//! granularity), padded with a `Pad` TLV.
//!
//! A reader must verify the signature before trusting anything but
//! `signed_size` and `total_size`: see [`split_signed`].

pub const MAGIC: [u8; 4] = *b"T5A1";
pub const HEADER_VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 48;

/// One `R_RISCV_RELATIVE` relocation: `*(base + offset) = base + addend`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reloc {
    pub offset: u32,
    pub addend: u32,
}

pub const RELOC_SIZE: usize = 8;

impl Reloc {
    pub fn from_bytes(b: &[u8]) -> Reloc {
        Reloc { offset: u32_at(b, 0), addend: u32_at(b, 4) }
    }

    pub fn to_bytes(&self) -> [u8; RELOC_SIZE] {
        let mut out = [0u8; RELOC_SIZE];
        out[0..4].copy_from_slice(&self.offset.to_le_bytes());
        out[4..8].copy_from_slice(&self.addend.to_le_bytes());
        out
    }
}

/// TLV types. 0x40..=0x7F are "must understand": a reader that does not
/// know such a type rejects the image. Other unknown types are skipped.
pub mod tlv {
    pub const NAME: u16 = 0x01;
    pub const VERSION: u16 = 0x02;
    pub const ABI_VERSION: u16 = 0x03;
    pub const BUILD_ID: u16 = 0x04;
    pub const PERMISSIONS: u16 = 0x05;
    pub const FLAGS: u16 = 0x06;
    pub const SECURE_VERSION: u16 = 0x07;
    /// Filler that puts the image on a 128-byte boundary (XIP images).
    pub const PAD: u16 = 0x08;
    /// NUL-separated names the app needs before it starts (doc/51 §3):
    /// `svc:NAME` (a service is listening) or `ready:NAME` (a process has
    /// raised that mark). The kernel holds the start until all are there.
    pub const NEEDS: u16 = 0x09;
    /// u32: link address of the data of an XIP image (must be `XIP_DELTA`).
    pub const XIP_DATA: u16 = 0x40;
    pub const SIGNATURE: u16 = 0x80;

    pub const MUST_UNDERSTAND: core::ops::RangeInclusive<u16> = 0x40..=0x7F;

    /// `Flags` bit 0: start the app at boot.
    pub const FLAG_AUTOSTART: u32 = 1 << 0;
}

pub const MAX_NAME: usize = 32;
/// Distance from an XIP image's text to its data: the distance between the
/// flash and PSRAM windows on the ESP32-P4 (doc/15 §2).
pub const XIP_DELTA: u32 = 0x0800_0000;
/// Alignment of the image within an XIP T5A file.
pub const XIP_IMAGE_ALIGN: usize = 128;
pub const BUILD_ID_SIZE: usize = 16;
pub const KEY_ID_SIZE: usize = 8;
pub const SIGNATURE_SIZE: usize = 64;
/// The footer: one Signature TLV (type, length, key id, signature).
pub const FOOTER_SIZE: usize = 4 + KEY_ID_SIZE + SIGNATURE_SIZE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub header_version: u16,
    pub header_size: u16,
    pub tlv_size: u32,
    pub image_size: u32,
    pub text_size: u32,
    pub mem_size: u32,
    pub stack_size: u32,
    pub entry: u32,
    pub reloc_count: u32,
    pub signed_size: u32,
    pub total_size: u32,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

impl Header {
    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        out[0..4].copy_from_slice(&MAGIC);
        out[4..6].copy_from_slice(&self.header_version.to_le_bytes());
        out[6..8].copy_from_slice(&self.header_size.to_le_bytes());
        let words = [
            self.tlv_size,
            self.image_size,
            self.text_size,
            self.mem_size,
            self.stack_size,
            self.entry,
            self.reloc_count,
            self.signed_size,
            self.total_size,
        ];
        for (i, w) in words.iter().enumerate() {
            out[8 + i * 4..12 + i * 4].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    /// Parses the fixed header (magic and version only; no layout checks).
    pub fn from_bytes(b: &[u8]) -> Option<Header> {
        if b.len() < HEADER_SIZE || b[0..4] != MAGIC {
            return None;
        }
        Some(Header {
            header_version: u16_at(b, 4),
            header_size: u16_at(b, 6),
            tlv_size: u32_at(b, 8),
            image_size: u32_at(b, 12),
            text_size: u32_at(b, 16),
            mem_size: u32_at(b, 20),
            stack_size: u32_at(b, 24),
            entry: u32_at(b, 28),
            reloc_count: u32_at(b, 32),
            signed_size: u32_at(b, 36),
            total_size: u32_at(b, 40),
        })
    }

    pub fn image_offset(&self) -> usize {
        self.header_size as usize + self.tlv_size as usize
    }

    pub fn relocs_offset(&self) -> usize {
        self.image_offset() + self.image_size as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not a T5A v1 image, or too short for its own size fields.
    BadHeader,
    /// The signature footer is missing or malformed.
    BadFooter,
    /// No trusted key has this id.
    UnknownKey,
    BadSignature,
    /// Inconsistent sizes or offsets (after the signature checked out, so
    /// this is a builder bug, not tampering).
    BadLayout,
    BadTlv,
    MissingTlv(u16),
    /// A must-understand TLV this reader does not know.
    UnknownCriticalTlv(u16),
    AbiMismatch { major: u16, minor: u16 },
}

/// The parts of an image before its signature is checked.
pub struct Signed<'a> {
    /// `[0, signed_size)`: what the signature covers.
    pub message: &'a [u8],
    pub key_id: [u8; KEY_ID_SIZE],
    pub signature: [u8; SIGNATURE_SIZE],
}

/// Splits an image into the signed bytes and the signature, reading only
/// `signed_size` and `total_size` from the header.
pub fn split_signed(b: &[u8]) -> Result<Signed<'_>, Error> {
    let h = Header::from_bytes(b).ok_or(Error::BadHeader)?;
    let signed = h.signed_size as usize;
    let total = h.total_size as usize;
    if signed < HEADER_SIZE || total != signed + FOOTER_SIZE || total > b.len() {
        return Err(Error::BadHeader);
    }
    let f = &b[signed..total];
    if u16_at(f, 0) != tlv::SIGNATURE || u16_at(f, 2) as usize != KEY_ID_SIZE + SIGNATURE_SIZE {
        return Err(Error::BadFooter);
    }
    let mut key_id = [0u8; KEY_ID_SIZE];
    key_id.copy_from_slice(&f[4..4 + KEY_ID_SIZE]);
    let mut signature = [0u8; SIGNATURE_SIZE];
    signature.copy_from_slice(&f[4 + KEY_ID_SIZE..]);
    Ok(Signed { message: &b[..signed], key_id, signature })
}

/// The key id of a public key: its first 8 bytes. Public keys are not
/// secret, and the id only selects among trusted keys.
pub fn key_id(public_key: &[u8; 32]) -> [u8; KEY_ID_SIZE] {
    let mut id = [0u8; KEY_ID_SIZE];
    id.copy_from_slice(&public_key[..KEY_ID_SIZE]);
    id
}

/// Iterates over the TLVs of a TLV area: (type, value).
pub struct Tlvs<'a> {
    rest: &'a [u8],
    bad: bool,
}

impl<'a> Tlvs<'a> {
    pub fn new(area: &'a [u8]) -> Self {
        Tlvs { rest: area, bad: false }
    }
}

impl<'a> Iterator for Tlvs<'a> {
    type Item = Result<(u16, &'a [u8]), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.bad || self.rest.is_empty() {
            return None;
        }
        if self.rest.len() < 4 {
            self.bad = true;
            return Some(Err(Error::BadTlv));
        }
        let t = u16_at(self.rest, 0);
        let len = u16_at(self.rest, 2) as usize;
        let padded = (4 + len).next_multiple_of(4);
        if padded > self.rest.len() {
            self.bad = true;
            return Some(Err(Error::BadTlv));
        }
        let value = &self.rest[4..4 + len];
        self.rest = &self.rest[padded..];
        Some(Ok((t, value)))
    }
}

/// Appends one TLV (with padding) to `out`.
pub fn push_tlv(out: &mut [u8], at: usize, t: u16, value: &[u8]) -> Option<usize> {
    let padded = (4 + value.len()).next_multiple_of(4);
    let dst = out.get_mut(at..at + padded)?;
    dst.fill(0);
    dst[0..2].copy_from_slice(&t.to_le_bytes());
    dst[2..4].copy_from_slice(&(value.len() as u16).to_le_bytes());
    dst[4..4 + value.len()].copy_from_slice(value);
    Some(at + padded)
}

/// What the TLVs say about an app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info<'a> {
    pub name: &'a str,
    pub version: [u32; 3],
    pub abi: (u16, u16),
    pub build_id: [u8; BUILD_ID_SIZE],
    /// NUL-separated names `open` may use.
    pub permissions: &'a [u8],
    /// NUL-separated names (the Needs TLV).
    pub needs: &'a [u8],
    pub flags: u32,
    pub secure_version: u32,
    /// `Some(XIP_DELTA)` for an XIP image.
    pub xip_data: Option<u32>,
}

impl Info<'_> {
    /// Whether `open(name)` is allowed by the Permissions TLV.
    pub fn permits(&self, name: &[u8]) -> bool {
        permits(self.permissions, name)
    }
}

/// Whether the NUL-separated `permissions` (the Permissions TLV) allow
/// `open(name)`: `name` itself, or `name/GRANT` (doc/52 §3.1: the object
/// with part of what it offers).
pub fn permits(permissions: &[u8], name: &[u8]) -> bool {
    entries(permissions).any(|p| p == name || (p.len() > name.len() && p.starts_with(name) && p[name.len()] == b'/'))
}

/// Whether `permissions` name `name` itself (all of the object).
pub fn permits_all(permissions: &[u8], name: &[u8]) -> bool {
    entries(permissions).any(|p| p == name)
}

/// The grants on `name`: GRANT of each `name/GRANT` (doc/52 §3.1).
pub fn grants<'a>(permissions: &'a [u8], name: &'a [u8]) -> impl Iterator<Item = &'a [u8]> + 'a {
    entries(permissions).filter_map(move |p| {
        (p.len() > name.len() && p.starts_with(name) && p[name.len()] == b'/').then(|| &p[name.len() + 1..])
    })
}

fn entries(permissions: &[u8]) -> impl Iterator<Item = &[u8]> {
    permissions.split(|&b| b == 0).filter(|p| !p.is_empty())
}

/// Permissions only images signed with a system key may hold (doc/52 §4):
/// serving a service, the devices a service owns, the keyboard, readiness
/// marks, starting or stopping any app, files outside the app's own
/// folder, secrets in the settings, changing the network, a CPU partition
/// (doc/57), and the Wi-Fi chip's firmware (doc/58 R5).
pub fn strong(entry: &[u8]) -> bool {
    const EXACT: [&[u8]; 10] =
        [b"sd", b"wlan:ctl", b"bt:hci", b"flash:cfg", b"kbd", b"proc:kill", b"proc:kill/any", b"proc:spawn", b"svc:net/link", b"svc:wifi/ota"];
    const PREFIX: [&[u8]; 7] = [b"listen:", b"netdev:", b"ready:", b"svc:fs/", b"svc:cfg/secret:", b"svc:wifi/join", b"cpu:"];
    EXACT.contains(&entry) || PREFIX.iter().any(|p| entry.starts_with(p))
}

/// A verified image: the header, what its TLVs say, and its parts.
pub struct Image<'a> {
    pub header: Header,
    pub info: Info<'a>,
    pub image: &'a [u8],
    pub relocs: &'a [u8],
}

/// Parses the signed part of an image. Call only on bytes whose signature
/// has been verified (`Signed::message`).
pub fn parse(signed: &[u8], abi_major: u16, abi_minor: u16) -> Result<Image<'_>, Error> {
    parse_inner(signed, Some((abi_major, abi_minor)))
}

/// `parse` without the ABI check, for tools that only describe images.
pub fn parse_any_abi(signed: &[u8]) -> Result<Image<'_>, Error> {
    parse_inner(signed, None)
}

fn parse_inner(signed: &[u8], want_abi: Option<(u16, u16)>) -> Result<Image<'_>, Error> {
    let h = Header::from_bytes(signed).ok_or(Error::BadHeader)?;
    let image_off = h.image_offset();
    let relocs_off = h.relocs_offset();
    let relocs_len = (h.reloc_count as usize).checked_mul(RELOC_SIZE).ok_or(Error::BadLayout)?;
    if h.header_version != HEADER_VERSION
        || (h.header_size as usize) < HEADER_SIZE
        || image_off % 4 != 0
        || relocs_off.checked_add(relocs_len) != Some(signed.len())
    {
        return Err(Error::BadLayout);
    }

    let mut name = None;
    let mut version = None;
    let mut abi = None;
    let mut build_id = None;
    let mut permissions: &[u8] = &[];
    let mut needs: &[u8] = &[];
    let mut flags = 0;
    let mut secure_version = 0;
    let mut xip_data = None;
    for item in Tlvs::new(&signed[h.header_size as usize..image_off]) {
        let (t, v) = item?;
        match t {
            tlv::NAME => {
                let s = core::str::from_utf8(v).map_err(|_| Error::BadTlv)?;
                let ok = !s.is_empty()
                    && s.len() <= MAX_NAME
                    && s.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-');
                if !ok {
                    return Err(Error::BadTlv);
                }
                name = Some(s);
            }
            tlv::VERSION if v.len() == 12 => version = Some([u32_at(v, 0), u32_at(v, 4), u32_at(v, 8)]),
            tlv::ABI_VERSION if v.len() == 4 => abi = Some((u16_at(v, 0), u16_at(v, 2))),
            tlv::BUILD_ID if v.len() == BUILD_ID_SIZE => {
                let mut id = [0u8; BUILD_ID_SIZE];
                id.copy_from_slice(v);
                build_id = Some(id);
            }
            tlv::PERMISSIONS => permissions = v,
            tlv::NEEDS => needs = v,
            tlv::FLAGS if v.len() == 4 => flags = u32_at(v, 0),
            tlv::SECURE_VERSION if v.len() == 4 => secure_version = u32_at(v, 0),
            tlv::XIP_DATA if v.len() == 4 && u32_at(v, 0) == XIP_DELTA => xip_data = Some(XIP_DELTA),
            tlv::XIP_DATA => return Err(Error::BadTlv),
            tlv::VERSION | tlv::ABI_VERSION | tlv::BUILD_ID | tlv::FLAGS | tlv::SECURE_VERSION => {
                return Err(Error::BadTlv);
            }
            t if tlv::MUST_UNDERSTAND.contains(&t) => return Err(Error::UnknownCriticalTlv(t)),
            _ => {}
        }
    }
    let name = name.ok_or(Error::MissingTlv(tlv::NAME))?;
    let version = version.ok_or(Error::MissingTlv(tlv::VERSION))?;
    let abi = abi.ok_or(Error::MissingTlv(tlv::ABI_VERSION))?;
    let build_id = build_id.ok_or(Error::MissingTlv(tlv::BUILD_ID))?;
    if xip_data.is_some() && image_off % XIP_IMAGE_ALIGN != 0 {
        return Err(Error::BadLayout);
    }
    if let Some((major, minor)) = want_abi
        && (abi.0 != major || abi.1 > minor)
    {
        return Err(Error::AbiMismatch { major: abi.0, minor: abi.1 });
    }
    Ok(Image {
        header: h,
        info: Info { name, version, abi, build_id, permissions, needs, flags, secure_version, xip_data },
        image: &signed[image_off..relocs_off],
        relocs: &signed[relocs_off..],
    })
}
