//! elf2t5a: checks that an app ELF is a static PIE the kernel can load and
//! converts it into a signed T5A v1 image (`t5_abi::t5a`, doc/14 §1).
//!
//! Checks:
//! - ELF32, little-endian, RISC-V, ET_DYN, no PT_INTERP
//! - loadable segments start at 0; R-X/R segments come before RW segments and
//!   the RW part starts on a 128-byte boundary (ESP32-P4 PMP granularity)
//! - every dynamic relocation is R_RISCV_RELATIVE
//!
//! Usage:
//!   elf2t5a keygen --secret FILE --public FILE [--seed HEX]
//!   elf2t5a build IN.elf OUT.t5a --key SECRET [--name NAME] [--version X.Y.Z]
//!                 [--stack BYTES] [--permit NAME]... [--autostart]
//!                 [--secure-version N] [--abi MAJOR.MINOR]
//!   elf2t5a verify IMAGE.t5a --public FILE
//!
//! Keys are hex files: the secret file holds the 32-byte Ed25519 seed, the
//! public file the 32-byte public key. `--abi` overrides the ABI version and
//! exists for the kernel's rejection tests.

use std::{env, fs, process::ExitCode};

use ed25519_compact::{KeyPair, PublicKey, Seed, Signature};
use object::elf;
use object::read::elf::{FileHeader, ProgramHeader, Rela as _, SectionHeader};
use object::Endianness;
use sha2::{Digest, Sha256};
use t5_abi::t5a::{self, tlv};

const PMP_GRANULE: u32 = 128;
const DEFAULT_STACK: u32 = 4096;
const USAGE: &str = "usage:
  elf2t5a keygen --secret FILE --public FILE [--seed HEX]
  elf2t5a build IN.elf OUT.t5a --key SECRET [--name NAME] [--version X.Y.Z] [--stack BYTES]
                [--permit NAME]... [--autostart] [--secure-version N] [--abi MAJOR.MINOR]
  elf2t5a verify IMAGE.t5a --public FILE";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("keygen") => keygen(&args[1..]),
        Some("build") => build(&args[1..]),
        Some("verify") => verify(&args[1..]),
        _ => Err(USAGE.to_string()),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("elf2t5a: error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn hex32(s: &str) -> Result<[u8; 32], String> {
    let s = s.trim();
    if s.len() != 64 {
        return Err("expected 64 hex digits".into());
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| "bad hex")?;
    }
    Ok(out)
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn read_hex32(path: &str) -> Result<[u8; 32], String> {
    let s = fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    hex32(&s).map_err(|e| format!("{path}: {e}"))
}

fn keygen(args: &[String]) -> Result<(), String> {
    let (mut secret, mut public, mut seed) = (None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--secret" => secret = Some(val()?),
            "--public" => public = Some(val()?),
            "--seed" => seed = Some(hex32(&val()?)?),
            _ => return Err(USAGE.into()),
        }
    }
    let (secret, public) = secret.zip(public).ok_or(USAGE)?;
    let seed = match seed {
        Some(s) => s,
        None => {
            let mut s = [0u8; 32];
            let mut f = fs::File::open("/dev/urandom").map_err(|e| e.to_string())?;
            std::io::Read::read_exact(&mut f, &mut s).map_err(|e| e.to_string())?;
            s
        }
    };
    let kp = KeyPair::from_seed(Seed::new(seed));
    if let Some(dir) = std::path::Path::new(&secret).parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    write_private(&secret, &format!("{}\n", to_hex(&seed)))?;
    fs::write(&public, format!("{}\n", to_hex(kp.pk.as_ref()))).map_err(|e| format!("{public}: {e}"))?;
    eprintln!("elf2t5a: key {} -> {secret} (secret), {public} (public)", to_hex(&t5a::key_id(&kp.pk)));
    Ok(())
}

#[cfg(unix)]
fn write_private(path: &str, contents: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{path}: {e} (refusing to overwrite an existing key)"))?;
    f.write_all(contents.as_bytes()).map_err(|e| e.to_string())
}

struct BuildOpts {
    name: Option<String>,
    version: [u32; 3],
    stack: u32,
    permits: Vec<String>,
    autostart: bool,
    secure_version: u32,
    abi: (u16, u16),
}

fn build(args: &[String]) -> Result<(), String> {
    let (mut input, mut output, mut key) = (None, None, None);
    let mut o = BuildOpts {
        name: None,
        version: [0, 1, 0],
        stack: DEFAULT_STACK,
        permits: Vec::new(),
        autostart: false,
        secure_version: 0,
        abi: (t5_abi::ABI_MAJOR, t5_abi::ABI_MINOR),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--key" => key = Some(val()?),
            "--name" => o.name = Some(val()?),
            "--version" => {
                let v = val()?;
                let p: Vec<u32> = v.split('.').map(|x| x.parse().map_err(|_| format!("bad version {v}"))).collect::<Result<_, _>>()?;
                o.version = p.try_into().map_err(|_| format!("version must be X.Y.Z: {v}"))?;
            }
            "--stack" => o.stack = parse_u32(&val()?)?,
            "--permit" => o.permits.push(val()?),
            "--autostart" => o.autostart = true,
            "--secure-version" => o.secure_version = parse_u32(&val()?)?,
            "--abi" => {
                let v = val()?;
                let (a, b) = v.split_once('.').ok_or(format!("--abi must be MAJOR.MINOR: {v}"))?;
                o.abi = (a.parse().map_err(|_| "bad --abi")?, b.parse().map_err(|_| "bad --abi")?);
            }
            s if s.starts_with("--") => return Err(format!("unknown option {s}\n{USAGE}")),
            s if input.is_none() => input = Some(s.to_string()),
            s if output.is_none() => output = Some(s.to_string()),
            s => return Err(format!("unexpected argument {s}")),
        }
    }
    let (input, output) = input.zip(output).ok_or(USAGE)?;
    let key = key.ok_or("--key SECRET is required: images are always signed")?;
    if o.stack % 16 != 0 {
        return Err("stack size must be a multiple of 16".into());
    }
    let name = o.name.clone().unwrap_or_else(|| {
        std::path::Path::new(&input).file_stem().map_or("app".into(), |s| s.to_string_lossy().into_owned())
    });

    let data = fs::read(&input).map_err(|e| format!("{input}: {e}"))?;
    let app = convert(&data, o.stack)?;
    let kp = KeyPair::from_seed(Seed::new(read_hex32(&key)?));
    let image = assemble(&app, &name, &o, &Sha256::digest(&data), &kp)?;
    fs::write(&output, &image).map_err(|e| format!("{output}: {e}"))?;
    eprintln!(
        "elf2t5a: {name} {}.{}.{}: {}text {:#x}, mem {:#x}, stack {:#x}, {} relocs, {} bytes, key {}",
        o.version[0],
        o.version[1],
        o.version[2],
        if app.xip { "XIP, " } else { "" },
        app.text_size,
        app.mem_size,
        app.stack_size,
        app.relocs.len(),
        image.len(),
        to_hex(&t5a::key_id(&kp.pk))
    );
    Ok(())
}

/// Header + TLVs + image + relocations, signed.
fn assemble(app: &App, name: &str, o: &BuildOpts, elf_sha256: &[u8], kp: &KeyPair) -> Result<Vec<u8>, String> {
    let mut tlvs = vec![0u8; 1024];
    let mut at = 0;
    let mut push = |t: u16, v: &[u8]| -> Result<(), String> {
        at = t5a::push_tlv(&mut tlvs, at, t, v).ok_or("TLV area too small")?;
        Ok(())
    };
    push(tlv::NAME, name.as_bytes())?;
    let ver: Vec<u8> = o.version.iter().flat_map(|v| v.to_le_bytes()).collect();
    push(tlv::VERSION, &ver)?;
    let mut abi = o.abi.0.to_le_bytes().to_vec();
    abi.extend_from_slice(&o.abi.1.to_le_bytes());
    push(tlv::ABI_VERSION, &abi)?;
    push(tlv::BUILD_ID, &elf_sha256[..t5a::BUILD_ID_SIZE])?;
    if !o.permits.is_empty() {
        push(tlv::PERMISSIONS, o.permits.join("\0").as_bytes())?;
    }
    if o.autostart {
        push(tlv::FLAGS, &tlv::FLAG_AUTOSTART.to_le_bytes())?;
    }
    if o.secure_version != 0 {
        push(tlv::SECURE_VERSION, &o.secure_version.to_le_bytes())?;
    }
    if app.xip {
        push(tlv::XIP_DATA, &t5a::XIP_DELTA.to_le_bytes())?;
        // The image must start on a PMP granule: its text is used in place.
        let mut pad = (t5a::XIP_IMAGE_ALIGN - (t5a::HEADER_SIZE + at) % t5a::XIP_IMAGE_ALIGN) % t5a::XIP_IMAGE_ALIGN;
        if pad != 0 && pad < 4 {
            pad += t5a::XIP_IMAGE_ALIGN;
        }
        if pad != 0 {
            at = t5a::push_tlv(&mut tlvs, at, tlv::PAD, &vec![0u8; pad - 4]).ok_or("TLV area too small")?;
        }
    }
    tlvs.truncate(at);

    let mut header = t5a::Header {
        header_version: t5a::HEADER_VERSION,
        header_size: t5a::HEADER_SIZE as u16,
        tlv_size: tlvs.len() as u32,
        image_size: app.image.len() as u32,
        text_size: app.text_size,
        mem_size: app.mem_size,
        stack_size: app.stack_size,
        entry: app.entry,
        reloc_count: app.relocs.len() as u32,
        signed_size: 0,
        total_size: 0,
    };
    let signed = t5a::HEADER_SIZE + tlvs.len() + app.image.len() + app.relocs.len() * t5a::RELOC_SIZE;
    header.signed_size = signed as u32;
    header.total_size = (signed + t5a::FOOTER_SIZE) as u32;

    let mut out = header.to_bytes().to_vec();
    out.extend_from_slice(&tlvs);
    out.extend_from_slice(&app.image);
    for r in &app.relocs {
        out.extend_from_slice(&r.to_bytes());
    }
    let sig = kp.sk.sign(&out, None);
    let mut footer = vec![0u8; t5a::FOOTER_SIZE];
    let mut value = t5a::key_id(&kp.pk).to_vec();
    value.extend_from_slice(sig.as_ref());
    t5a::push_tlv(&mut footer, 0, tlv::SIGNATURE, &value).ok_or("footer")?;
    out.extend_from_slice(&footer);
    Ok(out)
}

fn verify(args: &[String]) -> Result<(), String> {
    let (mut image, mut public) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--public" => public = Some(it.next().cloned().ok_or("--public needs a value")?),
            s => image = Some(s.to_string()),
        }
    }
    let (image, public) = image.zip(public).ok_or(USAGE)?;
    let pk_bytes = read_hex32(&public)?;
    let data = fs::read(&image).map_err(|e| format!("{image}: {e}"))?;
    let s = t5a::split_signed(&data).map_err(|e| format!("{e:?}"))?;
    if s.key_id != t5a::key_id(&pk_bytes) {
        return Err(format!("signed with key {}, not {}", to_hex(&s.key_id), to_hex(&t5a::key_id(&pk_bytes))));
    }
    PublicKey::new(pk_bytes)
        .verify(s.message, &Signature::new(s.signature))
        .map_err(|_| "bad signature")?;
    let img = t5a::parse(s.message, t5_abi::ABI_MAJOR, t5_abi::ABI_MINOR).map_err(|e| format!("{e:?}"))?;
    let i = &img.info;
    println!(
        "{}: {} {}.{}.{} abi {}.{} build {} flags {:#x} text {:#x} mem {:#x}: signature OK",
        image,
        i.name,
        i.version[0],
        i.version[1],
        i.version[2],
        i.abi.0,
        i.abi.1,
        to_hex(&i.build_id),
        i.flags,
        img.header.text_size,
        img.header.mem_size
    );
    Ok(())
}

fn parse_u32(s: &str) -> Result<u32, String> {
    let r = match s.strip_prefix("0x") {
        Some(h) => u32::from_str_radix(h, 16),
        None => s.parse(),
    };
    r.map_err(|_| format!("bad number {s}"))
}

struct App {
    image: Vec<u8>,
    relocs: Vec<t5a::Reloc>,
    text_size: u32,
    mem_size: u32,
    stack_size: u32,
    entry: u32,
    /// Data linked at `t5a::XIP_DELTA` (doc/15).
    xip: bool,
}

fn convert(data: &[u8], stack_size: u32) -> Result<App, String> {
    let hdr = elf::FileHeader32::<Endianness>::parse(data).map_err(|e| e.to_string())?;
    let endian = hdr.endian().map_err(|e| e.to_string())?;
    if endian != Endianness::Little {
        return Err("not little-endian".into());
    }
    if hdr.e_machine(endian) != elf::EM_RISCV {
        return Err("not a RISC-V ELF".into());
    }
    if hdr.e_type(endian) != elf::ET_DYN {
        return Err("not a position-independent executable (ET_DYN); build with -C relocation-model=pie -C link-arg=-pie".into());
    }

    let phdrs = hdr.program_headers(endian, data).map_err(|e| e.to_string())?;
    let mut loads = Vec::new();
    for ph in phdrs {
        match ph.p_type(endian) {
            elf::PT_INTERP => return Err("has PT_INTERP; link with --no-dynamic-linker".into()),
            elf::PT_LOAD => loads.push(ph),
            _ => {}
        }
    }
    loads.sort_by_key(|ph| ph.p_vaddr(endian));
    let first = loads.first().ok_or("no PT_LOAD segments")?;
    if first.p_vaddr(endian) != 0 {
        return Err(format!("first segment is at {:#x}, expected 0", first.p_vaddr(endian)));
    }

    // Split into the read-only/executable part and the writable part.
    let mut data_start = None;
    let mut ro_file_end = 0u32;
    let mut ro_mem_end = 0u32;
    let mut file_end = 0u32;
    let mut mem_end = 0u32;
    for ph in &loads {
        let (va, filesz, memsz, flags) = (
            ph.p_vaddr(endian),
            ph.p_filesz(endian),
            ph.p_memsz(endian),
            ph.p_flags(endian),
        );
        let writable = flags & elf::PF_W != 0;
        if writable && flags & elf::PF_X != 0 {
            return Err(format!("segment at {va:#x} is both writable and executable"));
        }
        match (writable, data_start) {
            (true, None) => data_start = Some(va),
            (false, Some(_)) => {
                return Err(format!("read-only segment at {va:#x} follows a writable one"));
            }
            _ => {}
        }
        if !writable {
            ro_file_end = ro_file_end.max(va + filesz);
            ro_mem_end = ro_mem_end.max(va + memsz);
        }
        if filesz > 0 {
            file_end = file_end.max(va + filesz);
        }
        mem_end = mem_end.max(va + memsz);
    }
    // XIP (doc/15): the data is linked a fixed distance above the text.
    let xip = data_start.is_some_and(|d| d >= t5a::XIP_DELTA);
    if xip && data_start != Some(t5a::XIP_DELTA) {
        return Err(format!("XIP data must start at {:#x} (check t5app-xip.ld)", t5a::XIP_DELTA));
    }
    let (text_size, data_base) = if xip {
        (align_up(ro_mem_end, PMP_GRANULE), t5a::XIP_DELTA)
    } else {
        let t = data_start.unwrap_or(align_up(mem_end, PMP_GRANULE));
        (t, t)
    };
    if text_size % PMP_GRANULE != 0 {
        return Err(format!(
            "writable part starts at {text_size:#x}, not {PMP_GRANULE}-byte aligned (check t5app.ld)"
        ));
    }
    // From the data's link address: data + bss.
    let mem_size = if xip { align_up(mem_end - t5a::XIP_DELTA, 16) } else { align_up(mem_end, 16) };

    // Flatten the file-backed bytes: the text, then the data's initial bytes.
    let data_file_end = if xip { file_end.max(data_base) } else { file_end };
    let image_len = if xip { (text_size + (data_file_end - data_base)) as usize } else { file_end as usize };
    let mut image = vec![0u8; image_len];
    for ph in &loads {
        let bytes = ph.data(endian, data).map_err(|_| "segment data out of range")?;
        let va = ph.p_vaddr(endian);
        let at = if xip && va >= data_base { (text_size + va - data_base) as usize } else { va as usize };
        image[at..at + bytes.len()].copy_from_slice(bytes);
    }
    let _ = ro_file_end;

    // Collect dynamic relocations; only R_RISCV_RELATIVE is supported.
    let sections = hdr.sections(endian, data).map_err(|e| e.to_string())?;
    let mut relocs = Vec::new();
    for sec in sections.iter() {
        match sec.sh_type(endian) {
            elf::SHT_RELA => {}
            elf::SHT_REL => return Err("REL relocations are not supported".into()),
            _ => continue,
        }
        if sec.sh_flags(endian) & elf::SHF_ALLOC == 0 {
            continue; // static relocations kept by --emit-relocs, not ours
        }
        let (rels, _) = sec
            .rela(endian, data)
            .map_err(|e| e.to_string())?
            .ok_or("bad RELA section")?;
        for r in rels {
            let ty = r.r_type(endian);
            if ty != elf::R_RISCV_RELATIVE {
                return Err(format!(
                    "unsupported dynamic relocation type {ty} at {:#x}",
                    r.r_offset(endian)
                ));
            }
            let offset = r.r_offset(endian);
            let (lo, hi) = if xip { (data_base, data_base + mem_size) } else { (text_size, mem_size) };
            if offset % 4 != 0 || offset + 4 > hi {
                return Err(format!("relocation at {offset:#x} is misaligned or out of range"));
            }
            if offset < lo {
                return Err(format!("relocation at {offset:#x} targets the read-only part"));
            }
            relocs.push(t5a::Reloc { offset, addend: r.r_addend(endian) as u32 });
        }
    }

    Ok(App { image, relocs, text_size, mem_size, stack_size, entry: hdr.e_entry(endian), xip })
}

fn align_up(v: u32, a: u32) -> u32 {
    v.div_ceil(a) * a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Vec<u8>, KeyPair) {
        let kp = KeyPair::from_seed(Seed::new([7u8; 32]));
        let app = App {
            image: (0..300u32).map(|i| i as u8).collect(),
            relocs: vec![t5a::Reloc { offset: 0x80, addend: 0x10 }],
            text_size: 0x80,
            mem_size: 0x140,
            stack_size: 0x100,
            entry: 0,
            xip: false,
        };
        let o = BuildOpts {
            name: None,
            version: [1, 2, 3],
            stack: 0x100,
            permits: vec!["timer".into(), "i2c0".into()],
            autostart: true,
            secure_version: 5,
            abi: (t5_abi::ABI_MAJOR, t5_abi::ABI_MINOR),
        };
        (assemble(&app, "demo", &o, &[0xAB; 32], &kp).unwrap(), kp)
    }

    fn check(bytes: &[u8], pk: &PublicKey) -> Result<(), String> {
        let s = t5a::split_signed(bytes).map_err(|e| format!("{e:?}"))?;
        // As the kernel does: the key id selects the trusted key.
        let pk_bytes: [u8; 32] = pk.as_ref().try_into().unwrap();
        if s.key_id != t5a::key_id(&pk_bytes) {
            return Err("unknown key".into());
        }
        pk.verify(s.message, &Signature::new(s.signature)).map_err(|_| "bad signature".to_string())?;
        t5a::parse(s.message, t5_abi::ABI_MAJOR, t5_abi::ABI_MINOR).map(|_| ()).map_err(|e| format!("{e:?}"))
    }

    #[test]
    fn roundtrip_and_fields() {
        let (bytes, kp) = sample();
        check(&bytes, &kp.pk).unwrap();
        let s = t5a::split_signed(&bytes).unwrap();
        assert_eq!(s.key_id, t5a::key_id(&kp.pk));
        let img = t5a::parse(s.message, t5_abi::ABI_MAJOR, t5_abi::ABI_MINOR).unwrap();
        assert_eq!(img.info.name, "demo");
        assert_eq!(img.info.version, [1, 2, 3]);
        assert_eq!(img.info.flags, tlv::FLAG_AUTOSTART);
        assert_eq!(img.info.secure_version, 5);
        assert!(img.info.permits(b"timer") && img.info.permits(b"i2c0") && !img.info.permits(b"i2c"));
        assert_eq!(img.image.len(), 300);
        assert_eq!(img.relocs.len(), t5a::RELOC_SIZE);
    }

    #[test]
    fn any_flipped_bit_is_rejected() {
        let (bytes, kp) = sample();
        // Every byte of the signed part and of the signature matters. (The
        // footer's type/length bytes are checked structurally.)
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0x01;
            assert!(check(&b, &kp.pk).is_err(), "byte {i} flipped but accepted");
        }
    }

    #[test]
    fn other_key_and_truncation() {
        let (bytes, _) = sample();
        let other = KeyPair::from_seed(Seed::new([8u8; 32]));
        assert!(check(&bytes, &other.pk).is_err());
        assert!(t5a::split_signed(&bytes[..bytes.len() - 1]).is_err());
        assert!(t5a::split_signed(&bytes[..10]).is_err());
    }
}
