//! App store layout in flash (doc/14 §2, doc/05 §5.2).
//!
//! - `appmeta` (0x41_0000, 64 KiB): sectors 0 and 1 (4 KiB each) hold two
//!   copies of the catalog (A/B). The valid copy with the higher `seq` wins,
//!   so a catalog update that is cut short leaves the old one in force.
//! - `apps` (0x42_0000, 8 MiB): 128 extents of 64 KiB (one MMU page each).
//!   An app occupies `extent_count` consecutive extents from `extent_first`.
//!
//! Catalog sector:
//! ```text
//! 0   magic "T5AM" | u16 version | u16 count | u32 seq | u32 reserved
//! 16  entries[count], 64 bytes each
//! 4092 u32 crc32 over bytes [0, 4092)
//! ```
//! Entry: `name[32] | u16 extent_first | u16 extent_count | u32 length |
//! u32 version[3] | u32 secure_version | u8 state | u8 flags | u16 reserved`.
//! The image itself is trusted only after its signature is verified.

pub const APPMETA_OFFSET: u32 = 0x41_0000;
pub const APPMETA_SIZE: u32 = 0x1_0000;
pub const SECTOR_SIZE: usize = 4096;
pub const APPS_OFFSET: u32 = 0x42_0000;
pub const APPS_SIZE: u32 = 0x80_0000;
pub const EXTENT_SIZE: u32 = 0x1_0000;
pub const EXTENTS: u32 = APPS_SIZE / EXTENT_SIZE;

pub const MAGIC: [u8; 4] = *b"T5AM";
/// v2 (M5b, doc/16 §4.1) adds `State::Previous`; v1 catalogs still read.
pub const VERSION: u16 = 2;
pub const MIN_VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 16;
pub const ENTRY_SIZE: usize = 64;
pub const MAX_ENTRIES: usize = (SECTOR_SIZE - HEADER_SIZE - 4) / ENTRY_SIZE;

/// Install state (doc/05 §5.2, doc/16 §4). A name has at most one current
/// entry (`Valid` or `PendingVerify`) and one `Previous`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    New = 0,
    PendingVerify = 1,
    Valid = 2,
    Invalid = 3,
    /// The version an update replaced, kept until the update is confirmed.
    Previous = 4,
}

impl State {
    pub fn from_u8(v: u8) -> Option<State> {
        Some(match v {
            0 => State::New,
            1 => State::PendingVerify,
            2 => State::Valid,
            3 => State::Invalid,
            4 => State::Previous,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: [u8; 32],
    pub extent_first: u16,
    pub extent_count: u16,
    pub length: u32,
    pub version: [u32; 3],
    pub secure_version: u32,
    pub state: State,
    pub flags: u8,
}

impl Entry {
    pub fn name(&self) -> &str {
        let n = self.name.iter().position(|&b| b == 0).unwrap_or(self.name.len());
        core::str::from_utf8(&self.name[..n]).unwrap_or("")
    }

    /// The version `run` and autostart use (not one kept to go back to).
    pub fn is_current(&self) -> bool {
        self.state != State::Previous
    }

    /// Flash offset of the image.
    pub fn offset(&self) -> u32 {
        APPS_OFFSET + self.extent_first as u32 * EXTENT_SIZE
    }

    /// The extents fit in `apps` and hold the image.
    pub fn in_bounds(&self) -> bool {
        let end = self.extent_first as u32 + self.extent_count as u32;
        self.extent_count > 0 && end <= EXTENTS && self.length <= self.extent_count as u32 * EXTENT_SIZE
    }

    pub fn to_bytes(&self) -> [u8; ENTRY_SIZE] {
        let mut b = [0u8; ENTRY_SIZE];
        b[0..32].copy_from_slice(&self.name);
        b[32..34].copy_from_slice(&self.extent_first.to_le_bytes());
        b[34..36].copy_from_slice(&self.extent_count.to_le_bytes());
        b[36..40].copy_from_slice(&self.length.to_le_bytes());
        for (i, v) in self.version.iter().enumerate() {
            b[40 + i * 4..44 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        b[52..56].copy_from_slice(&self.secure_version.to_le_bytes());
        b[56] = self.state as u8;
        b[57] = self.flags;
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<Entry> {
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let mut name = [0u8; 32];
        name.copy_from_slice(&b[0..32]);
        Some(Entry {
            name,
            extent_first: u16_at(32),
            extent_count: u16_at(34),
            length: u32_at(36),
            version: [u32_at(40), u32_at(44), u32_at(48)],
            secure_version: u32_at(52),
            state: State::from_u8(b[56])?,
            flags: b[57],
        })
    }
}

/// CRC-32 (IEEE 802.3, reflected, as zlib's `crc32`).
pub const fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    let mut i = 0;
    while i < data.len() {
        crc ^= data[i] as u32;
        let mut k = 0;
        while k < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            k += 1;
        }
        i += 1;
    }
    !crc
}

/// A catalog sector that passed its checks.
#[derive(Clone, Copy)]
pub struct Catalog<'a> {
    pub seq: u32,
    pub count: usize,
    sector: &'a [u8],
}

impl<'a> Catalog<'a> {
    pub fn parse(sector: &'a [u8]) -> Option<Catalog<'a>> {
        if sector.len() < SECTOR_SIZE || sector[0..4] != MAGIC {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes([sector[o], sector[o + 1], sector[o + 2], sector[o + 3]]);
        let version = u16::from_le_bytes([sector[4], sector[5]]);
        let count = u16::from_le_bytes([sector[6], sector[7]]) as usize;
        if !(MIN_VERSION..=VERSION).contains(&version) || count > MAX_ENTRIES || u32_at(SECTOR_SIZE - 4) != crc32(&sector[..SECTOR_SIZE - 4]) {
            return None;
        }
        Some(Catalog { seq: u32_at(8), count, sector })
    }

    /// Entries that decode and fit; others are skipped.
    pub fn entries(self) -> impl Iterator<Item = Entry> + Clone + 'a {
        let sector = self.sector;
        (0..self.count)
            .filter_map(move |i| Entry::from_bytes(&sector[HEADER_SIZE + i * ENTRY_SIZE..][..ENTRY_SIZE]))
            .filter(Entry::in_bounds)
    }
}

/// Picks the catalog in force from the two `appmeta` sectors: the valid
/// one, or of two valid ones the newer (serial-number comparison, so `seq`
/// may wrap).
pub fn active<'a>(a: &'a [u8], b: &'a [u8]) -> Option<Catalog<'a>> {
    active_index(a, b).map(|i| Catalog::parse(if i == 0 { a } else { b }).unwrap())
}

/// Which of the two sectors (0 or 1) `active` picks. An update writes the
/// other one.
pub fn active_index(a: &[u8], b: &[u8]) -> Option<usize> {
    match (Catalog::parse(a), Catalog::parse(b)) {
        (Some(x), Some(y)) => Some(if (y.seq.wrapping_sub(x.seq) as i32) > 0 { 1 } else { 0 }),
        (Some(_), None) => Some(0),
        (None, Some(_)) => Some(1),
        (None, None) => None,
    }
}

/// The first run of `count` extents that none of `entries` uses.
pub fn find_free(entries: impl Iterator<Item = Entry> + Clone, count: u16) -> Option<u16> {
    if count == 0 || count as u32 > EXTENTS {
        return None;
    }
    let mut first = 0u32;
    while first + count as u32 <= EXTENTS {
        let end = first + count as u32;
        // The entry that overlaps [first, end) and ends last, if any.
        let blocking = entries
            .clone()
            .filter(|e| (e.extent_first as u32) < end && first < e.extent_first as u32 + e.extent_count as u32)
            .map(|e| e.extent_first as u32 + e.extent_count as u32)
            .max();
        match blocking {
            None => return Some(first as u16),
            Some(next) => first = next,
        }
    }
    None
}

/// Extents needed for `length` bytes.
pub fn extents_for(length: u32) -> u16 {
    length.div_ceil(EXTENT_SIZE) as u16
}

/// Builds a catalog sector (host tools, and the kernel's installer).
/// Returns false if there are more than `MAX_ENTRIES` entries.
pub fn build_sector(seq: u32, entries: impl IntoIterator<Item = Entry>, out: &mut [u8; SECTOR_SIZE]) -> bool {
    out.fill(0xFF);
    let mut count = 0;
    for e in entries {
        if count == MAX_ENTRIES {
            return false;
        }
        out[HEADER_SIZE + count * ENTRY_SIZE..][..ENTRY_SIZE].copy_from_slice(&e.to_bytes());
        count += 1;
    }
    out[0..4].copy_from_slice(&MAGIC);
    out[4..6].copy_from_slice(&VERSION.to_le_bytes());
    out[6..8].copy_from_slice(&(count as u16).to_le_bytes());
    out[8..12].copy_from_slice(&seq.to_le_bytes());
    out[12..16].fill(0);
    let crc = crc32(&out[..SECTOR_SIZE - 4]);
    out[SECTOR_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());
    true
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::ToString;
    use std::vec::Vec;

    fn entry(name: &str, first: u16) -> Entry {
        let mut n = [0u8; 32];
        n[..name.len()].copy_from_slice(name.as_bytes());
        Entry {
            name: n,
            extent_first: first,
            extent_count: 1,
            length: 1000,
            version: [1, 0, 0],
            secure_version: 0,
            state: State::Valid,
            flags: 0,
        }
    }

    #[test]
    fn crc32_matches_zlib() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn newer_valid_copy_wins() {
        let mut a = [0u8; SECTOR_SIZE];
        let mut b = [0u8; SECTOR_SIZE];
        assert!(build_sector(7, [entry("hello", 0)], &mut a));
        assert!(build_sector(8, [entry("hello", 0), entry("spin", 1)], &mut b));
        let c = active(&a, &b).unwrap();
        assert_eq!((c.seq, c.entries().count()), (8, 2));
        assert_eq!(c.entries().nth(1).unwrap().name(), "spin");
        // A torn (corrupted) newer copy falls back to the older one.
        b[100] ^= 1;
        assert_eq!(active(&a, &b).unwrap().seq, 7);
        // Erased flash is not a catalog.
        assert!(active(&[0xFF; SECTOR_SIZE], &[0xFF; SECTOR_SIZE]).is_none());
        // Sequence numbers may wrap.
        assert!(build_sector(u32::MAX, [], &mut a));
        assert!(build_sector(0, [], &mut b));
        assert_eq!(active(&a, &b).unwrap().seq, 0);
    }

    #[test]
    fn free_extents() {
        let mut big = entry("big", 2);
        big.extent_count = 3;
        let es = [entry("a", 0), big, entry("c", 6)];
        assert_eq!(find_free(es.iter().copied(), 1), Some(1));
        assert_eq!(find_free(es.iter().copied(), 2), Some(7));
        assert_eq!(find_free(es.iter().copied(), 127), None);
        assert_eq!(find_free([].into_iter(), 128), Some(0));
        assert_eq!(find_free([].into_iter(), 0), None);
        assert_eq!((extents_for(1), extents_for(EXTENT_SIZE), extents_for(EXTENT_SIZE + 1)), (1, 1, 2));
        let mut a = [0u8; SECTOR_SIZE];
        let mut b = [0xFFu8; SECTOR_SIZE];
        assert!(build_sector(3, [], &mut a));
        assert_eq!(active_index(&a, &b), Some(0));
        assert!(build_sector(4, [], &mut b));
        assert_eq!(active_index(&a, &b), Some(1));
    }

    #[test]
    fn v1_catalogs_and_previous_entries() {
        let mut s = [0u8; SECTOR_SIZE];
        let mut prev = entry("app", 1);
        prev.state = State::Previous;
        assert!(build_sector(1, [entry("app", 0), prev], &mut s));
        let es: Vec<_> = Catalog::parse(&s).unwrap().entries().collect();
        assert_eq!(es[1].state, State::Previous);
        assert!(es[0].is_current() && !es[1].is_current());
        // A v1 sector (written before M5b) still reads.
        s[4..6].copy_from_slice(&1u16.to_le_bytes());
        let crc = crc32(&s[..SECTOR_SIZE - 4]);
        s[SECTOR_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(Catalog::parse(&s).unwrap().count, 2);
        s[4..6].copy_from_slice(&3u16.to_le_bytes());
        let crc = crc32(&s[..SECTOR_SIZE - 4]);
        s[SECTOR_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());
        assert!(Catalog::parse(&s).is_none());
    }

    #[test]
    fn out_of_bounds_entries_are_skipped() {
        let mut s = [0u8; SECTOR_SIZE];
        let mut bad = entry("big", 127);
        bad.extent_count = 2;
        assert!(build_sector(1, [bad, entry("ok", 0)], &mut s));
        let names: Vec<_> = Catalog::parse(&s).unwrap().entries().map(|e| e.name().to_string()).collect();
        assert_eq!(names, ["ok"]);
    }
}
