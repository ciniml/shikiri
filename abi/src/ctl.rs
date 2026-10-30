//! Control commands from the host (doc/14 §3). They travel over the RTT
//! down-channel as COBS frames terminated by 0x00, like log records going
//! the other way. Results come back as log events (`ctl.done` and the
//! events of the operation itself), not as a separate reply format.
//!
//! Frame: `u8 command | u32 request_id | arguments`.
//!
//! Installing (M3d) takes several frames: `install_begin` (length),
//! `install_data` (offset and up to `INSTALL_CHUNK` bytes, in order, each
//! starting on a `INSTALL_CHUNK` boundary), `install_end`. Data frames are
//! answered only when they fail; the kernel then drops the install, and
//! `install_end` fails too. The name comes from the signed image.

pub mod cmd {
    pub const LIST: u8 = 1;
    /// `u8 name_len | name | u32 arg0 | u32 arg1`
    pub const RUN: u8 = 2;
    /// `u32 pid`
    pub const KILL: u8 = 3;
    pub const PS: u8 = 4;
    /// `u32 length`
    pub const INSTALL_BEGIN: u8 = 5;
    /// `u32 offset | data`
    pub const INSTALL_DATA: u8 = 6;
    pub const INSTALL_END: u8 = 7;
    /// `u8 name_len | name`
    pub const REMOVE: u8 = 8;
    /// Erase/write/read-back of a spare sector (doc/14 §5.4).
    pub const FLASH_TEST: u8 = 9;
    /// Bring up the PSRAM (M4a bring-up; later done at boot).
    pub const PSRAM: u8 = 10;
}

/// Data bytes per `install_data` frame: one flash page.
pub const INSTALL_CHUNK: usize = 256;

/// Largest command frame (before COBS).
pub const MAX_COMMAND: usize = 1 + 4 + 4 + INSTALL_CHUNK;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command<'a> {
    List,
    Run { name: &'a str, arg0: u32, arg1: u32 },
    Kill { pid: u32 },
    Ps,
    InstallBegin { length: u32 },
    InstallData { offset: u32, data: &'a [u8] },
    InstallEnd,
    Remove { name: &'a str },
    FlashTest,
    Psram,
}

impl Command<'_> {
    pub fn name(&self) -> &'static str {
        match self {
            Command::List => "list",
            Command::Run { .. } => "run",
            Command::Kill { .. } => "kill",
            Command::Ps => "ps",
            Command::InstallBegin { .. } => "install_begin",
            Command::InstallData { .. } => "install_data",
            Command::InstallEnd => "install_end",
            Command::Remove { .. } => "remove",
            Command::FlashTest => "flashtest",
            Command::Psram => "psram",
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
        cmd::INSTALL_BEGIN if args.len() == 4 => Command::InstallBegin { length: u32_at(args, 0)? },
        cmd::INSTALL_DATA if args.len() > 4 && args.len() <= 4 + INSTALL_CHUNK => {
            Command::InstallData { offset: u32_at(args, 0)?, data: &args[4..] }
        }
        cmd::INSTALL_END if args.is_empty() => Command::InstallEnd,
        cmd::FLASH_TEST if args.is_empty() => Command::FlashTest,
        cmd::PSRAM if args.is_empty() => Command::Psram,
        cmd::REMOVE => {
            let n = *args.first()? as usize;
            if args.len() != 1 + n {
                return None;
            }
            Command::Remove { name: core::str::from_utf8(&args[1..]).ok()? }
        }
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
        Command::InstallBegin { .. } => put(&[cmd::INSTALL_BEGIN])?,
        Command::InstallData { .. } => put(&[cmd::INSTALL_DATA])?,
        Command::InstallEnd => put(&[cmd::INSTALL_END])?,
        Command::Remove { .. } => put(&[cmd::REMOVE])?,
        Command::FlashTest => put(&[cmd::FLASH_TEST])?,
        Command::Psram => put(&[cmd::PSRAM])?,
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
        Command::InstallBegin { length } => put(&length.to_le_bytes())?,
        Command::InstallData { offset, data } => {
            if data.is_empty() || data.len() > INSTALL_CHUNK {
                return None;
            }
            put(&offset.to_le_bytes())?;
            put(data)?;
        }
        Command::Remove { name } => {
            if name.len() > 32 {
                return None;
            }
            put(&[name.len() as u8])?;
            put(name.as_bytes())?;
        }
        _ => {}
    }
    Some(o)
}

/// Parses a command line as typed on the host: `list`, `ps`, `kill PID`,
/// `run NAME [ARG0 [ARG1]]`, `remove NAME`, `flashtest`. (`install FILE`
/// reads a file, so the host tools expand it; see `install_frames`.)
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
        Some("remove") => Command::Remove { name: w.next().ok_or("remove NAME")? },
        Some("flashtest") => Command::FlashTest,
        Some("psram") => Command::Psram,
        _ => {
            return Err(
                "commands: list | ps | run NAME [ARG0 [ARG1]] | kill PID | install FILE | remove NAME | flashtest | psram",
            );
        }
    };
    if w.next().is_some() {
        return Err("too many arguments");
    }
    Ok(c)
}

/// The commands that install `image`: begin, one data frame per
/// `INSTALL_CHUNK`, end.
pub fn install_commands(image: &[u8]) -> impl Iterator<Item = Command<'_>> {
    core::iter::once(Command::InstallBegin { length: image.len() as u32 })
        .chain(
            image
                .chunks(INSTALL_CHUNK)
                .enumerate()
                .map(|(i, data)| Command::InstallData { offset: (i * INSTALL_CHUNK) as u32, data }),
        )
        .chain(core::iter::once(Command::InstallEnd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for line in ["list", "ps", "kill 3", "run ticker 1 0x1e", "remove hello", "flashtest"] {
            let c = parse_line(line).unwrap();
            let mut b = [0u8; MAX_COMMAND];
            let n = encode(42, &c, &mut b).unwrap();
            assert_eq!(decode(&b[..n]), Some((42, c)), "{line}");
        }
        assert_eq!(parse_line("run ticker 1 30").unwrap(), Command::Run { name: "ticker", arg0: 1, arg1: 30 });
        assert!(parse_line("kill").is_err());
        assert!(decode(&[cmd::KILL, 0, 0, 0, 0, 1]).is_none());
        assert!(decode(&[99, 0, 0, 0, 0]).is_none());
    }

    #[test]
    fn install_roundtrip() {
        let image: [u8; 600] = core::array::from_fn(|i| i as u8);
        let cmds: [Command; 5] = {
            let mut it = install_commands(&image);
            core::array::from_fn(|_| it.next().unwrap())
        };
        assert_eq!(cmds[0], Command::InstallBegin { length: 600 });
        assert_eq!(cmds[3], Command::InstallData { offset: 512, data: &image[512..] });
        assert_eq!(cmds[4], Command::InstallEnd);
        for c in cmds {
            let mut b = [0u8; MAX_COMMAND];
            let n = encode(7, &c, &mut b).unwrap();
            assert_eq!(decode(&b[..n]), Some((7, c)));
        }
        let too_big = [0u8; INSTALL_CHUNK + 1];
        assert!(encode(1, &Command::InstallData { offset: 0, data: &too_big }, &mut [0u8; 512]).is_none());
    }
}
