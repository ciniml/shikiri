//! Expands RV32C (with the F loads/stores) into the equivalent 32-bit
//! instruction, so the executor only has to decode one format. Encodings are
//! from the RISC-V Unprivileged ISA, chapter "C" (Compressed) tables 16.5-16.7.
//! D-precision forms and the RV64-only forms are illegal on the ESP32-P4
//! (RV32IMAFC, doc/01 §1.1).

/// Returns `None` for an illegal compressed instruction.
pub fn expand(c: u32) -> Option<u32> {
    let f3 = (c >> 13) & 7;
    let rd = (c >> 7) & 0x1f;
    let rs2 = (c >> 2) & 0x1f;
    let rdp = ((c >> 2) & 7) + 8;
    let rs1p = ((c >> 7) & 7) + 8;
    let imm6 = sext(((c >> 7) & 0x20) | ((c >> 2) & 0x1f), 6);
    match c & 3 {
        0 => match f3 {
            0 => {
                let imm = ((c >> 7) & 0x30) | ((c >> 1) & 0x3c0) | ((c >> 4) & 0x4) | ((c >> 2) & 0x8);
                (imm != 0).then(|| i(OP_IMM, rdp, 0, 2, imm))
            }
            2 => Some(i(LOAD, rdp, 2, rs1p, cl_imm(c))),
            3 => Some(i(LOAD_FP, rdp, 2, rs1p, cl_imm(c))),
            6 => Some(s(STORE, 2, rs1p, rdp, cl_imm(c))),
            7 => Some(s(STORE_FP, 2, rs1p, rdp, cl_imm(c))),
            _ => None,
        },
        1 => match f3 {
            0 => Some(i(OP_IMM, rd, 0, rd, imm6)),
            1 => Some(j(1, cj_imm(c))),
            2 => Some(i(OP_IMM, rd, 0, 0, imm6)),
            3 if rd == 2 => {
                let imm = ((c >> 3) & 0x200) | ((c >> 2) & 0x10) | ((c << 1) & 0x40) | ((c << 4) & 0x180) | ((c << 3) & 0x20);
                (imm != 0).then(|| i(OP_IMM, 2, 0, 2, sext(imm, 10)))
            }
            3 => {
                let imm = sext(((c << 5) & 0x20000) | ((c << 10) & 0x1f000), 18);
                (imm != 0).then(|| u(LUI, rd, imm))
            }
            4 => {
                let shamt = ((c >> 7) & 0x20) | ((c >> 2) & 0x1f);
                match (c >> 10) & 3 {
                    0 if shamt < 32 => Some(i(OP_IMM, rs1p, 5, rs1p, shamt)),
                    1 if shamt < 32 => Some(i(OP_IMM, rs1p, 5, rs1p, shamt | 0x400)),
                    2 => Some(i(OP_IMM, rs1p, 7, rs1p, imm6)),
                    3 if c & (1 << 12) == 0 => {
                        let (f3, f7) = match (c >> 5) & 3 {
                            0 => (0, 0x20),
                            1 => (4, 0),
                            2 => (6, 0),
                            _ => (7, 0),
                        };
                        Some(r(OP, rs1p, f3, rs1p, rdp, f7))
                    }
                    _ => None,
                }
            }
            5 => Some(j(0, cj_imm(c))),
            6 => Some(b(0, rs1p, 0, cb_imm(c))),
            _ => Some(b(1, rs1p, 0, cb_imm(c))),
        },
        2 => match f3 {
            0 => {
                let shamt = ((c >> 7) & 0x20) | ((c >> 2) & 0x1f);
                (shamt < 32).then(|| i(OP_IMM, rd, 1, rd, shamt))
            }
            2 => (rd != 0).then(|| i(LOAD, rd, 2, 2, ci_lw_imm(c))),
            3 => Some(i(LOAD_FP, rd, 2, 2, ci_lw_imm(c))),
            4 => {
                let bit12 = c & (1 << 12) != 0;
                match (bit12, rd, rs2) {
                    (false, 0, 0) => None,
                    (false, _, 0) => Some(i(JALR, 0, 0, rd, 0)),
                    (false, _, _) => Some(r(OP, rd, 0, 0, rs2, 0)),
                    (true, 0, 0) => Some(0x0010_0073), // ebreak
                    (true, _, 0) => Some(i(JALR, 1, 0, rd, 0)),
                    (true, _, _) => Some(r(OP, rd, 0, rd, rs2, 0)),
                }
            }
            6 => Some(s(STORE, 2, 2, rs2, css_imm(c))),
            7 => Some(s(STORE_FP, 2, 2, rs2, css_imm(c))),
            _ => None,
        },
        _ => None,
    }
}

const LOAD: u32 = 0x03;
const LOAD_FP: u32 = 0x07;
const OP_IMM: u32 = 0x13;
const STORE: u32 = 0x23;
const STORE_FP: u32 = 0x27;
const OP: u32 = 0x33;
const LUI: u32 = 0x37;
const JALR: u32 = 0x67;

fn sext(v: u32, bits: u32) -> u32 {
    let sh = 32 - bits;
    (((v << sh) as i32) >> sh) as u32
}

/// c.lw / c.flw / c.sw / c.fsw: uimm[5:3] = c[12:10], uimm[2] = c[6], uimm[6] = c[5].
fn cl_imm(c: u32) -> u32 {
    ((c >> 7) & 0x38) | ((c >> 4) & 0x4) | ((c << 1) & 0x40)
}

/// c.j / c.jal: offset[11|4|9:8|10|6|7|3:1|5] = c[12:2].
fn cj_imm(c: u32) -> u32 {
    let v = ((c >> 1) & 0x800)
        | ((c >> 7) & 0x10)
        | ((c >> 1) & 0x300)
        | ((c << 2) & 0x400)
        | ((c >> 1) & 0x40)
        | ((c << 1) & 0x80)
        | ((c >> 2) & 0xe)
        | ((c << 3) & 0x20);
    sext(v, 12)
}

/// c.beqz / c.bnez: offset[8|4:3] = c[12:10], offset[7:6|2:1|5] = c[6:2].
fn cb_imm(c: u32) -> u32 {
    let v = ((c >> 4) & 0x100) | ((c >> 7) & 0x18) | ((c << 1) & 0xc0) | ((c >> 2) & 0x6) | ((c << 3) & 0x20);
    sext(v, 9)
}

/// c.lwsp / c.flwsp: uimm[5] = c[12], uimm[4:2|7:6] = c[6:2].
fn ci_lw_imm(c: u32) -> u32 {
    ((c >> 7) & 0x20) | ((c >> 2) & 0x1c) | ((c << 4) & 0xc0)
}

/// c.swsp / c.fswsp: uimm[5:2|7:6] = c[12:7].
fn css_imm(c: u32) -> u32 {
    ((c >> 7) & 0x3c) | ((c >> 1) & 0xc0)
}

fn r(op: u32, rd: u32, f3: u32, rs1: u32, rs2: u32, f7: u32) -> u32 {
    (f7 << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | op
}

fn i(op: u32, rd: u32, f3: u32, rs1: u32, imm: u32) -> u32 {
    ((imm & 0xfff) << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | op
}

fn s(op: u32, f3: u32, rs1: u32, rs2: u32, imm: u32) -> u32 {
    (((imm >> 5) & 0x7f) << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | ((imm & 0x1f) << 7) | op
}

fn b(f3: u32, rs1: u32, rs2: u32, imm: u32) -> u32 {
    (((imm >> 12) & 1) << 31)
        | (((imm >> 5) & 0x3f) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (f3 << 12)
        | (((imm >> 1) & 0xf) << 8)
        | (((imm >> 11) & 1) << 7)
        | 0x63
}

fn u(op: u32, rd: u32, imm: u32) -> u32 {
    (imm & 0xffff_f000) | (rd << 7) | op
}

fn j(rd: u32, imm: u32) -> u32 {
    (((imm >> 20) & 1) << 31)
        | (((imm >> 1) & 0x3ff) << 21)
        | (((imm >> 11) & 1) << 20)
        | (((imm >> 12) & 0xff) << 12)
        | (rd << 7)
        | 0x6f
}

#[cfg(test)]
mod tests {
    use super::expand;

    /// Spot checks. Expected values come from llvm-mc 18
    /// (`--disassemble -show-encoding -mattr=+c,+f`, then reassembled without C).
    /// The exhaustive comparison is `cross_check_with_llvm_table` below.
    #[test]
    fn known_encodings() {
        let cases: &[(u32, u32)] = &[
            (0x0040, 0x00410413), // addi s0, sp, 4
            (0x4108, 0x00052503), // lw a0, 0(a0)
            (0x8502, 0x00050067), // jr a0
            (0xbffd, 0xfffff06f), // j -2
        ];
        for &(c, want) in cases {
            assert_eq!(expand(c), Some(want), "c=0x{c:04x}");
        }
    }

    #[test]
    fn illegal() {
        assert_eq!(expand(0x0000), None); // all zeros
        assert_eq!(expand(0x2000), None); // c.fld
        assert_eq!(expand(0x8002), None); // c.jr x0
    }

    /// Compares every 16-bit encoding against a table generated with llvm-mc
    /// (doc/11 §4). Set T5EMU_RVC_TABLE to the table path to run it; each line
    /// is `cccc eeeeeeee text` or `cccc ? text` for HINT/reserved forms that
    /// have no 32-bit equivalent in the assembler.
    #[test]
    fn cross_check_with_llvm_table() {
        let Ok(path) = std::env::var("T5EMU_RVC_TABLE") else {
            return;
        };
        let table = std::fs::read_to_string(path).unwrap();
        let mut listed = std::collections::HashSet::new();
        let mut mismatches = Vec::new();
        for line in table.lines() {
            let mut it = line.splitn(3, ' ');
            let c = u32::from_str_radix(it.next().unwrap(), 16).unwrap();
            let e = it.next().unwrap();
            listed.insert(c);
            let got = expand(c);
            if c == 0 {
                // `unimp`: llvm-mc spells it as a write to the read-only
                // `cycle` CSR, which is just as illegal.
                assert_eq!(got, None);
                continue;
            }
            if e == "?" {
                // HINTs must have no architectural effect: write x0, or shift
                // or add 0 to a register in place. Reserved RV32 shifts (shamt >= 32) and
                // c.lui with imm 0 are rejected.
                if let Some(w) = got {
                    let rd = (w >> 7) & 0x1f;
                    let rs1 = (w >> 15) & 0x1f;
                    let shift0 = w & 0x7f == 0x13 && matches!((w >> 12) & 7, 1 | 5) && (w >> 20) & 0x1f == 0;
                    let addi0 = w & 0x7f == 0x13 && (w >> 12) & 7 == 0 && w >> 20 == 0;
                    assert!(rd == 0 || ((shift0 || addi0) && rd == rs1), "{line}: got {w:08x}");
                }
            } else {
                let want = u32::from_str_radix(e, 16).unwrap();
                let text = line.splitn(3, ' ').nth(2).unwrap();
                let same = match got {
                    Some(g) if g == want => true,
                    // `mv rd, rs` reassembles as `addi rd, rs, 0`; the spec
                    // expands c.mv to `add rd, x0, rs`. Same effect.
                    Some(g) if text.starts_with("mv") => {
                        let (rd, rs) = ((want >> 7) & 0x1f, (want >> 15) & 0x1f);
                        g == (rs << 20) | (rd << 7) | 0x33
                    }
                    // llvm-mc decodes c.lui with nzimm = 0, which the spec
                    // reserves. We keep it illegal.
                    None => text.starts_with("lui") && want >> 12 == 0,
                    _ => false,
                };
                if !same {
                    mismatches.push(format!("{line} -> {got:08x?}"));
                }
            }
        }
        assert!(mismatches.is_empty(), "{} mismatches:\n{}", mismatches.len(), mismatches.join("\n"));
        // Encodings llvm-mc rejects must be illegal here too, except the
        // D-precision forms and the RV64-only ones, which are already None.
        for c in 0..0x10000u32 {
            if c & 3 != 3 && !listed.contains(&c) {
                assert_eq!(expand(c), None, "c=0x{c:04x} is not in the llvm table");
            }
        }
    }
}
