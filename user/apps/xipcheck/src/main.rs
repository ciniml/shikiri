#![no_std]
#![no_main]

//! M4b test app (doc/15 §5), linked with t5app-xip.ld: its text runs from
//! flash and its data from PSRAM. Sums a 128 KiB table in rodata (read in
//! place from flash, never copied) and checks where its code and data are.
//! Exit 0 if all holds.

use t5_rt::{Addr, event};

t5_rt::entry!(main);

const WORDS: usize = 32 * 1024;

const fn mix(i: u32) -> u32 {
    let x = i.wrapping_mul(0x9E37_79B9);
    x ^ (x >> 16)
}

static TABLE: [u32; WORDS] = {
    let mut t = [0u32; WORDS];
    let mut i = 0;
    while i < WORDS {
        t[i] = mix(i as u32);
        i += 1;
    }
    t
};

const fn expected_sum() -> u32 {
    let mut s = 0u32;
    let mut i = 0;
    while i < WORDS {
        s = s.wrapping_add(mix(i as u32));
        i += 1;
    }
    s
}

/// Written at run time, so it lives in .data (PSRAM).
static mut COUNTER: u32 = 7;

fn main(_: usize, _: usize) -> usize {
    let pc = main as *const () as usize;
    let table = TABLE.as_ptr() as usize;
    let data = core::ptr::addr_of!(COUNTER) as usize;
    let mut sum = 0u32;
    for w in TABLE.iter() {
        sum = sum.wrapping_add(unsafe { core::ptr::read_volatile(w) });
    }
    let counter = unsafe {
        let c = &mut *core::ptr::addr_of_mut!(COUNTER);
        *c += 1;
        *c
    };
    let in_flash = |a: usize| (0x4000_0000..0x4400_0000).contains(&a);
    let in_psram = |a: usize| (0x4800_0000..0x4C00_0000).contains(&a);
    let ok = sum == expected_sum() && counter == 8 && in_flash(pc) && in_flash(table) && in_psram(data) && data - pc >= 0x0800_0000;
    event!(
        Info,
        "xipcheck.done",
        ok = ok,
        pc = Addr(pc as u32),
        table = Addr(table as u32),
        data = Addr(data as u32),
        sum = sum
    );
    if ok { 0 } else { 1 }
}
