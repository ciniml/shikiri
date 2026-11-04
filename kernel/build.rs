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

    // Trusted app-signing keys (doc/14 §1.3): keys/*.pub, 64 hex digits each.
    // `app-*.pub` are app keys, the rest system keys (doc/52 §4).
    let mut keys = String::from("/// Public keys whose signatures the loader accepts, and whether each is a system key.\npub static TRUSTED_KEYS: &[([u8; 32], bool)] = &[\n");
    let mut found = 0;
    for entry in fs::read_dir("../keys").expect("../keys") {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "pub") {
            println!("cargo:rerun-if-changed={}", path.display());
            let hex = fs::read_to_string(&path).unwrap();
            let hex = hex.trim();
            assert_eq!(hex.len(), 64, "{}: expected 64 hex digits", path.display());
            let bytes: Vec<String> = (0..32).map(|i| format!("0x{}", &hex[i * 2..i * 2 + 2])).collect();
            let system = !path.file_name().unwrap().to_string_lossy().starts_with("app-");
            keys.push_str(&format!("    ([{}], {system}), // {}\n", bytes.join(", "), path.display()));
            found += 1;
        }
    }
    assert!(found > 0, "no keys/*.pub: run `make keys`");
    keys.push_str("];\n");
    println!("cargo:rerun-if-changed=../keys");
    fs::write(out.join("keys.rs"), keys).unwrap();
}
