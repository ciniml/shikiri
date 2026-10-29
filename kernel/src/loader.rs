//! Loads a signed T5A v1 image (`t5_abi::t5a`, doc/14 §1) into memory:
//! verifies the Ed25519 signature, checks the TLVs, copies the image,
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
}

impl From<t5a::Error> for LoadError {
    fn from(e: t5a::Error) -> Self {
        LoadError::Image(e)
    }
}

#[derive(Debug)]
pub struct Loaded {
    pub region: Region,
    pub entry: usize,
    pub stack_top: usize,
    pub build_id: [u8; t5a::BUILD_ID_SIZE],
}

fn align_up(v: usize, a: usize) -> usize {
    v.div_ceil(a) * a
}

/// Checks the signature of `bytes` against the trusted keys and returns the
/// signed part.
pub fn verify(bytes: &[u8]) -> Result<&[u8], t5a::Error> {
    let s = t5a::split_signed(bytes)?;
    let key = TRUSTED_KEYS.iter().find(|k| t5a::key_id(k) == s.key_id).ok_or(t5a::Error::UnknownKey)?;
    ed25519_compact::PublicKey::new(*key)
        .verify(s.message, &ed25519_compact::Signature::new(s.signature))
        .map_err(|_| t5a::Error::BadSignature)?;
    Ok(s.message)
}

/// Verifies `bytes` and loads the image at `base`, which must be
/// `GRANULE`-aligned and have `capacity` writable bytes behind it.
///
/// # Safety
/// `[base, base + capacity)` must be memory the kernel owns and nothing else
/// uses, and must not overlap `bytes`.
pub unsafe fn load(bytes: &[u8], base: usize, capacity: usize) -> Result<Loaded, LoadError> {
    let started = Current::now();
    let signed = verify(bytes)?;
    let verify_us = time::ticks_to_ns(Current::now() - started) / 1000;
    let img = t5a::parse(signed, t5_abi::ABI_MAJOR, t5_abi::ABI_MINOR)?;
    let h = &img.header;
    let info = &img.info;
    event!(
        Info,
        "loader.verified",
        name = info.name,
        major = info.version[0],
        minor = info.version[1],
        patch = info.version[2],
        build_id = &info.build_id[..],
        verify_us = verify_us
    );

    let image_size = img.image.len();
    let text_size = h.text_size as usize;
    let mem_size = h.mem_size as usize;
    let stack_size = h.stack_size as usize;
    if text_size % GRANULE != 0
        || text_size > mem_size
        || image_size > mem_size
        || (h.entry as usize) >= text_size
        || stack_size % 16 != 0
    {
        return Err(LoadError::BadLayout);
    }
    let end_off = align_up(mem_size + stack_size, GRANULE);
    if base % GRANULE != 0 || end_off > capacity {
        return Err(LoadError::TooLarge { need: end_off, have: capacity });
    }

    unsafe {
        let dst = base as *mut u8;
        core::ptr::copy_nonoverlapping(img.image.as_ptr(), dst, image_size);
        core::ptr::write_bytes(dst.add(image_size), 0, end_off - image_size);

        for chunk in img.relocs.chunks_exact(t5a::RELOC_SIZE) {
            let r = t5a::Reloc::from_bytes(chunk);
            let o = r.offset as usize;
            // Only the writable part may be patched (elf2t5a checks this too).
            if o % 4 != 0 || o < text_size || o + 4 > mem_size {
                return Err(LoadError::BadReloc { offset: r.offset });
            }
            core::ptr::write_volatile(dst.add(o) as *mut u32, (base + r.addend as usize) as u32);
        }
    }

    // On ESP32-P4 the copy sits in the L1 D-cache; without this the core
    // fetches stale bytes from L2MEM (seen on hardware in M1b).
    <Current as Platform>::sync_code(base, end_off);

    Ok(Loaded {
        region: Region { base, text_end: base + text_size, end: base + end_off },
        entry: base + h.entry as usize,
        stack_top: base + mem_size + stack_size,
        build_id: info.build_id,
    })
}
