//! Decoder for tab5-os structured logs (doc/12). Shared by the `t5log` CLI
//! and the t5emu emulator.
//!
//! - [`SchemaTable`] collects schema texts from the `.t5log` sections of the
//!   kernel and app ELFs (or from a schema file made by `t5log schemas`).
//! - [`StreamDecoder`] splits a byte stream into COBS frames and records;
//!   bytes that are not frames (the bootloader's text) come out as text.
//! - [`Event::to_json`] / [`Event::to_pretty`] render one record.

pub mod capture;
pub mod ctl;
#[cfg(feature = "rtt")]
pub mod rtt;

use std::collections::HashMap;
use std::fmt::Write as _;

use t5_abi::log::{self as wire, Level, tag};

/// One call site's schema (see `t5_abi::event!`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schema {
    pub id: u32,
    pub name: String,
    pub level: String,
    pub file: String,
    pub line: u32,
    pub fields: Vec<String>,
    pub msg: Option<String>,
    /// The exact text the id was computed from.
    pub text: String,
}

/// Field names that would clash with the record's own keys in JSON.
pub const RESERVED_FIELDS: &[&str] = &["seq", "t_us", "src", "level", "event", "prev_boot"];

/// Tells which events were replayed from a previous boot: the kernel sends
/// them between `log.prev_begin` and `log.prev_end` (doc/12 L3).
#[derive(Default)]
pub struct BootContext {
    prev: Option<u32>,
}

impl BootContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// The previous boot's id if `e` is a replayed record.
    pub fn classify(&mut self, e: &Event, t: &SchemaTable) -> Option<u32> {
        match e.name(t) {
            Some("log.prev_begin") => {
                self.prev = match e.field(t, "boot_id") {
                    Some(Value::U(id)) => Some(*id as u32),
                    _ => Some(0),
                };
                None
            }
            Some("log.prev_end") => {
                self.prev = None;
                None
            }
            _ => self.prev,
        }
    }
}

/// Splits a `.t5log` section (or a schema file) into schemas. Each schema
/// starts with `t5log/1\n` and ends with an empty line.
pub fn parse_schemas(bytes: &[u8]) -> Result<Vec<Schema>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("schema text is not UTF-8: {e}"))?;
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(wire::SCHEMA_MAGIC) {
        let block = &rest[start..];
        let end = block.find("\n\n").ok_or("unterminated schema")? + 2;
        let block = &block[..end];
        out.push(parse_one(block)?);
        rest = &rest[start + end..];
    }
    Ok(out)
}

fn parse_one(block: &str) -> Result<Schema, String> {
    let mut s = Schema {
        id: wire::fnv1a(block.as_bytes()),
        name: String::new(),
        level: String::new(),
        file: String::new(),
        line: 0,
        fields: Vec::new(),
        msg: None,
        text: block.to_string(),
    };
    for line in block.lines().skip(1) {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "name" => s.name = v.to_string(),
            "level" => s.level = v.to_lowercase(),
            "file" => s.file = v.to_string(),
            "line" => s.line = v.parse().unwrap_or(0),
            "fields" => s.fields = v.split(',').filter(|f| !f.is_empty()).map(str::to_string).collect(),
            "msg" if !v.is_empty() => s.msg = Some(v.to_string()),
            _ => {}
        }
    }
    if s.name.is_empty() {
        return Err(format!("schema without a name:\n{block}"));
    }
    Ok(s)
}

#[derive(Default)]
pub struct SchemaTable {
    map: HashMap<u32, Schema>,
}

impl SchemaTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&self, id: u32) -> Option<&Schema> {
        self.map.get(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Schema> {
        let mut v: Vec<&Schema> = self.map.values().collect();
        v.sort_by(|a, b| (&a.name, &a.file, a.line).cmp(&(&b.name, &b.file, b.line)));
        v.into_iter()
    }

    /// Adds schemas; identical texts merge, different texts with the same id
    /// are a collision.
    pub fn add(&mut self, schemas: Vec<Schema>) -> Result<(), String> {
        for s in schemas {
            match self.map.get(&s.id) {
                Some(old) if old.text != s.text => {
                    return Err(format!(
                        "schema id collision {:#010x}: {} ({}:{}) and {} ({}:{})",
                        s.id, old.name, old.file, old.line, s.name, s.file, s.line
                    ));
                }
                Some(_) => {}
                None => {
                    self.map.insert(s.id, s);
                }
            }
        }
        Ok(())
    }

    pub fn add_elf(&mut self, path: &str) -> Result<usize, String> {
        let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        let schemas = parse_schemas(&elf_section(&data, wire::SECTION).map_err(|e| format!("{path}: {e}"))?)?;
        let n = schemas.len();
        self.add(schemas).map_err(|e| format!("{path}: {e}"))?;
        Ok(n)
    }

    pub fn add_file(&mut self, path: &str) -> Result<usize, String> {
        let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        let schemas = parse_schemas(&data)?;
        let n = schemas.len();
        self.add(schemas).map_err(|e| format!("{path}: {e}"))?;
        Ok(n)
    }

    /// The schema file format: the schema texts, concatenated.
    pub fn to_file_bytes(&self) -> Vec<u8> {
        self.iter().flat_map(|s| s.text.bytes()).collect()
    }

    /// Naming rules (doc/12 §2.1): lower-case dotted names, no reserved
    /// field names, no duplicate fields.
    pub fn check(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for s in self.iter() {
            let ok_name = s.name.split('.').count() >= 2
                && s.name.split('.').all(|p| {
                    !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                });
            if !ok_name {
                problems.push(format!("{}:{}: event name {:?} is not lower_case.dotted", s.file, s.line, s.name));
            }
            for (i, f) in s.fields.iter().enumerate() {
                if RESERVED_FIELDS.contains(&f.as_str()) {
                    problems.push(format!("{}:{}: field name {f:?} is reserved", s.file, s.line));
                }
                if s.fields[..i].contains(f) {
                    problems.push(format!("{}:{}: duplicate field {f:?}", s.file, s.line));
                }
            }
        }
        problems
    }
}

fn elf_section(data: &[u8], name: &str) -> Result<Vec<u8>, String> {
    use object::read::elf::{FileHeader, SectionHeader};
    let header = object::elf::FileHeader32::<object::Endianness>::parse(data).map_err(|e| e.to_string())?;
    let endian = header.endian().map_err(|e| e.to_string())?;
    let sections = header.sections(endian, data).map_err(|e| e.to_string())?;
    for s in sections.iter() {
        if sections.section_name(endian, s).map_err(|e| e.to_string())? == name.as_bytes() {
            return Ok(s.data(endian, data).map_err(|e| e.to_string())?.to_vec());
        }
    }
    Ok(Vec::new())
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    U(u64),
    I(i64),
    Bool(bool),
    F32(f32),
    Str { s: String, truncated: bool },
    Bytes { b: Vec<u8>, truncated: bool },
    Addr(u32),
}

impl Value {
    fn json(&self, out: &mut String) {
        match self {
            Value::U(v) => write!(out, "{v}").unwrap(),
            Value::I(v) => write!(out, "{v}").unwrap(),
            Value::Bool(v) => write!(out, "{v}").unwrap(),
            Value::F32(v) if v.is_finite() => write!(out, "{v}").unwrap(),
            Value::F32(v) => json_str(out, &v.to_string()),
            Value::Str { s, .. } => json_str(out, s),
            Value::Bytes { b, .. } => json_str(out, &hex(b)),
            Value::Addr(a) => json_str(out, &format!("{a:#010x}")),
        }
    }

    fn pretty(&self) -> String {
        match self {
            Value::Str { s, truncated } => format!("{s}{}", if *truncated { "…" } else { "" }),
            Value::Bytes { b, truncated } => format!("{}{}", hex(b), if *truncated { "…" } else { "" }),
            Value::Addr(a) => format!("{a:#010x}"),
            Value::U(v) => v.to_string(),
            Value::I(v) => v.to_string(),
            Value::Bool(v) => v.to_string(),
            Value::F32(v) => v.to_string(),
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32).unwrap(),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A decoded record.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub level: Level,
    pub id: u32,
    pub seq: u64,
    pub t_us: u64,
    pub source: u32,
    pub values: Vec<Value>,
}

pub fn decode_record(r: &[u8]) -> Result<Event, String> {
    if r.len() < 5 {
        return Err("record too short".into());
    }
    let level = Level::from_u8(r[0] >> 5).ok_or("bad level")?;
    let count = (r[0] & 0x1F) as usize;
    let id = u32::from_le_bytes(r[1..5].try_into().unwrap());
    let mut o = 5;
    let next = |o: &mut usize| -> Result<u64, String> {
        let (v, n) = wire::get_varint(&r[*o..]).ok_or("bad varint")?;
        *o += n;
        Ok(v)
    };
    let seq = next(&mut o)?;
    let t_us = next(&mut o)?;
    let source = next(&mut o)? as u32;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let raw = *r.get(o).ok_or("missing field")?;
        o += 1;
        let truncated = raw & tag::TRUNCATED != 0;
        let t = raw & !tag::TRUNCATED;
        let take = |o: &mut usize, n: usize| -> Result<&[u8], String> {
            let b = r.get(*o..*o + n).ok_or("field past the end")?;
            *o += n;
            Ok(b)
        };
        let v = match t {
            tag::U8 => Value::U(take(&mut o, 1)?[0] as u64),
            tag::U16 => Value::U(u16::from_le_bytes(take(&mut o, 2)?.try_into().unwrap()) as u64),
            tag::U32 => Value::U(u32::from_le_bytes(take(&mut o, 4)?.try_into().unwrap()) as u64),
            tag::U64 => Value::U(u64::from_le_bytes(take(&mut o, 8)?.try_into().unwrap())),
            tag::I8 => Value::I(take(&mut o, 1)?[0] as i8 as i64),
            tag::I16 => Value::I(i16::from_le_bytes(take(&mut o, 2)?.try_into().unwrap()) as i64),
            tag::I32 => Value::I(i32::from_le_bytes(take(&mut o, 4)?.try_into().unwrap()) as i64),
            tag::I64 => Value::I(i64::from_le_bytes(take(&mut o, 8)?.try_into().unwrap())),
            tag::BOOL => Value::Bool(take(&mut o, 1)?[0] != 0),
            tag::F32 => Value::F32(f32::from_le_bytes(take(&mut o, 4)?.try_into().unwrap())),
            tag::ADDR => Value::Addr(u32::from_le_bytes(take(&mut o, 4)?.try_into().unwrap())),
            tag::STR | tag::BYTES => {
                let (n, used) = wire::get_varint(&r[o..]).ok_or("bad length")?;
                o += used;
                let b = take(&mut o, n as usize)?.to_vec();
                if t == tag::STR {
                    Value::Str { s: String::from_utf8_lossy(&b).into_owned(), truncated }
                } else {
                    Value::Bytes { b, truncated }
                }
            }
            _ => return Err(format!("unknown field tag {raw:#04x}")),
        };
        values.push(v);
    }
    if o != r.len() {
        return Err("trailing bytes after the fields".into());
    }
    Ok(Event { level, id, seq, t_us, source, values })
}

impl Event {
    pub fn name<'a>(&self, table: &'a SchemaTable) -> Option<&'a str> {
        table.get(self.id).map(|s| s.name.as_str())
    }

    /// The value of field `name`, if the schema is known.
    pub fn field(&self, table: &SchemaTable, name: &str) -> Option<&Value> {
        let s = table.get(self.id)?;
        s.fields.iter().position(|f| f == name).and_then(|i| self.values.get(i))
    }

    fn source_name(&self) -> String {
        if self.source == wire::SOURCE_KERNEL { "kernel".into() } else { format!("pid{}", self.source) }
    }

    /// One JSON object: `seq`, `t_us`, `src`, `level`, `event`, then the
    /// fields by name (or `id` and `values` if the schema is unknown).
    pub fn to_json(&self, table: &SchemaTable) -> String {
        self.to_json_in(table, None)
    }

    /// `to_json` with `"prev_boot":ID` for records replayed from boot ID.
    pub fn to_json_in(&self, table: &SchemaTable, prev_boot: Option<u32>) -> String {
        let mut o = String::new();
        write!(o, "{{\"seq\":{},\"t_us\":{},", self.seq, self.t_us).unwrap();
        if let Some(b) = prev_boot {
            write!(o, "\"prev_boot\":{b},").unwrap();
        }
        o.push_str("\"src\":");
        json_str(&mut o, &self.source_name());
        write!(o, ",\"level\":\"{}\"", self.level.name()).unwrap();
        match table.get(self.id) {
            Some(s) => {
                o.push_str(",\"event\":");
                json_str(&mut o, &s.name);
                for (i, v) in self.values.iter().enumerate() {
                    o.push(',');
                    let key = s.fields.get(i).cloned().unwrap_or_else(|| format!("_{i}"));
                    json_str(&mut o, &key);
                    o.push(':');
                    v.json(&mut o);
                }
            }
            None => {
                write!(o, ",\"event\":null,\"id\":\"{:#010x}\",\"values\":[", self.id).unwrap();
                for (i, v) in self.values.iter().enumerate() {
                    if i > 0 {
                        o.push(',');
                    }
                    v.json(&mut o);
                }
                o.push(']');
            }
        }
        o.push('}');
        o
    }

    /// One line for people: time, level, source, name, then the message
    /// template if the schema has one, else `key=value` pairs.
    /// `to_pretty` with a `prev#ID` mark for records replayed from boot ID.
    pub fn to_pretty_in(&self, table: &SchemaTable, prev_boot: Option<u32>) -> String {
        match prev_boot {
            Some(b) => format!("prev#{b} {}", self.to_pretty(table)),
            None => self.to_pretty(table),
        }
    }

    pub fn to_pretty(&self, table: &SchemaTable) -> String {
        let lvl = match self.level {
            Level::Error => "E",
            Level::Warn => "W",
            Level::Info => "I",
            Level::Debug => "D",
            Level::Trace => "T",
        };
        let head = format!(
            "{:>4}.{:06} {lvl} {:<6}",
            self.t_us / 1_000_000,
            self.t_us % 1_000_000,
            self.source_name()
        );
        let Some(s) = table.get(self.id) else {
            let vals: Vec<String> = self.values.iter().map(Value::pretty).collect();
            return format!("{head} <unknown {:#010x}> {}", self.id, vals.join(" "));
        };
        let field = |name: &str| s.fields.iter().position(|f| f == name).and_then(|i| self.values.get(i));
        if let Some(t) = &s.msg {
            let mut out = String::new();
            let mut rest = t.as_str();
            while let Some(a) = rest.find('{') {
                out.push_str(&rest[..a]);
                let Some(b) = rest[a..].find('}') else { break };
                let key = &rest[a + 1..a + b];
                match field(key) {
                    Some(v) => out.push_str(&v.pretty()),
                    None => out.push_str(&rest[a..=a + b]),
                }
                rest = &rest[a + b + 1..];
            }
            out.push_str(rest);
            return format!("{head} {}: {out}", s.name);
        }
        let kv: Vec<String> = s
            .fields
            .iter()
            .zip(self.values.iter())
            .map(|(k, v)| format!("{k}={}", v.pretty()))
            .collect();
        format!("{head} {} {}", s.name, kv.join(" ")).trim_end().to_string()
    }
}

/// What the stream decoder found.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Event(Event),
    /// Bytes that are not a record (e.g. the bootloader's text).
    Text(String),
    /// A frame that failed to decode.
    Bad { bytes: Vec<u8>, error: String },
    /// A screen capture frame (doc/54), decoded from COBS.
    Capture(Vec<u8>),
}

/// Splits a byte stream at 0x00 and decodes each frame.
#[derive(Default)]
pub struct StreamDecoder {
    pending: Vec<u8>,
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<Item> {
        let mut out = Vec::new();
        for &b in bytes {
            if b != 0 {
                self.pending.push(b);
                continue;
            }
            let frame = std::mem::take(&mut self.pending);
            if frame.is_empty() {
                continue;
            }
            out.push(decode_frame(&frame));
        }
        out
    }

    /// Whatever is left without a terminator (shown as text).
    pub fn finish(&mut self) -> Option<Item> {
        let rest = std::mem::take(&mut self.pending);
        (!rest.is_empty()).then(|| Item::Text(String::from_utf8_lossy(&rest).into_owned()))
    }
}

fn decode_frame(frame: &[u8]) -> Item {
    // Mostly-printable frames are text that ran into a 0x00 (the bootloader
    // output before the kernel's first frame).
    let printable = frame.iter().filter(|&&b| b == b'\n' || b == b'\r' || (0x20..0x7F).contains(&b)).count();
    let mut rec = vec![0u8; frame.len()];
    if let Some(n) = wire::cobs_decode(frame, &mut rec)
        && n >= 2
        && rec[0] == t5_abi::capture::FRAME
    {
        rec.truncate(n);
        return Item::Capture(rec);
    }
    let decoded = wire::cobs_decode(frame, &mut rec)
        .ok_or_else(|| "bad COBS".to_string())
        .and_then(|n| decode_record(&rec[..n]));
    match decoded {
        Ok(e) => Item::Event(e),
        Err(_) if printable * 10 >= frame.len() * 9 => Item::Text(String::from_utf8_lossy(frame).into_owned()),
        Err(error) => Item::Bad { bytes: frame.to_vec(), error },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use t5_abi::log::{Addr, Field, Payload};

    fn schema(name: &str, fields: &str, msg: &str) -> String {
        format!("t5log/1\nname={name}\nlevel=Info\nfile=x.rs\nline=1\nfields={fields}\nmsg={msg}\n\n")
    }

    fn frame(id: u32, seq: u64, fields: impl Fn(&mut Payload), n: usize) -> Vec<u8> {
        let mut p = Payload::new(Level::Info, id, n);
        fields(&mut p);
        let pb = p.as_bytes();
        let mut rec = pb[..5].to_vec();
        for v in [seq, 1_500_000u64, 2] {
            let mut b = [0u8; 10];
            let k = wire::put_varint(&mut b, v).unwrap();
            rec.extend_from_slice(&b[..k]);
        }
        rec.extend_from_slice(&pb[5..]);
        let mut out = vec![0u8; rec.len() + 4];
        let k = wire::cobs_encode(&rec, &mut out).unwrap();
        out.truncate(k);
        out
    }

    #[test]
    fn roundtrip_with_schema() {
        let text = schema("proc.exit", "pid,code,pc,", "{pid} exited with {code}");
        let mut t = SchemaTable::new();
        t.add(parse_schemas(text.as_bytes()).unwrap()).unwrap();
        let id = wire::fnv1a(text.as_bytes());

        let mut stream = b"ESP-ROM boot text\n".to_vec();
        stream.push(0);
        stream.extend(frame(
            id,
            7,
            |p| {
                3u32.t5log_encode(p);
                0usize.t5log_encode(p);
                Addr(0x4FF4_0000).t5log_encode(p);
            },
            3,
        ));
        let mut d = StreamDecoder::new();
        let items = d.push(&stream);
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], Item::Text(s) if s.starts_with("ESP-ROM")));
        let Item::Event(e) = &items[1] else { panic!("{:?}", items[1]) };
        assert_eq!(e.seq, 7);
        assert_eq!(e.source, 2);
        assert_eq!(
            e.to_json(&t),
            r#"{"seq":7,"t_us":1500000,"src":"pid2","level":"info","event":"proc.exit","pid":3,"code":0,"pc":"0x4ff40000"}"#
        );
        assert_eq!(e.to_pretty(&t), "   1.500000 I pid2   proc.exit: 3 exited with 0");
        assert_eq!(e.field(&t, "pid"), Some(&Value::U(3)));
    }

    #[test]
    fn collisions_and_checks() {
        let mut t = SchemaTable::new();
        let a = schema("a.b", "x,", "");
        t.add(parse_schemas(a.as_bytes()).unwrap()).unwrap();
        // The same text twice merges.
        t.add(parse_schemas(a.as_bytes()).unwrap()).unwrap();
        assert_eq!(t.len(), 1);
        // A forged schema with the same id but different text is rejected.
        let mut forged = parse_schemas(schema("c.d", "", "").as_bytes()).unwrap();
        forged[0].id = wire::fnv1a(a.as_bytes());
        assert!(t.add(forged).is_err());

        let mut bad = SchemaTable::new();
        bad.add(parse_schemas(schema("Bad", "seq,x,x,", "").as_bytes()).unwrap()).unwrap();
        assert_eq!(bad.check().len(), 3);
    }
}
