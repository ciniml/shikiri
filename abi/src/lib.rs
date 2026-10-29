//! Definitions shared between the kernel, the user runtime (`t5-rt`) and host
//! tools. See doc/07-architecture-proposal.md §4 for the syscall ABI and
//! doc/04-dynamic-loading-and-abi.md for the T5A image format.

#![no_std]

/// ABI version carried by every app image. The kernel accepts an image when
/// the major numbers match and the image's minor is not newer than its own.
pub const ABI_MAJOR: u16 = 0;
/// Minor 2 (M2): yield, sleep, time.
pub const ABI_MINOR: u16 = 2;

/// Syscall numbers (passed in a7). Numbers are only ever added, never reused.
pub mod sys {
    /// a0 = exit code. Does not return.
    pub const EXIT: usize = 0x000;
    /// Gives up the rest of the time slice.
    pub const YIELD: usize = 0x001;
    /// a0/a1 = nanoseconds (lo/hi). Sleeps at least that long.
    pub const SLEEP: usize = 0x002;
    pub const SYS_INFO: usize = 0x003;
    /// Returns monotonic nanoseconds since boot in a1/a2 (lo/hi).
    pub const TIME: usize = 0x004;
    pub const DBG_WRITE: usize = 0xF00;
}

/// Error codes. A syscall returns `-(code as isize)` in a0 on failure.
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Fail = 1,
    Busy = 2,
    Already = 3,
    Off = 4,
    Reserve = 5,
    Invalid = 6,
    Size = 7,
    Cancel = 8,
    NoMem = 9,
    NoSupport = 10,
    NoDevice = 11,
    Uninstalled = 12,
    NoAck = 13,
    /// A user pointer was outside the calling process's memory.
    BadAddress = 14,
}

impl Error {
    pub fn from_code(code: usize) -> Error {
        match code {
            2 => Error::Busy,
            3 => Error::Already,
            4 => Error::Off,
            5 => Error::Reserve,
            6 => Error::Invalid,
            7 => Error::Size,
            8 => Error::Cancel,
            9 => Error::NoMem,
            10 => Error::NoSupport,
            11 => Error::NoDevice,
            12 => Error::Uninstalled,
            13 => Error::NoAck,
            14 => Error::BadAddress,
            _ => Error::Fail,
        }
    }
}

/// Exit code a process reports when it panics.
pub const EXIT_PANIC: usize = 0xDEAD;

/// T5A image format, version 0 (no TLVs or signature yet; those arrive in M3).
///
/// Layout of an image file:
///
/// ```text
/// +----------------+  offset 0
/// | T5aHeader      |
/// +----------------+  header_size
/// | image bytes    |  image_size bytes, loaded at the process base address
/// +----------------+
/// | Reloc[]        |  reloc_count entries
/// +----------------+
/// ```
///
/// The image is linked as a static PIE at address 0. Memory layout once loaded:
/// `[0, text_size)` is text + rodata (R-X), `[text_size, mem_size)` is
/// data + bss (RW-), followed by `stack_size` bytes of stack (RW-).
pub mod t5a {
    pub const MAGIC: [u8; 4] = *b"T5A0";

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct Header {
        pub magic: [u8; 4],
        pub header_size: u32,
        pub abi_major: u16,
        pub abi_minor: u16,
        /// Bytes of image data that follow the header.
        pub image_size: u32,
        /// End of the R-X part; the RW part starts here.
        pub text_size: u32,
        /// End of data + bss (bss is zero-filled by the kernel).
        pub mem_size: u32,
        pub stack_size: u32,
        /// Entry point, as an offset from the base address.
        pub entry: u32,
        pub reloc_count: u32,
    }

    pub const HEADER_SIZE: usize = core::mem::size_of::<Header>();

    /// One `R_RISCV_RELATIVE` relocation: `*(base + offset) = base + addend`.
    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct Reloc {
        pub offset: u32,
        pub addend: u32,
    }

    pub const RELOC_SIZE: usize = core::mem::size_of::<Reloc>();

    impl Header {
        pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
            let mut out = [0u8; HEADER_SIZE];
            out[0..4].copy_from_slice(&self.magic);
            out[4..8].copy_from_slice(&self.header_size.to_le_bytes());
            out[8..10].copy_from_slice(&self.abi_major.to_le_bytes());
            out[10..12].copy_from_slice(&self.abi_minor.to_le_bytes());
            let words = [
                self.image_size,
                self.text_size,
                self.mem_size,
                self.stack_size,
                self.entry,
                self.reloc_count,
            ];
            for (i, w) in words.iter().enumerate() {
                out[12 + i * 4..16 + i * 4].copy_from_slice(&w.to_le_bytes());
            }
            out
        }

        pub fn from_bytes(b: &[u8]) -> Option<Header> {
            if b.len() < HEADER_SIZE {
                return None;
            }
            let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
            let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
            let h = Header {
                magic: [b[0], b[1], b[2], b[3]],
                header_size: u32_at(4),
                abi_major: u16_at(8),
                abi_minor: u16_at(10),
                image_size: u32_at(12),
                text_size: u32_at(16),
                mem_size: u32_at(20),
                stack_size: u32_at(24),
                entry: u32_at(28),
                reloc_count: u32_at(32),
            };
            if h.magic != MAGIC {
                return None;
            }
            Some(h)
        }
    }

    impl Reloc {
        pub fn from_bytes(b: &[u8]) -> Reloc {
            Reloc {
                offset: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                addend: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            }
        }

        pub fn to_bytes(&self) -> [u8; RELOC_SIZE] {
            let mut out = [0u8; RELOC_SIZE];
            out[0..4].copy_from_slice(&self.offset.to_le_bytes());
            out[4..8].copy_from_slice(&self.addend.to_le_bytes());
            out
        }
    }
}
