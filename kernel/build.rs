use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    let virt = env::var_os("CARGO_FEATURE_PLATFORM_VIRT").is_some();
    let p4 = env::var_os("CARGO_FEATURE_PLATFORM_ESP32P4V1").is_some();
    let script = match (virt, p4) {
        (true, false) => "link-virt.ld",
        (false, true) => "link-esp32p4v1.ld",
        _ => panic!("select exactly one of the features platform-virt / platform-esp32p4v1"),
    };
    fs::copy(script, out.join("link.ld")).unwrap();
    fs::copy("kernel-common.ld", out.join("kernel-common.ld")).unwrap();
    println!("cargo:rerun-if-changed=kernel-common.ld");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rustc-link-arg=-Tlink.ld");
    println!("cargo:rerun-if-changed={script}");

    // App images built by `make apps` (user/ + tools/elf2t5a) are embedded into
    // the kernel until app loading from flash arrives in M3.
    let apps = PathBuf::from(env::var("T5_APPS_DIR").unwrap_or_else(|_| "../build/apps".into()));
    println!("cargo:rerun-if-env-changed=T5_APPS_DIR");
    for app in ["hello", "fault", "ticker", "spin"] {
        let src = apps.join(format!("{app}.t5a"));
        println!("cargo:rerun-if-changed={}", src.display());
        fs::copy(&src, out.join(format!("{app}.t5a"))).unwrap_or_else(|e| {
            panic!("{}: {e} (run `make apps` first)", src.display())
        });
    }
}
