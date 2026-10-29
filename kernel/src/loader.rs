//! Loads a T5A image (static PIE, see `t5_abi::t5a`) into memory: copies the
//! image, zero-fills bss, applies R_RISCV_RELATIVE relocations and computes
//! the PMP region.

use t5_abi::t5a;

use crate::arch::riscv32::pmp::{GRANULE, Region};
use crate::platform::{Current, Platform};

// Fields are only read through Debug when a load failure is reported.
#[allow(dead_code)]
#[derive(Debug)]
pub enum LoadError {
    BadHeader,
    AbiMismatch { major: u16, minor: u16 },
    Truncated,
    BadLayout,
    TooLarge { need: usize, have: usize },
    BadReloc { offset: u32 },
}

#[derive(Debug)]
pub struct Loaded {
    pub region: Region,
    pub entry: usize,
    pub stack_top: usize,
}

fn align_up(v: usize, a: usize) -> usize {
    v.div_ceil(a) * a
}

/// Loads `image` at `base`, which must be `GRANULE`-aligned and have
/// `capacity` writable bytes behind it.
///
/// # Safety
/// `[base, base + capacity)` must be memory the kernel owns and nothing else
/// uses.
pub unsafe fn load(image: &[u8], base: usize, capacity: usize) -> Result<Loaded, LoadError> {
    let h = t5a::Header::from_bytes(image).ok_or(LoadError::BadHeader)?;
    if h.abi_major != t5_abi::ABI_MAJOR || h.abi_minor > t5_abi::ABI_MINOR {
        return Err(LoadError::AbiMismatch { major: h.abi_major, minor: h.abi_minor });
    }

    let header_size = h.header_size as usize;
    let image_size = h.image_size as usize;
    let text_size = h.text_size as usize;
    let mem_size = h.mem_size as usize;
    let stack_size = h.stack_size as usize;
    let relocs_off = header_size + image_size;
    let relocs_len = h.reloc_count as usize * t5a::RELOC_SIZE;
    if header_size < t5a::HEADER_SIZE || image.len() < relocs_off + relocs_len {
        return Err(LoadError::Truncated);
    }
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
        core::ptr::copy_nonoverlapping(image[header_size..].as_ptr(), dst, image_size);
        core::ptr::write_bytes(dst.add(image_size), 0, end_off - image_size);

        for i in 0..h.reloc_count as usize {
            let off = relocs_off + i * t5a::RELOC_SIZE;
            let r = t5a::Reloc::from_bytes(&image[off..off + t5a::RELOC_SIZE]);
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
    })
}
