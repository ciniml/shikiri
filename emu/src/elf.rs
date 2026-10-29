//! Minimal ELF32 little-endian reader: entry point and PT_LOAD segments.

pub struct Segment {
    pub vaddr: u32,
    pub data: Vec<u8>,
    pub memsz: u32,
}

pub struct Elf {
    pub entry: u32,
    pub segments: Vec<Segment>,
    /// Defined symbols from `.symtab`: (name, value).
    pub symbols: Vec<(String, u32)>,
}

impl Elf {
    pub fn symbol(&self, name: &str) -> Option<u32> {
        self.symbols.iter().find(|(n, _)| n == name).map(|&(_, v)| v)
    }
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
    Ok(Elf { entry, segments, symbols: symbols(b)? })
}

fn symbols(b: &[u8]) -> Result<Vec<(String, u32)>, String> {
    let shoff = u32_at(b, 0x20)? as usize;
    let shentsize = u16_at(b, 0x2E)? as usize;
    let shnum = u16_at(b, 0x30)? as usize;
    let mut out = Vec::new();
    for i in 0..shnum {
        let sh = shoff + i * shentsize;
        if u32_at(b, sh + 4)? != 2 {
            continue; // SHT_SYMTAB only
        }
        let (off, size) = (u32_at(b, sh + 16)? as usize, u32_at(b, sh + 20)? as usize);
        let strtab = shoff + u32_at(b, sh + 24)? as usize * shentsize;
        let str_off = u32_at(b, strtab + 16)? as usize;
        for s in (off..off + size).step_by(16) {
            let shndx = u16_at(b, s + 14)?;
            if shndx == 0 {
                continue; // undefined
            }
            let name_off = str_off + u32_at(b, s)? as usize;
            let end = b[name_off..].iter().position(|&c| c == 0).ok_or("bad symbol name")?;
            let name = String::from_utf8_lossy(&b[name_off..name_off + end]).into_owned();
            out.push((name, u32_at(b, s + 4)?));
        }
    }
    Ok(out)
}
