// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

use std::{env, fs, path::PathBuf};

fn main() {
    // Put the app linker script where the linker can find it (-Tt5app.ld).
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    // t5app-xip.ld is the XIP variant (doc/15), picked by RUSTFLAGS.
    for script in ["t5app.ld", "t5app-xip.ld"] {
        fs::copy(script, out.join(script)).unwrap();
        println!("cargo:rerun-if-changed={script}");
    }
    println!("cargo:rustc-link-search={}", out.display());
}
