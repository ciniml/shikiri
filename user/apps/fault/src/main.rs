//! Memory-protection self-test. The kernel starts this app several times with
//! `mode` in a0 and a target address in a1; every mode except 0 must be killed
//! by an access fault before it reaches `exit`.

#![no_std]
#![no_main]

use t5_rt::{Addr, event};

t5_rt::entry!(main);

/// `ret` (0x8082, c.jr ra) placed in the RW data region. Executing it must
/// fault because data is mapped RW- (W^X).
#[unsafe(no_mangle)]
static mut DATA_CODE: [u16; 2] = [0x8082, 0x0001];

pub const MODE_OK: usize = 0;
pub const MODE_LOAD: usize = 1;
pub const MODE_STORE: usize = 2;
pub const MODE_EXEC_DATA: usize = 3;
pub const MODE_STORE_TEXT: usize = 4;
/// Malformed `log_emit` payloads must be rejected with an error (exit 0 if
/// all are).
pub const MODE_BAD_LOG: usize = 5;

fn main(mode: usize, addr: usize) -> usize {
    event!(Info, "fault.start", mode = mode, addr = Addr(addr as u32));
    unsafe {
        match mode {
            MODE_LOAD => {
                let v = core::ptr::read_volatile(addr as *const u32);
                event!(Warn, "fault.load_succeeded", addr = Addr(addr as u32), value = v);
            }
            MODE_STORE => core::ptr::write_volatile(addr as *mut u32, 0xBAD),
            MODE_EXEC_DATA => {
                let f: extern "C" fn() =
                    core::mem::transmute(core::ptr::addr_of_mut!(DATA_CODE) as *const ());
                f();
            }
            MODE_STORE_TEXT => {
                let p = main as *const () as *mut u32;
                core::ptr::write_volatile(p, 0);
            }
            MODE_BAD_LOG => return bad_log(addr),
            _ => {}
        }
    }
    if mode == MODE_OK { 0 } else { 1 }
}

/// `kernel_addr` is memory the process may not read.
fn bad_log(kernel_addr: usize) -> usize {
    use t5_rt::{Error, log, sys};
    let emit = |ptr: usize, len: usize| unsafe { t5_rt::raw_syscall(sys::LOG_EMIT, ptr, len).0 };
    let err = |e: Error| -(e as isize);
    // Header says 3 fields, none follow.
    let short = [(2 << 5) | 3u8, 1, 2, 3, 4];
    // A field tag that does not exist.
    let bad_tag = [(2 << 5) | 1u8, 1, 2, 3, 4, 0x7F, 0];
    let big = [0u8; log::MAX_PAYLOAD + 1];
    let results = [
        emit(short.as_ptr() as usize, short.len()) == err(Error::Invalid),
        emit(bad_tag.as_ptr() as usize, bad_tag.len()) == err(Error::Invalid),
        emit(big.as_ptr() as usize, big.len()) == err(Error::Size),
        emit(kernel_addr, 8) == err(Error::BadAddress),
    ];
    if results.iter().all(|&ok| ok) { 0 } else { 2 }
}
