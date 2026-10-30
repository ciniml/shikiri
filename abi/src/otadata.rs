//! The kernel slots (ota_0, ota_1) and the ESP-IDF bootloader's otadata
//! (doc/16 §1): two sectors, each starting with one
//! `esp_ota_select_entry_t` (bootloader_support/include/
//! esp_flash_partitions.h:66-84). The partition offsets are those of
//! boot/partitions.csv.

pub const OTADATA_OFFSET: u32 = 0xD000;
pub const SECTOR_SIZE: u32 = 4096;
pub const SLOTS: [u32; 2] = [0x1_0000, 0x21_0000];
pub const SLOT_SIZE: u32 = 0x20_0000;
pub const ENTRY_SIZE: usize = 32;

/// `esp_ota_img_states_t`.
pub mod state {
    pub const NEW: u32 = 0;
    pub const PENDING_VERIFY: u32 = 1;
    pub const VALID: u32 = 2;
    pub const INVALID: u32 = 3;
    pub const ABORTED: u32 = 4;
    pub const UNDEFINED: u32 = u32::MAX;
}

/// CRC-32 of `ota_seq` as the bootloader computes it:
/// `esp_rom_crc32_le(UINT32_MAX, &seq, 4)`, i.e. the reflected IEEE CRC
/// with initial register 0 and final inversion (bootloader_common_loader.c:
/// 66-69). zlib's CRC-32 starts from 0xFFFF_FFFF instead.
pub const fn seq_crc(seq: u32) -> u32 {
    let b = seq.to_le_bytes();
    let mut crc = 0u32;
    let mut i = 0;
    while i < 4 {
        crc ^= b[i] as u32;
        let mut k = 0;
        while k < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            k += 1;
        }
        i += 1;
    }
    !crc
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub seq: u32,
    pub label: [u8; 20],
    pub state: u32,
    pub crc: u32,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl Entry {
    /// A new entry for `seq` in `state`; the label stays erased.
    pub fn new(seq: u32, state: u32) -> Entry {
        Entry { seq, label: [0xFF; 20], state, crc: seq_crc(seq) }
    }

    pub fn from_bytes(b: &[u8]) -> Entry {
        let mut label = [0u8; 20];
        label.copy_from_slice(&b[4..24]);
        Entry { seq: u32_at(b, 0), label, state: u32_at(b, 24), crc: u32_at(b, 28) }
    }

    pub fn to_bytes(&self) -> [u8; ENTRY_SIZE] {
        let mut out = [0u8; ENTRY_SIZE];
        out[0..4].copy_from_slice(&self.seq.to_le_bytes());
        out[4..24].copy_from_slice(&self.label);
        out[24..28].copy_from_slice(&self.state.to_le_bytes());
        out[28..32].copy_from_slice(&self.crc.to_le_bytes());
        out
    }

    pub fn crc_ok(&self) -> bool {
        self.crc == seq_crc(self.seq)
    }

    /// Erased (all 0xFF) or never written.
    pub fn is_initial(&self) -> bool {
        self.seq == u32::MAX
    }

    /// `bootloader_common_ota_select_valid`.
    pub fn is_valid(&self) -> bool {
        !self.is_initial() && self.state != state::INVALID && self.state != state::ABORTED && self.crc_ok()
    }

    /// The slot (0 = ota_0, 1 = ota_1) this entry boots.
    pub fn slot(&self) -> usize {
        (self.seq.wrapping_sub(1) % 2) as usize
    }
}

/// The entry the bootloader boots: the valid one with the larger sequence
/// number (`bootloader_common_get_active_otadata`).
pub fn active(e: &[Entry; 2]) -> Option<usize> {
    match (e[0].is_valid(), e[1].is_valid()) {
        (true, true) => Some(if e[0].seq >= e[1].seq { 0 } else { 1 }),
        (true, false) => Some(0),
        (false, true) => Some(1),
        (false, false) => None,
    }
}

/// Where an entry selecting `slot` goes, and its sequence number: the
/// sector without the active entry, and the smallest number above every
/// written one that maps to `slot` (as `esp_rewrite_ota_data` and
/// esp-bootloader-esp-idf's `set_current_slot`).
pub fn next(e: &[Entry; 2], slot: usize) -> (usize, u32) {
    let sector = match active(e) {
        Some(i) => 1 - i,
        None => 0,
    };
    let max = e.iter().filter(|x| !x.is_initial() && x.crc_ok()).map(|x| x.seq).max().unwrap_or(0);
    let mut seq = max + 1;
    if (seq - 1) % 2 != slot as u32 {
        seq += 1;
    }
    (sector, seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_bootloader() {
        // Python's binascii.crc32(data, 0xFFFFFFFF) starts its register
        // at !0xFFFF_FFFF = 0, as the ROM does.
        assert_eq!(seq_crc(1), 0x4743_989A);
        assert_eq!(seq_crc(2), 0x55F6_3774);
        assert_ne!(seq_crc(1), crate::store::crc32(&1u32.to_le_bytes()));
    }

    #[test]
    fn selection() {
        let erased = Entry::from_bytes(&[0xFF; 32]);
        assert!(erased.is_initial() && !erased.is_valid());
        // Fresh otadata: an update to ota_1 goes into sector 0 with seq 2.
        assert_eq!(next(&[erased, erased], 1), (0, 2));
        // The bootloader wrote seq 1 VALID into sector 0 (ota_0).
        let one = Entry::new(1, state::VALID);
        let e = [one, erased];
        assert_eq!(active(&e), Some(0));
        assert_eq!(next(&e, 1), (1, 2));
        // After the update: seq 2 (ota_1) is the active one.
        let two = Entry::new(2, state::NEW);
        let e = [one, two];
        assert_eq!(active(&e), Some(1));
        assert_eq!(e[1].slot(), 1);
        // Back to ota_0: seq 3 in sector 0.
        assert_eq!(next(&e, 0), (0, 3));
        // An aborted update falls back to the other entry.
        let e = [one, Entry::new(2, state::ABORTED)];
        assert_eq!(active(&e), Some(0));
        // ...and the next update to ota_1 skips the aborted number.
        assert_eq!(next(&e, 1), (1, 4));
        let bad_crc = Entry { crc: 0, ..Entry::new(5, state::VALID) };
        assert!(!bad_crc.is_valid());
    }

    #[test]
    fn roundtrip() {
        let e = Entry::new(7, state::PENDING_VERIFY);
        assert_eq!(Entry::from_bytes(&e.to_bytes()), e);
    }
}
