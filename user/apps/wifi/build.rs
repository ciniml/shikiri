// Copyright (c) 2026 Kenta IDA
// Distributed under the Boost Software License, Version 1.0.
// (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
// SPDX-License-Identifier: BSL-1.0

//! The access point to join, from a file outside the repository (doc/22
//! §6): `$T5_WIFI_FILE`, else `~/.config/tab5-os/wifi.txt`, with the SSID on
//! the first line and the passphrase on the second. Without it the service
//! starts the chip but does not connect. Nothing of it is printed.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=T5_WIFI_FILE");
    let path = std::env::var_os("T5_WIFI_FILE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/tab5-os/wifi.txt")));
    let (mut ssid, mut psk) = (String::new(), String::new());
    if let Some(p) = &path {
        println!("cargo:rerun-if-changed={}", p.display());
        if let Ok(text) = std::fs::read_to_string(p) {
            let mut lines = text.lines();
            ssid = lines.next().unwrap_or("").to_string();
            psk = lines.next().unwrap_or("").to_string();
        }
    }
    // The soft-AP's (doc/58 R4): `$T5_WIFI_AP_FILE`, else
    // `~/.config/tab5-os/wifi-ap.txt`, the same two lines.
    println!("cargo:rerun-if-env-changed=T5_WIFI_AP_FILE");
    let ap_path = std::env::var_os("T5_WIFI_AP_FILE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/tab5-os/wifi-ap.txt")));
    let (mut ap_ssid, mut ap_psk) = (String::new(), String::new());
    if let Some(p) = &ap_path {
        println!("cargo:rerun-if-changed={}", p.display());
        if let Ok(text) = std::fs::read_to_string(p) {
            let mut lines = text.lines();
            ap_ssid = lines.next().unwrap_or("").to_string();
            ap_psk = lines.next().unwrap_or("").to_string();
        }
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("credentials.rs");
    std::fs::write(
        out,
        format!(
            "const SSID: &[u8] = &{:?};\nconst PSK: &[u8] = &{:?};\nconst AP_SSID: &[u8] = &{:?};\nconst AP_PSK: &[u8] = &{:?};\n",
            ssid.as_bytes(),
            psk.as_bytes(),
            ap_ssid.as_bytes(),
            ap_psk.as_bytes()
        ),
    )
    .unwrap();
}
