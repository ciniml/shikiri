//! Memory-protection self-test. The kernel starts this app several times with
//! `mode` in a0 and a target address in a1; every mode except 0 must be killed
//! by an access fault before it reaches `exit`.

#![no_std]
#![no_main]

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

fn main(mode: usize, addr: usize) -> usize {
    t5_rt::println!("fault test: mode={} addr={:#x}", mode, addr);
    unsafe {
        match mode {
            MODE_LOAD => {
                let v = core::ptr::read_volatile(addr as *const u32);
                t5_rt::println!("load succeeded: {:#x}", v);
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
            _ => {}
        }
    }
    if mode == MODE_OK { 0 } else { 1 }
}
