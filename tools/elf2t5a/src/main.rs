//! elf2t5a: checks that an app ELF is a static PIE the kernel can load and
//! converts it into a T5A image (see `t5_abi::t5a`).
//!
//! Checks:
//! - ELF32, little-endian, RISC-V, ET_DYN, no PT_INTERP
//! - loadable segments start at 0; R-X/R segments come before RW segments and
//!   the RW part starts on a 128-byte boundary (ESP32-P4 PMP granularity)
//! - every dynamic relocation is R_RISCV_RELATIVE
//!
//! Usage: elf2t5a <input.elf> <output.t5a> [--stack <bytes>]

use std::{env, fs, process::ExitCode};

use object::elf;
use object::read::elf::{FileHeader, ProgramHeader, Rela as _, SectionHeader};
use object::Endianness;
use t5_abi::t5a;

const PMP_GRANULE: u32 = 128;
const DEFAULT_STACK: u32 = 4096;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("elf2t5a: error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    let (mut input, mut output, mut stack) = (None, None, DEFAULT_STACK);
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--stack" => {
                i += 1;
                stack = parse_u32(args.get(i).ok_or("--stack needs a value")?)?;
            }
            a if input.is_none() => input = Some(a.to_string()),
            a if output.is_none() => output = Some(a.to_string()),
            a => return Err(format!("unexpected argument {a}")),
        }
        i += 1;
    }
    let (input, output) = match (input, output) {
        (Some(i), Some(o)) => (i, o),
        _ => return Err("usage: elf2t5a <input.elf> <output.t5a> [--stack <bytes>]".into()),
    };
    if stack % 16 != 0 {
        return Err("stack size must be a multiple of 16".into());
    }

    let data = fs::read(&input).map_err(|e| format!("{input}: {e}"))?;
    let image = convert(&data, stack)?;
    fs::write(&output, &image).map_err(|e| format!("{output}: {e}"))?;
    Ok(())
}

fn parse_u32(s: &str) -> Result<u32, String> {
    let r = match s.strip_prefix("0x") {
        Some(h) => u32::from_str_radix(h, 16),
        None => s.parse(),
    };
    r.map_err(|_| format!("bad number {s}"))
}

fn convert(data: &[u8], stack_size: u32) -> Result<Vec<u8>, String> {
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
    let mut text_size = None;
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
        match (writable, text_size) {
            (true, None) => text_size = Some(va),
            (false, Some(_)) => {
                return Err(format!("read-only segment at {va:#x} follows a writable one"));
            }
            _ => {}
        }
        if filesz > 0 {
            file_end = file_end.max(va + filesz);
        }
        mem_end = mem_end.max(va + memsz);
    }
    let text_size = text_size.unwrap_or(align_up(mem_end, PMP_GRANULE));
    if text_size % PMP_GRANULE != 0 {
        return Err(format!(
            "writable part starts at {text_size:#x}, not {PMP_GRANULE}-byte aligned (check t5app.ld)"
        ));
    }
    let mem_size = align_up(mem_end, 16);

    // Flatten the file-backed bytes of every segment.
    let mut image = vec![0u8; file_end as usize];
    for ph in &loads {
        let bytes = ph.data(endian, data).map_err(|_| "segment data out of range")?;
        let va = ph.p_vaddr(endian) as usize;
        image[va..va + bytes.len()].copy_from_slice(bytes);
    }

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
            if offset % 4 != 0 || offset + 4 > mem_size {
                return Err(format!("relocation at {offset:#x} is misaligned or out of range"));
            }
            if offset < text_size {
                return Err(format!("relocation at {offset:#x} targets the read-only part"));
            }
            relocs.push(t5a::Reloc { offset, addend: r.r_addend(endian) as u32 });
        }
    }

    let header = t5a::Header {
        magic: t5a::MAGIC,
        header_size: t5a::HEADER_SIZE as u32,
        abi_major: t5_abi::ABI_MAJOR,
        abi_minor: t5_abi::ABI_MINOR,
        image_size: image.len() as u32,
        text_size,
        mem_size,
        stack_size,
        entry: hdr.e_entry(endian),
        reloc_count: relocs.len() as u32,
    };

    let mut out = header.to_bytes().to_vec();
    out.extend_from_slice(&image);
    for r in &relocs {
        out.extend_from_slice(&r.to_bytes());
    }
    eprintln!(
        "elf2t5a: text {:#x}, mem {:#x}, stack {:#x}, entry {:#x}, {} relocs, {} bytes",
        text_size,
        mem_size,
        stack_size,
        header.entry,
        relocs.len(),
        out.len()
    );
    Ok(out)
}

fn align_up(v: u32, a: u32) -> u32 {
    v.div_ceil(a) * a
}
