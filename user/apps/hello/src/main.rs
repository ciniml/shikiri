#![no_std]
#![no_main]

t5_rt::entry!(main);

static GREETING: &str = "hello from U-mode";

fn main(_arg0: usize, _arg1: usize) -> usize {
    // GREETING is reached through a relocated pointer, so this line also
    // checks that the kernel applied the R_RISCV_RELATIVE relocations.
    t5_rt::println!("{}", GREETING);
    0
}
