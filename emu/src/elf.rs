//! Minimal ELF32 little-endian reader: entry point and PT_LOAD segments.

pub struct Segment {
    pub vaddr: u32,
    pub data: Vec<u8>,
    pub memsz: u32,
}

pub struct Elf {
    pub entry: u32,
    pub segments: Vec<Segment>,
}

fn u16_at(b: &[u8], o: usize) -> Result<u16, String> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or_else(|| "truncated ELF".into())
}

fn u32_at(b: &[u8], o: usize) -> Result<u32, String> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap())).ok_or_else(|| "truncated ELF".into())
}

pub fn parse(b: &[u8]) -> Result<Elf, String> {
    if b.get(0..4) != Some(b"\x7fELF") || b.get(4) != Some(&1) || b.get(5) != Some(&1) {
        return Err("not a 32-bit little-endian ELF".into());
    }
    if u16_at(b, 0x12)? != 243 {
        return Err("not a RISC-V ELF".into());
    }
    let entry = u32_at(b, 0x18)?;
    let phoff = u32_at(b, 0x1C)? as usize;
    let phentsize = u16_at(b, 0x2A)? as usize;
    let phnum = u16_at(b, 0x2C)? as usize;
    let mut segments = Vec::new();
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        if u32_at(b, p)? != 1 {
            continue; // PT_LOAD only
        }
        let offset = u32_at(b, p + 4)? as usize;
        let vaddr = u32_at(b, p + 8)?;
        let filesz = u32_at(b, p + 16)? as usize;
        let memsz = u32_at(b, p + 20)?;
        let data = b.get(offset..offset + filesz).ok_or("segment outside the file")?.to_vec();
        segments.push(Segment { vaddr, data, memsz });
    }
    Ok(Elf { entry, segments })
}
