//! t5log: tools for tab5-os structured logs (doc/12).
//!
//!   t5log schemas -o OUT ELF...     collect schemas into a file (and check them)
//!   t5log check ELF...              check schemas: id collisions, naming rules
//!   t5log decode [--schemas F]... [--elf E]... [--pretty] [--tcp HOST:PORT | INPUT]
//!                                   decode a log stream to JSON Lines (default)
//!                                   or one line per event (--pretty). INPUT is a
//!                                   file or device (default: stdin). For a tty,
//!                                   set it to raw mode first (stty raw -echo).
//!                                   --hex reads the bytes as hex pairs ("0a 1b "),
//!                                   which is how probe-rs prints a binary RTT
//!                                   channel; other text is ignored.

use std::io::{Read, Write};
use std::process::ExitCode;

use t5log::{BootContext, Item, SchemaTable, StreamDecoder};

const USAGE: &str = "usage:
  t5log schemas -o OUT ELF...
  t5log check ELF...
  t5log decode [--schemas FILE]... [--elf ELF]... [--pretty] [--hex] [--tcp HOST:PORT | INPUT]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("schemas") => cmd_schemas(&args[1..]),
        Some("check") => cmd_check(&args[1..]),
        Some("decode") => cmd_decode(&args[1..]),
        _ => Err(USAGE.to_string()),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("t5log: {e}");
            ExitCode::FAILURE
        }
    }
}

fn load_elfs(paths: &[String]) -> Result<SchemaTable, String> {
    let mut t = SchemaTable::new();
    for p in paths {
        t.add_elf(p)?;
    }
    Ok(t)
}

fn check(t: &SchemaTable) -> Result<(), String> {
    let problems = t.check();
    for p in &problems {
        eprintln!("t5log: {p}");
    }
    if problems.is_empty() { Ok(()) } else { Err(format!("{} schema problem(s)", problems.len())) }
}

fn cmd_check(args: &[String]) -> Result<(), String> {
    let t = load_elfs(args)?;
    check(&t)?;
    eprintln!("t5log: {} schemas OK", t.len());
    Ok(())
}

fn cmd_schemas(args: &[String]) -> Result<(), String> {
    let mut out = None;
    let mut elfs = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = Some(it.next().ok_or("-o needs a path")?.clone()),
            _ => elfs.push(a.clone()),
        }
    }
    let out = out.ok_or("missing -o OUT")?;
    let t = load_elfs(&elfs)?;
    check(&t)?;
    std::fs::write(&out, t.to_file_bytes()).map_err(|e| format!("{out}: {e}"))?;
    eprintln!("t5log: {} schemas from {} ELF(s) -> {out}", t.len(), elfs.len());
    Ok(())
}

fn cmd_decode(args: &[String]) -> Result<(), String> {
    let mut t = SchemaTable::new();
    let mut pretty = false;
    let mut hex = false;
    let mut tcp = None;
    let mut input = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--schemas" => {
                t.add_file(it.next().ok_or("--schemas needs a path")?)?;
            }
            "--elf" => {
                t.add_elf(it.next().ok_or("--elf needs a path")?)?;
            }
            "--pretty" => pretty = true,
            "--hex" => hex = true,
            "--json" => pretty = false,
            "--tcp" => tcp = Some(it.next().ok_or("--tcp needs HOST:PORT")?.clone()),
            s if s.starts_with("--") => return Err(format!("unknown option {s}\n{USAGE}")),
            s => input = Some(s.to_string()),
        }
    }
    if t.is_empty() {
        eprintln!("t5log: no schemas given (--schemas/--elf); events will show as ids");
    }
    let mut reader: Box<dyn Read> = match (tcp, input.as_deref()) {
        (Some(addr), _) => Box::new(std::net::TcpStream::connect(&addr).map_err(|e| format!("{addr}: {e}"))?),
        (None, None | Some("-")) => Box::new(std::io::stdin()),
        (None, Some(path)) => Box::new(std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?),
    };

    let mut d = StreamDecoder::new();
    let mut ctx = BootContext::new();
    let mut hexd = HexDecoder::default();
    let mut out = std::io::stdout().lock();
    let mut buf = [0u8; 4096];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
        };
        let bytes = if hex { hexd.push(&buf[..n]) } else { buf[..n].to_vec() };
        for item in d.push(&bytes) {
            print_item(&mut out, &t, &mut ctx, &item, pretty)?;
        }
        out.flush().map_err(|e| e.to_string())?;
    }
    if let Some(item) = d.finish() {
        print_item(&mut out, &t, &mut ctx, &item, pretty)?;
    }
    Ok(())
}

/// Turns "0a 1b ..." text into bytes. A token is two hex digits surrounded
/// by whitespace; anything else (status lines) is skipped.
#[derive(Default)]
struct HexDecoder {
    token: Vec<u8>,
}

impl HexDecoder {
    fn push(&mut self, text: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for &c in text {
            if c.is_ascii_whitespace() {
                if self.token.len() == 2 && self.token.iter().all(u8::is_ascii_hexdigit) {
                    let s = std::str::from_utf8(&self.token).unwrap();
                    out.push(u8::from_str_radix(s, 16).unwrap());
                }
                self.token.clear();
            } else if self.token.len() < 3 {
                self.token.push(c);
            }
        }
        out
    }
}

fn print_item(
    out: &mut impl Write,
    t: &SchemaTable,
    ctx: &mut BootContext,
    item: &Item,
    pretty: bool,
) -> Result<(), String> {
    let line = match (item, pretty) {
        (Item::Event(e), true) => e.to_pretty_in(t, ctx.classify(e, t)),
        (Item::Event(e), false) => e.to_json_in(t, ctx.classify(e, t)),
        (Item::Text(s), true) => s.lines().map(|l| format!("| {l}")).collect::<Vec<_>>().join("\n"),
        (Item::Text(s), false) => {
            let mut j = String::from("{\"text\":");
            j.push_str(&json_string(s));
            j.push('}');
            j
        }
        (Item::Bad { bytes, error }, _) => {
            eprintln!("t5log: undecodable frame ({error}, {} bytes)", bytes.len());
            return Ok(());
        }
    };
    if line.is_empty() {
        return Ok(());
    }
    writeln!(out, "{line}").map_err(|e| e.to_string())
}

fn json_string(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}
