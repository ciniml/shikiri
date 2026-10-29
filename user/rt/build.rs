use std::{env, fs, path::PathBuf};

fn main() {
    // Put the app linker script where the linker can find it (-Tt5app.ld).
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::copy("t5app.ld", out.join("t5app.ld")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=t5app.ld");
}
