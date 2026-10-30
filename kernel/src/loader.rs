//! Loads a signed T5A v1 image (`t5_abi::t5a`, doc/14 §1) into memory:
//! verifies the Ed25519 signature, checks the TLVs, copies the image (or,
//! for an XIP image, maps its text where it lies in flash, doc/15),
//! zero-fills bss, applies R_RISCV_RELATIVE relocations and computes the PMP
//! region.
//!
//! Nothing in the image is trusted before the signature checks out: only
//! `signed_size` and `total_size` are read to find the signature.

use t5_abi::event;
use t5_abi::t5a;

use crate::arch::riscv32::pmp::{GRANULE, Region};
use crate::platform::{Current, Platform};
use crate::time;

include!(concat!(env!("OUT_DIR"), "/keys.rs"));

// Fields are only read through Debug when a load failure is reported.
#[allow(dead_code)]
#[derive(Debug)]
pub enum LoadError {
    /// Rejected by signature or TLV checks.
    Image(t5a::Error),
    BadLayout,
    TooLarge { need: usize, have: usize },
    BadReloc { offset: u32 },
    /// An XIP image not read from the flash window, or no XIP here.
    NoXip,
}

impl From<t5a::Error> for LoadError {
    fn from(e: t5a::Error) -> Self {
        LoadError::Image(e)
    }
}

#[derive(Debug)]
pub struct Loaded {
    /// From the signed `Name` TLV.
    pub name: &'static str,
    pub region: Region,
    pub entry: usize,
    pub stack_top: usize,
    pub build_id: [u8; t5a::BUILD_ID_SIZE],
    /// The signed Permissions TLV.
    pub permissions: &'static [u8],
    /// The text runs from flash where it lies.
    pub xip: bool,
}

fn align_up(v: usize, a: usize) -> usize {
    v.div_ceil(a) * a
}

/// Checks the signature of `bytes` against the trusted keys and returns the
/// signed part.
pub fn verify(bytes: &[u8]) -> Result<&[u8], t5a::Error> {
    let s = t5a::split_signed(bytes)?;
    check_signature(&s)?;
    Ok(s.message)
}

/// Checks a signature against the trusted keys (apps and kernel packages
/// alike, doc/16 §2).
pub fn check_signature(s: &t5a::Signed) -> Result<(), t5a::Error> {
    let key = TRUSTED_KEYS.iter().find(|k| t5a::key_id(k) == s.key_id).ok_or(t5a::Error::UnknownKey)?;
    ed25519_compact::PublicKey::new(*key)
        .verify(s.message, &ed25519_compact::Signature::new(s.signature))
        .map_err(|_| t5a::Error::BadSignature)
}

/// Where a process goes: its slot number and the slot's memory (for images
/// that are copied).
pub struct Slot {
    pub index: usize,
    pub base: usize,
    pub capacity: usize,
}

/// Verifies `bytes` and loads the image into `slot`.
///
/// # Safety
/// The slot's memory must be the kernel's and unused, and must not overlap
/// `bytes`.
pub unsafe fn load(bytes: &'static [u8], slot: &Slot) -> Result<Loaded, LoadError> {
    let started = Current::now();
    let signed = verify(bytes)?;
    let verify_us = time::ticks_to_ns(Current::now() - started) / 1000;
    let img = t5a::parse(signed, t5_abi::ABI_MAJOR, t5_abi::ABI_MINOR)?;
    let info = &img.info;
    event!(
        Info,
        "loader.verified",
        name = info.name,
        major = info.version[0],
        minor = info.version[1],
        patch = info.version[2],
        build_id = &info.build_id[..],
        xip = info.xip_data.is_some(),
        verify_us = verify_us
    );
    let h = &img.header;
    let (image_size, text_size, mem_size, stack_size) =
        (img.image.len(), h.text_size as usize, h.mem_size as usize, h.stack_size as usize);
    if text_size % GRANULE != 0 || (h.entry as usize) >= text_size || stack_size % 16 != 0 || image_size < text_size {
        return Err(LoadError::BadLayout);
    }
    let data_init = &img.image[text_size..];

    // `text`: where link address 0 runs; `data_link`: the data's link
    // address; `data_link_end`: the end of data + bss. Every link address
    // `a` runs at `text + a`. `mem_size` counts from 0 for a copied image
    // and from the data for an XIP one.
    let (text, data_link, data_link_end) = match info.xip_data {
        None => {
            // Copied: the data follows the text.
            if text_size > mem_size {
                return Err(LoadError::BadLayout);
            }
            let end_off = align_up(mem_size + stack_size, GRANULE);
            if slot.base % GRANULE != 0 || end_off > slot.capacity {
                return Err(LoadError::TooLarge { need: end_off, have: slot.capacity });
            }
            <Current as Platform>::retire_alias(slot.base, end_off);
            unsafe { core::ptr::copy_nonoverlapping(img.image.as_ptr(), slot.base as *mut u8, text_size) };
            (slot.base, text_size, mem_size)
        }
        Some(delta) => {
            // XIP: the text stays in flash, mapped into the slot.
            let offset = <Current as Platform>::flash_offset(bytes.as_ptr() as usize).ok_or(LoadError::NoXip)?;
            let image_off = h.image_offset();
            let pages = (image_off + text_size).div_ceil(64 << 10);
            let vbase = <Current as Platform>::map_xip(slot.index, offset, pages).ok_or(LoadError::NoXip)?;
            let need = image_off + align_up(mem_size + stack_size, GRANULE);
            if need > slot.capacity {
                return Err(LoadError::TooLarge { need, have: slot.capacity });
            }
            (vbase + image_off, delta as usize, delta as usize + mem_size)
        }
    };
    if data_init.len() > data_link_end - data_link {
        return Err(LoadError::BadLayout);
    }
    let data = text + data_link;
    let data_end = text + align_up(data_link_end + stack_size, GRANULE);
    if info.xip_data.is_some() {
        <Current as Platform>::retire_alias(data, data_end - data);
    }

    unsafe {
        core::ptr::copy_nonoverlapping(data_init.as_ptr(), data as *mut u8, data_init.len());
        core::ptr::write_bytes((data + data_init.len()) as *mut u8, 0, data_end - data - data_init.len());

        for chunk in img.relocs.chunks_exact(t5a::RELOC_SIZE) {
            let r = t5a::Reloc::from_bytes(chunk);
            let o = r.offset as usize;
            // Only the writable part may be patched (elf2t5a checks this too).
            if o % 4 != 0 || o < data_link || o + 4 > data_link_end {
                return Err(LoadError::BadReloc { offset: r.offset });
            }
            core::ptr::write_volatile((text + o) as *mut u32, (text + r.addend as usize) as u32);
        }
    }

    // On ESP32-P4 a copy sits in the L1 D-cache (and L2 for PSRAM); without
    // this the core fetches stale bytes (seen on hardware in M1b). The XIP
    // text was invalidated when it was mapped.
    if info.xip_data.is_none() {
        <Current as Platform>::sync_code(text, data_end - text);
    }

    Ok(Loaded {
        name: info.name,
        region: Region { text: text..text + text_size, data: data..data_end, shared: None, shm: None },
        entry: text + h.entry as usize,
        stack_top: text + data_link_end + stack_size,
        build_id: info.build_id,
        permissions: info.permissions,
        xip: info.xip_data.is_some(),
    })
}
