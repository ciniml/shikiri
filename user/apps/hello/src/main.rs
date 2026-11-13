#![no_std]
// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

#![no_main]

t5_rt::entry!(main);

static GREETING: &str = "hello from U-mode";

/// Ends with `arg0` as its code (init's test list: one that fails, doc/56
/// §3).
fn main(arg0: usize, _arg1: usize) -> usize {
    // GREETING is reached through a relocated pointer, so this line also
    // checks that the kernel applied the R_RISCV_RELATIVE relocations.
    t5_rt::event!(Info, "hello.greet", text = GREETING);
    arg0
}
