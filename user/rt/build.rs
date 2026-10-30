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
