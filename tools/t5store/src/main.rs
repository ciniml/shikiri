//! t5store: builds the images of the `appmeta` and `apps` partitions from
//! signed T5A files, and lists them (doc/14 §2).
//!
//!   t5store build -o DIR APP.t5a...   -> DIR/appmeta.bin, DIR/apps.bin
//!   t5store list DIR
//!
//! `appmeta.bin` is the whole 64 KiB partition (catalog in sector A, seq 1;
//! the rest erased). `apps.bin` covers the extents in use. Write them at
//! 0x410000 and 0x420000. Images are placed as they are; the kernel checks
//! their signatures when it loads them.

use std::fs;
use std::process::ExitCode;

use t5_abi::store::{self, Entry, State};
use t5_abi::t5a;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("build") => build(&args[1..]),
        Some("list") => list(&args[1..]),
        _ => Err("usage: t5store build -o DIR APP.t5a... | t5store list DIR".into()),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("t5store: error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn build(args: &[String]) -> Result<(), String> {
    let mut out = None;
    let mut inputs = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = Some(it.next().ok_or("-o needs a directory")?.clone()),
            s => inputs.push(s.to_string()),
        }
    }
    let out = out.ok_or("missing -o DIR")?;
    let mut entries: Vec<Entry> = Vec::new();
    let mut apps = Vec::new();
    let mut next_extent = 0u32;
    for path in &inputs {
        let bytes = fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        // The signature is the kernel's to check; here only read the TLVs.
        let s = t5a::split_signed(&bytes).map_err(|e| format!("{path}: {e:?}"))?;
        let img = t5a::parse_any_abi(s.message).map_err(|e| format!("{path}: {e:?}"))?;
        let info = img.info;
        if entries.iter().any(|e| e.name() == info.name) {
            return Err(format!("{path}: an app named {} is already in the store", info.name));
        }
        let total = img.header.total_size;
        let count = total.div_ceil(store::EXTENT_SIZE);
        if next_extent + count > store::EXTENTS {
            return Err(format!("{path}: the apps partition is full"));
        }
        let mut name = [0u8; 32];
        name[..info.name.len()].copy_from_slice(info.name.as_bytes());
        entries.push(Entry {
            name,
            extent_first: next_extent as u16,
            extent_count: count as u16,
            length: total,
            version: info.version,
            secure_version: info.secure_version,
            state: State::Valid,
            flags: (info.flags & t5a::tlv::FLAG_AUTOSTART) as u8,
        });
        let start = (next_extent * store::EXTENT_SIZE) as usize;
        apps.resize(start + (count * store::EXTENT_SIZE) as usize, 0xFF);
        apps[start..start + total as usize].copy_from_slice(&bytes[..total as usize]);
        next_extent += count;
    }

    let mut sector = [0u8; store::SECTOR_SIZE];
    if !store::build_sector(1, entries.iter().copied(), &mut sector) {
        return Err(format!("too many apps (max {})", store::MAX_ENTRIES));
    }
    let mut appmeta = vec![0xFF; store::APPMETA_SIZE as usize];
    appmeta[..store::SECTOR_SIZE].copy_from_slice(&sector);
    fs::create_dir_all(&out).map_err(|e| format!("{out}: {e}"))?;
    fs::write(format!("{out}/appmeta.bin"), &appmeta).map_err(|e| e.to_string())?;
    fs::write(format!("{out}/apps.bin"), &apps).map_err(|e| e.to_string())?;
    eprintln!(
        "t5store: {} app(s), {} extent(s) -> {out}/appmeta.bin @{:#x}, {out}/apps.bin @{:#x}",
        entries.len(),
        next_extent,
        store::APPMETA_OFFSET,
        store::APPS_OFFSET
    );
    print_entries(&entries);
    Ok(())
}

fn list(args: &[String]) -> Result<(), String> {
    let dir = args.first().ok_or("usage: t5store list DIR")?;
    let meta = fs::read(format!("{dir}/appmeta.bin")).map_err(|e| format!("{dir}/appmeta.bin: {e}"))?;
    let (a, b) = (&meta[..store::SECTOR_SIZE], &meta[store::SECTOR_SIZE..2 * store::SECTOR_SIZE]);
    let cat = store::active(a, b).ok_or("no valid catalog")?;
    println!("catalog seq {} ({} entries)", cat.seq, cat.count);
    print_entries(&cat.entries().collect::<Vec<_>>());
    Ok(())
}

fn print_entries(entries: &[Entry]) {
    for e in entries {
        println!(
            "  {:<20} {}.{}.{}  extents {}+{}  {} bytes  {:?}{}",
            e.name(),
            e.version[0],
            e.version[1],
            e.version[2],
            e.extent_first,
            e.extent_count,
            e.length,
            e.state,
            if e.flags & 1 != 0 { " autostart" } else { "" }
        );
    }
}
