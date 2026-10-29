//! Control commands from the host (doc/14 §3). They travel over the RTT
//! down-channel as COBS frames terminated by 0x00, like log records going
//! the other way. Results come back as log events (`ctl.done` and the
//! events of the operation itself), not as a separate reply format.
//!
//! Frame: `u8 command | u32 request_id | arguments`.

pub mod cmd {
    pub const LIST: u8 = 1;
    /// `u8 name_len | name | u32 arg0 | u32 arg1`
    pub const RUN: u8 = 2;
    /// `u32 pid`
    pub const KILL: u8 = 3;
    pub const PS: u8 = 4;
}

/// Largest command frame (before COBS).
pub const MAX_COMMAND: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command<'a> {
    List,
    Run { name: &'a str, arg0: u32, arg1: u32 },
    Kill { pid: u32 },
    Ps,
}

impl Command<'_> {
    pub fn name(&self) -> &'static str {
        match self {
            Command::List => "list",
            Command::Run { .. } => "run",
            Command::Kill { .. } => "kill",
            Command::Ps => "ps",
        }
    }
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// Decodes a command frame (after COBS): `(request_id, command)`.
pub fn decode(b: &[u8]) -> Option<(u32, Command<'_>)> {
    let id = u32_at(b, 1)?;
    let args = b.get(5..)?;
    let c = match *b.first()? {
        cmd::LIST if args.is_empty() => Command::List,
        cmd::PS if args.is_empty() => Command::Ps,
        cmd::KILL if args.len() == 4 => Command::Kill { pid: u32_at(args, 0)? },
        cmd::RUN => {
            let n = *args.first()? as usize;
            let name = core::str::from_utf8(args.get(1..1 + n)?).ok()?;
            if args.len() != 1 + n + 8 {
                return None;
            }
            Command::Run { name, arg0: u32_at(args, 1 + n)?, arg1: u32_at(args, 5 + n)? }
        }
        _ => return None,
    };
    Some((id, c))
}

/// Encodes a command (before COBS) into `out`; returns its length.
pub fn encode(id: u32, c: &Command, out: &mut [u8]) -> Option<usize> {
    let mut o = 0;
    let mut put = |bytes: &[u8]| -> Option<()> {
        out.get_mut(o..o + bytes.len())?.copy_from_slice(bytes);
        o += bytes.len();
        Some(())
    };
    match c {
        Command::List => put(&[cmd::LIST])?,
        Command::Ps => put(&[cmd::PS])?,
        Command::Kill { .. } => put(&[cmd::KILL])?,
        Command::Run { .. } => put(&[cmd::RUN])?,
    }
    put(&id.to_le_bytes())?;
    match c {
        Command::Kill { pid } => put(&pid.to_le_bytes())?,
        Command::Run { name, arg0, arg1 } => {
            if name.len() > 32 {
                return None;
            }
            put(&[name.len() as u8])?;
            put(name.as_bytes())?;
            put(&arg0.to_le_bytes())?;
            put(&arg1.to_le_bytes())?;
        }
        _ => {}
    }
    Some(o)
}

/// Parses a command line as typed on the host: `list`, `ps`, `kill PID`,
/// `run NAME [ARG0 [ARG1]]`.
pub fn parse_line(line: &str) -> Result<Command<'_>, &'static str> {
    let mut w = line.split_whitespace();
    let num = |s: Option<&str>| -> Result<u32, &'static str> {
        match s {
            None => Ok(0),
            Some(s) => match s.strip_prefix("0x") {
                Some(h) => u32::from_str_radix(h, 16).map_err(|_| "bad number"),
                None => s.parse().map_err(|_| "bad number"),
            },
        }
    };
    let c = match w.next() {
        Some("list") => Command::List,
        Some("ps") => Command::Ps,
        Some("kill") => Command::Kill { pid: num(Some(w.next().ok_or("kill PID")?))? },
        Some("run") => {
            let name = w.next().ok_or("run NAME [ARG0 [ARG1]]")?;
            let arg0 = num(w.next())?;
            let arg1 = num(w.next())?;
            Command::Run { name, arg0, arg1 }
        }
        _ => return Err("commands: list | ps | run NAME [ARG0 [ARG1]] | kill PID"),
    };
    if w.next().is_some() {
        return Err("too many arguments");
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for line in ["list", "ps", "kill 3", "run ticker 1 0x1e"] {
            let c = parse_line(line).unwrap();
            let mut b = [0u8; MAX_COMMAND];
            let n = encode(42, &c, &mut b).unwrap();
            assert_eq!(decode(&b[..n]), Some((42, c)), "{line}");
        }
        assert_eq!(parse_line("run ticker 1 30").unwrap(), Command::Run { name: "ticker", arg0: 1, arg1: 30 });
        assert!(parse_line("kill").is_err());
        assert!(decode(&[cmd::KILL, 0, 0, 0, 0, 1]).is_none());
        assert!(decode(&[9, 0, 0, 0, 0]).is_none());
    }
}
