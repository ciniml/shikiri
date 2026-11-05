//! Structured logging, wire format v1 (doc/12). tab5-os has no text logs:
//! every log is an event with a name, a level and named, typed fields.
//!
//! - [`event!`](crate::event) builds a *payload* (header, schema id, fields)
//!   and hands it to `t5log_emit`, which the kernel and the user runtime
//!   each define. The kernel adds the sequence number, timestamp and source.
//! - The *schema* of each call site (name, level, field names, location) is
//!   a text block in the non-loaded `.t5log` ELF section; the schema id is
//!   its FNV-1a hash, so position-independent apps need no address fix-ups.
//! - Records travel as COBS frames terminated by 0x00.
//!
//! Payload (from `event!` and the `log_emit` syscall):
//! `u8 header (level << 5 | field count) | u32 schema id (LE) | field*`
//!
//! Record (what the kernel stores and sends):
//! `u8 header | u32 schema id | varint seq | varint t_us | varint source | field*`
//!
//! Field: `u8 tag | value`; see [`tag`].

pub const SCHEMA_MAGIC: &str = "t5log/1\n";
/// ELF section holding the schema texts.
pub const SECTION: &str = ".t5log";
pub const MAX_FIELDS: usize = 31;
/// Longest str/bytes value; longer values are cut and flagged.
pub const MAX_STR: usize = 128;
/// Largest payload `log_emit` accepts.
pub const MAX_PAYLOAD: usize = 256;
/// Largest record: payload plus three varints of at most 10 bytes.
pub const MAX_RECORD: usize = MAX_PAYLOAD + 30;
/// Source id of the kernel; processes use their pid (>= 1).
pub const SOURCE_KERNEL: u32 = 0;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    pub fn from_u8(v: u8) -> Option<Level> {
        Some(match v {
            0 => Level::Error,
            1 => Level::Warn,
            2 => Level::Info,
            3 => Level::Debug,
            4 => Level::Trace,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        }
    }
}

/// Field type tags.
pub mod tag {
    pub const U8: u8 = 0x01;
    pub const U16: u8 = 0x02;
    pub const U32: u8 = 0x03;
    pub const U64: u8 = 0x04;
    pub const I8: u8 = 0x05;
    pub const I16: u8 = 0x06;
    pub const I32: u8 = 0x07;
    pub const I64: u8 = 0x08;
    pub const BOOL: u8 = 0x09;
    pub const F32: u8 = 0x0A;
    /// varint length + UTF-8. Bit 7 of the tag marks a truncated value.
    pub const STR: u8 = 0x0B;
    /// varint length + bytes. Bit 7 of the tag marks a truncated value.
    pub const BYTES: u8 = 0x0C;
    /// u32 that is an address (shown in hex).
    pub const ADDR: u8 = 0x0D;
    pub const TRUNCATED: u8 = 0x80;

    /// Size of a fixed-size value, or `None` for length-prefixed ones.
    pub const fn fixed_size(t: u8) -> Option<usize> {
        match t {
            U8 | I8 | BOOL => Some(1),
            U16 | I16 => Some(2),
            U32 | I32 | F32 | ADDR => Some(4),
            U64 | I64 => Some(8),
            _ => None,
        }
    }
}

/// FNV-1a, 32 bits. The schema id of a call site.
pub const fn fnv1a(b: &[u8]) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    let mut i = 0;
    while i < b.len() {
        h ^= b[i] as u32;
        h = h.wrapping_mul(0x0100_0193);
        i += 1;
    }
    h
}

/// Copies a schema text into the array stored in `.t5log`.
pub const fn schema_bytes<const N: usize>(s: &str) -> [u8; N] {
    let b = s.as_bytes();
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = b[i];
        i += 1;
    }
    out
}

pub fn put_varint(out: &mut [u8], mut v: u64) -> Option<usize> {
    let mut n = 0;
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        *out.get_mut(n)? = if v == 0 { b } else { b | 0x80 };
        n += 1;
        if v == 0 {
            return Some(n);
        }
    }
}

/// Returns the value and the number of bytes read.
pub fn get_varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &x) in b.iter().enumerate().take(10) {
        v |= ((x & 0x7F) as u64) << (7 * i);
        if x & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// Checks that `fields` holds exactly `count` well-formed fields. The kernel
/// runs this on payloads from processes before storing them.
pub fn validate_fields(fields: &[u8], count: usize) -> bool {
    let mut o = 0;
    for _ in 0..count {
        let Some(&t) = fields.get(o) else { return false };
        o += 1;
        let t = t & !tag::TRUNCATED;
        let len = match tag::fixed_size(t) {
            Some(n) => n,
            None if t == tag::STR || t == tag::BYTES => {
                let Some((n, used)) = fields.get(o..).and_then(get_varint) else { return false };
                o += used;
                if n as usize > MAX_STR {
                    return false;
                }
                n as usize
            }
            None => return false,
        };
        o += len;
        if o > fields.len() {
            return false;
        }
    }
    o == fields.len()
}

/// COBS-encodes `input` into `out` and appends the 0x00 terminator.
/// Returns the frame length, or `None` if `out` is too small
/// (it needs `input.len() + input.len() / 254 + 2` bytes).
pub fn cobs_encode(input: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut code_at = 0;
    let mut o = 1;
    let mut code = 1u8;
    for &b in input {
        if b == 0 {
            *out.get_mut(code_at)? = code;
            code_at = o;
            o += 1;
            code = 1;
        } else {
            *out.get_mut(o)? = b;
            o += 1;
            code += 1;
            if code == 0xFF {
                *out.get_mut(code_at)? = code;
                code_at = o;
                o += 1;
                code = 1;
            }
        }
    }
    *out.get_mut(code_at)? = code;
    *out.get_mut(o)? = 0;
    Some(o + 1)
}

/// Decodes one COBS frame (without its 0x00 terminator) in place-free
/// fashion into `out`. Returns the decoded length.
pub fn cobs_decode(frame: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut i = 0;
    let mut o = 0;
    while i < frame.len() {
        let code = frame[i] as usize;
        if code == 0 {
            return None;
        }
        i += 1;
        for _ in 1..code {
            *out.get_mut(o)? = *frame.get(i)?;
            o += 1;
            i += 1;
        }
        if code != 0xFF && i < frame.len() {
            *out.get_mut(o)? = 0;
            o += 1;
        }
    }
    Some(o)
}

/// A payload under construction.
pub struct Payload {
    buf: [u8; MAX_PAYLOAD],
    len: usize,
}

impl Payload {
    pub fn new(level: Level, id: u32, fields: usize) -> Payload {
        let mut buf = [0u8; MAX_PAYLOAD];
        buf[0] = ((level as u8) << 5) | (fields.min(MAX_FIELDS) as u8);
        buf[1..5].copy_from_slice(&id.to_le_bytes());
        Payload { buf, len: 5 }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    fn room(&self) -> usize {
        MAX_PAYLOAD - self.len
    }

    pub fn put_fixed(&mut self, t: u8, v: &[u8]) {
        if self.room() < 1 + v.len() {
            return self.overflow();
        }
        self.buf[self.len] = t;
        self.buf[self.len + 1..self.len + 1 + v.len()].copy_from_slice(v);
        self.len += 1 + v.len();
    }

    pub fn put_bytes(&mut self, t: u8, v: &[u8]) {
        let mut n = v.len().min(MAX_STR);
        // Keep at least the tag and a 1-byte length for later fields.
        n = n.min(self.room().saturating_sub(3));
        if t == tag::STR {
            // Cut on a character boundary.
            while n > 0 && n < v.len() && (v[n] & 0xC0) == 0x80 {
                n -= 1;
            }
        }
        if self.room() < 3 + n {
            return self.overflow();
        }
        let flag = if n < v.len() { tag::TRUNCATED } else { 0 };
        self.buf[self.len] = t | flag;
        self.len += 1;
        self.len += put_varint(&mut self.buf[self.len..], n as u64).unwrap_or(0);
        self.buf[self.len..self.len + n].copy_from_slice(&v[..n]);
        self.len += n;
    }

    /// Out of room: the remaining fields are dropped and the header's field
    /// count is corrected, so the payload stays well-formed.
    fn overflow(&mut self) {
        let mut count = 0;
        let mut o = 5;
        while o < self.len {
            let t = self.buf[o] & !tag::TRUNCATED;
            o += 1;
            o += match tag::fixed_size(t) {
                Some(n) => n,
                None => {
                    let (n, used) = get_varint(&self.buf[o..self.len]).unwrap_or((0, 1));
                    used + n as usize
                }
            };
            count += 1;
        }
        self.buf[0] = (self.buf[0] & 0xE0) | count;
    }
}

/// Values that can be event fields.
pub trait Field {
    fn t5log_encode(&self, p: &mut Payload);
}

/// A u32 shown as an address.
#[derive(Clone, Copy)]
pub struct Addr(pub u32);

macro_rules! fixed_field {
    ($($t:ty => $tag:expr),* $(,)?) => {
        $(impl Field for $t {
            fn t5log_encode(&self, p: &mut Payload) {
                p.put_fixed($tag, &self.to_le_bytes());
            }
        })*
    };
}

fixed_field!(
    u8 => tag::U8, u16 => tag::U16, u32 => tag::U32, u64 => tag::U64,
    i8 => tag::I8, i16 => tag::I16, i32 => tag::I32, i64 => tag::I64,
    f32 => tag::F32,
);

impl Field for usize {
    fn t5log_encode(&self, p: &mut Payload) {
        if usize::BITS <= 32 { (*self as u32).t5log_encode(p) } else { (*self as u64).t5log_encode(p) }
    }
}

impl Field for bool {
    fn t5log_encode(&self, p: &mut Payload) {
        p.put_fixed(tag::BOOL, &[*self as u8]);
    }
}

impl Field for Addr {
    fn t5log_encode(&self, p: &mut Payload) {
        p.put_fixed(tag::ADDR, &self.0.to_le_bytes());
    }
}

impl Field for str {
    fn t5log_encode(&self, p: &mut Payload) {
        p.put_bytes(tag::STR, self.as_bytes());
    }
}

impl Field for [u8] {
    fn t5log_encode(&self, p: &mut Payload) {
        p.put_bytes(tag::BYTES, self);
    }
}

impl<T: Field + ?Sized> Field for &T {
    fn t5log_encode(&self, p: &mut Payload) {
        (**self).t5log_encode(p)
    }
}

/// Fixed-size `core::fmt::Write` target for building a str field (e.g. a
/// panic message) without an allocator. Output beyond `N` bytes is dropped.
pub struct FmtBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> FmtBuf<N> {
    pub const fn new() -> Self {
        FmtBuf { buf: [0; N], len: 0 }
    }

    pub fn as_str(&self) -> &str {
        // Only whole UTF-8 sequences are ever copied in.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    /// Keeps the first `len` bytes (a length `as_str` returned before).
    pub fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }
}

impl<const N: usize> Default for FmtBuf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> core::fmt::Write for FmtBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let mut n = s.len().min(N - self.len);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

unsafe extern "Rust" {
    /// Defined once per binary: by the kernel (stores the record directly)
    /// and by the user runtime (the `log_emit` syscall).
    safe fn t5log_emit(payload: &[u8]);
}

pub fn emit(p: &Payload) {
    t5log_emit(p.as_bytes());
}

/// Emits a structured log event.
///
/// ```ignore
/// event!(Info, "sched.preempt", pid = p.pid, pc = Addr(frame.pc as u32));
/// event!(Info, "proc.exit" => "{name} exited with {code}", name = p.name, code = c);
/// ```
///
/// The optional `=> "template"` is for human-readable views only; it is kept
/// in the schema, never in the record.
#[macro_export]
macro_rules! event {
    ($level:ident, $name:literal $(=> $msg:literal)? $(, $key:ident = $val:expr)* $(,)?) => {{
        const __T5LOG_SCHEMA: &str = concat!(
            "t5log/1\nname=", $name,
            "\nlevel=", stringify!($level),
            "\nfile=", file!(),
            "\nline=", line!(),
            "\nfields=", $(stringify!($key), ",",)*
            "\nmsg=", $($msg,)?
            "\n\n"
        );
        #[used]
        #[unsafe(link_section = ".t5log")]
        static __T5LOG_SCHEMA_BYTES: [u8; __T5LOG_SCHEMA.len()] =
            $crate::log::schema_bytes(__T5LOG_SCHEMA);
        const __T5LOG_ID: u32 = $crate::log::fnv1a(__T5LOG_SCHEMA.as_bytes());
        #[allow(unused_imports)]
        use $crate::log::Field as _;
        let mut __p = $crate::log::Payload::new(
            $crate::log::Level::$level,
            __T5LOG_ID,
            0 $(+ { let _ = stringify!($key); 1 })*,
        );
        $(($val).t5log_encode(&mut __p);)*
        $crate::log::emit(&__p);
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cobs_roundtrip() {
        for input in [&b""[..], b"\0", b"\0\0", b"abc", b"a\0b\0", &[1u8; 300][..], &[0u8; 300][..]] {
            let mut enc = [0u8; 700];
            let n = cobs_encode(input, &mut enc).unwrap();
            assert_eq!(enc[n - 1], 0);
            assert!(!enc[..n - 1].contains(&0));
            let mut dec = [0u8; 700];
            let m = cobs_decode(&enc[..n - 1], &mut dec).unwrap();
            assert_eq!(&dec[..m], input);
        }
    }

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut b = [0u8; 10];
            let n = put_varint(&mut b, v).unwrap();
            assert_eq!(get_varint(&b[..n]), Some((v, n)));
        }
    }

    #[test]
    fn payload_fields_validate() {
        let mut p = Payload::new(Level::Warn, 0x1234_5678, 4);
        7u8.t5log_encode(&mut p);
        "héllo".t5log_encode(&mut p);
        Addr(0x4FF4_0000).t5log_encode(&mut p);
        (-1i64).t5log_encode(&mut p);
        let b = p.as_bytes();
        assert_eq!(b[0], (1 << 5) | 4);
        assert_eq!(&b[1..5], &0x1234_5678u32.to_le_bytes());
        assert!(validate_fields(&b[5..], 4));
        assert!(!validate_fields(&b[5..], 3));
        assert!(!validate_fields(&b[5..b.len() - 1], 4));
    }

    #[test]
    fn overflow_keeps_payload_valid() {
        let long = [b'x'; 200];
        let mut p = Payload::new(Level::Info, 1, 3);
        core::str::from_utf8(&long).unwrap().t5log_encode(&mut p);
        core::str::from_utf8(&long).unwrap().t5log_encode(&mut p);
        5u32.t5log_encode(&mut p);
        let b = p.as_bytes();
        let n = (b[0] & 0x1F) as usize;
        assert!(validate_fields(&b[5..], n), "count {n}");
    }
}
