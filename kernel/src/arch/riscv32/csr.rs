//! Minimal CSR access. Hand-written instead of using the `riscv` crate so that
//! ESP32-P4 custom CSRs can be added the same way later.

macro_rules! read_csr {
    ($name:literal) => {{
        let v: usize;
        unsafe { core::arch::asm!(concat!("csrr {0}, ", $name), out(reg) v, options(nomem, nostack)) };
        v
    }};
}

macro_rules! write_csr {
    ($name:literal, $v:expr) => {{
        let v: usize = $v;
        unsafe { core::arch::asm!(concat!("csrw ", $name, ", {0}"), in(reg) v, options(nomem, nostack)) };
    }};
}

pub(crate) use {read_csr, write_csr};

pub mod mstatus {
    pub const MPIE: usize = 1 << 7;
    pub const MPP_MASK: usize = 0b11 << 11;
    pub const FS_MASK: usize = 0b11 << 13;
    pub const FS_OFF: usize = 0b00 << 13;
    pub const FS_INITIAL: usize = 0b01 << 13;
    pub const FS_CLEAN: usize = 0b10 << 13;
    pub const FS_DIRTY: usize = 0b11 << 13;
    pub const MPRV: usize = 1 << 17;
}

/// Machine interrupt-enable bits (standard `mie`; unused in CLIC mode).
#[allow(dead_code)]
pub mod mie {
    pub const MTIE: usize = 1 << 7;
}

/// Exception codes (mcause with the interrupt bit clear).
pub mod cause {
    pub const INSN_MISALIGNED: usize = 0;
    pub const INSN_ACCESS: usize = 1;
    pub const ILLEGAL_INSN: usize = 2;
    pub const BREAKPOINT: usize = 3;
    pub const LOAD_MISALIGNED: usize = 4;
    pub const LOAD_ACCESS: usize = 5;
    pub const STORE_MISALIGNED: usize = 6;
    pub const STORE_ACCESS: usize = 7;
    pub const ECALL_U: usize = 8;
    pub const ECALL_M: usize = 11;

    pub const INTERRUPT: usize = 1 << 31;
    /// In CLIC mode (ESP32-P4) mcause also carries mpil/minhv/mpp/mpie in
    /// the upper bits, so the code must be masked out rather than compared
    /// whole (doc/01 §4.1).
    pub const CODE_MASK: usize = 0xFFF;

    pub fn name(code: usize) -> &'static str {
        match code {
            INSN_MISALIGNED => "instruction address misaligned",
            INSN_ACCESS => "instruction access fault",
            ILLEGAL_INSN => "illegal instruction",
            BREAKPOINT => "breakpoint",
            LOAD_MISALIGNED => "load address misaligned",
            LOAD_ACCESS => "load access fault",
            STORE_MISALIGNED => "store address misaligned",
            STORE_ACCESS => "store access fault",
            ECALL_U => "ecall from U-mode",
            ECALL_M => "ecall from M-mode",
            _ => "unknown",
        }
    }
}
