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
//!
//! A kernel update (M5a, doc/16 §3) is the same with `kupdate_begin` in
//! place of `install_begin`: the data and end frames go to whichever
//! session was begun last. `reboot` restarts the chip.

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
    /// `u32 length`: a T5K kernel package follows (doc/16 §2).
    pub const KUPDATE_BEGIN: u8 = 11;
    pub const REBOOT: u8 = 12;
    /// Logs the kernel slots and otadata.
    pub const KINFO: u8 = 13;
    /// `u32 mhz`: CPU clock (90 or 360 on the P4), for experiments.
    pub const CLOCK: u8 = 14;
    /// `u32 on`: automatic voltage adjustment (PVT) on or off.
    pub const PVT: u8 = 15;
    /// Logs the core voltage settings.
    pub const POWER: u8 = 16;
    /// `u8 name_len | name`: go back to the previous version (doc/16 §4.2).
    pub const ROLLBACK: u8 = 17;
    /// `u32 seconds`: log touch points for that long (M6a).
    pub const TOUCH: u8 = 18;
    /// Log every address on the board I2C bus that answers.
    pub const I2C_SCAN: u8 = 19;
    /// Log the display's frame count and rate (M6b).
    pub const DISPLAY: u8 = 20;
    /// `u32 degrees` (0, 90, 180, 270): the screen orientation (M6c).
    pub const ROTATE: u8 = 21;
    /// Initialise the SD card and log its partitions (M7a).
    pub const SD_INFO: u8 = 22;
    /// `u32 lba`: log one SD sector.
    pub const SD_READ: u8 = 23;
    /// Write/read-back test and speed of the SD card (M7b).
    pub const SD_TEST: u8 = 24;
    /// Power the Wi-Fi chip and identify it on SDIO (N2b-1, doc/22).
    pub const WLAN_PROBE: u8 = 25;
    /// `u32 op | u32 a | u32 b`: the sampling profiler (doc/34). op 1:
    /// start on pid `a` every `b` µs; 2: stop and log the summary; 3: log
    /// samples part `a`.
    pub const PROF: u8 = 26;
    /// `u32 pause`: memory bandwidth on a free slot, also with the frame
    /// DMA stopped if `pause` (doc/43).
    pub const MEMBENCH: u8 = 27;
    /// Fill, glyph and copy with the PPA and 2D-DMA on a free slot (doc/44).
    pub const PPA_BENCH: u8 = 28;
    /// `u32 raw, u32 us`: fills 2 MiB of a free slot with random words, or
    /// with `raw` the generator's register read once every `us` µs (0: back
    /// to back), for a debug probe to read (doc/47 §5). The result is the
    /// buffer's address.
    pub const RNG_DUMP: u8 = 29;
}

/// `Command::Prof` operations.
pub mod prof {
    pub const START: u32 = 1;
    pub const STOP: u32 = 2;
    pub const DUMP: u32 = 3;
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
    Rollback { name: &'a str },
    FlashTest,
    Psram,
    KupdateBegin { length: u32 },
    Reboot,
    Kinfo,
    Clock { mhz: u32 },
    Pvt { on: bool },
    Power,
    Touch { seconds: u32 },
    I2cScan,
    Display,
    Rotate { degrees: u32 },
    SdInfo,
    SdRead { lba: u32 },
    SdTest,
    WlanProbe,
    Prof { op: u32, a: u32, b: u32 },
    Membench { pause: bool },
    PpaBench,
    RngDump { raw: bool, us: u32 },
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
            Command::Rollback { .. } => "rollback",
            Command::FlashTest => "flashtest",
            Command::Psram => "psram",
            Command::KupdateBegin { .. } => "kupdate_begin",
            Command::Reboot => "reboot",
            Command::Kinfo => "kinfo",
            Command::Clock { .. } => "clock",
            Command::Pvt { .. } => "pvt",
            Command::Power => "power",
            Command::Touch { .. } => "touch",
            Command::I2cScan => "i2cscan",
            Command::Display => "display",
            Command::Rotate { .. } => "rotate",
            Command::SdInfo => "sdinfo",
            Command::SdRead { .. } => "sdread",
            Command::SdTest => "sdtest",
            Command::WlanProbe => "wlanprobe",
            Command::Prof { .. } => "prof",
            Command::Membench { .. } => "membench",
            Command::PpaBench => "ppabench",
            Command::RngDump { .. } => "rngdump",
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
        cmd::KUPDATE_BEGIN if args.len() == 4 => Command::KupdateBegin { length: u32_at(args, 0)? },
        cmd::REBOOT if args.is_empty() => Command::Reboot,
        cmd::KINFO if args.is_empty() => Command::Kinfo,
        cmd::CLOCK if args.len() == 4 => Command::Clock { mhz: u32_at(args, 0)? },
        cmd::PVT if args.len() == 4 => Command::Pvt { on: u32_at(args, 0)? != 0 },
        cmd::POWER if args.is_empty() => Command::Power,
        cmd::TOUCH if args.len() == 4 => Command::Touch { seconds: u32_at(args, 0)? },
        cmd::I2C_SCAN if args.is_empty() => Command::I2cScan,
        cmd::DISPLAY if args.is_empty() => Command::Display,
        cmd::ROTATE if args.len() == 4 => Command::Rotate { degrees: u32_at(args, 0)? },
        cmd::SD_INFO if args.is_empty() => Command::SdInfo,
        cmd::SD_READ if args.len() == 4 => Command::SdRead { lba: u32_at(args, 0)? },
        cmd::SD_TEST if args.is_empty() => Command::SdTest,
        cmd::WLAN_PROBE if args.is_empty() => Command::WlanProbe,
        cmd::PPA_BENCH if args.is_empty() => Command::PpaBench,
        cmd::MEMBENCH if args.len() == 4 => Command::Membench { pause: u32_at(args, 0)? != 0 },
        cmd::RNG_DUMP if args.len() == 8 => Command::RngDump { raw: u32_at(args, 0)? != 0, us: u32_at(args, 4)? },
        cmd::PROF if args.len() == 12 => Command::Prof { op: u32_at(args, 0)?, a: u32_at(args, 4)?, b: u32_at(args, 8)? },
        cmd::REMOVE | cmd::ROLLBACK => {
            let n = *args.first()? as usize;
            if args.len() != 1 + n {
                return None;
            }
            let name = core::str::from_utf8(&args[1..]).ok()?;
            if b[0] == cmd::REMOVE { Command::Remove { name } } else { Command::Rollback { name } }
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
        Command::Rollback { .. } => put(&[cmd::ROLLBACK])?,
        Command::FlashTest => put(&[cmd::FLASH_TEST])?,
        Command::Psram => put(&[cmd::PSRAM])?,
        Command::KupdateBegin { .. } => put(&[cmd::KUPDATE_BEGIN])?,
        Command::Reboot => put(&[cmd::REBOOT])?,
        Command::Kinfo => put(&[cmd::KINFO])?,
        Command::Clock { .. } => put(&[cmd::CLOCK])?,
        Command::Pvt { .. } => put(&[cmd::PVT])?,
        Command::Power => put(&[cmd::POWER])?,
        Command::Touch { .. } => put(&[cmd::TOUCH])?,
        Command::I2cScan => put(&[cmd::I2C_SCAN])?,
        Command::Display => put(&[cmd::DISPLAY])?,
        Command::Rotate { .. } => put(&[cmd::ROTATE])?,
        Command::SdInfo => put(&[cmd::SD_INFO])?,
        Command::SdRead { .. } => put(&[cmd::SD_READ])?,
        Command::SdTest => put(&[cmd::SD_TEST])?,
        Command::WlanProbe => put(&[cmd::WLAN_PROBE])?,
        Command::Prof { .. } => put(&[cmd::PROF])?,
        Command::Membench { .. } => put(&[cmd::MEMBENCH])?,
        Command::PpaBench => put(&[cmd::PPA_BENCH])?,
        Command::RngDump { .. } => put(&[cmd::RNG_DUMP])?,
    }
    put(&id.to_le_bytes())?;
    match c {
        Command::Kill { pid } => put(&pid.to_le_bytes())?,
        Command::Clock { mhz } => put(&mhz.to_le_bytes())?,
        Command::Touch { seconds } => put(&seconds.to_le_bytes())?,
        Command::Rotate { degrees } => put(&degrees.to_le_bytes())?,
        Command::SdRead { lba } => put(&lba.to_le_bytes())?,
        Command::Pvt { on } => put(&(*on as u32).to_le_bytes())?,
        Command::Membench { pause } => put(&(*pause as u32).to_le_bytes())?,
        Command::RngDump { raw, us } => {
            put(&(*raw as u32).to_le_bytes())?;
            put(&us.to_le_bytes())?;
        }
        Command::Prof { op, a, b } => {
            put(&op.to_le_bytes())?;
            put(&a.to_le_bytes())?;
            put(&b.to_le_bytes())?;
        }
        Command::Run { name, arg0, arg1 } => {
            if name.len() > 32 {
                return None;
            }
            put(&[name.len() as u8])?;
            put(name.as_bytes())?;
            put(&arg0.to_le_bytes())?;
            put(&arg1.to_le_bytes())?;
        }
        Command::InstallBegin { length } | Command::KupdateBegin { length } => put(&length.to_le_bytes())?,
        Command::InstallData { offset, data } => {
            if data.is_empty() || data.len() > INSTALL_CHUNK {
                return None;
            }
            put(&offset.to_le_bytes())?;
            put(data)?;
        }
        Command::Remove { name } | Command::Rollback { name } => {
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
/// `run NAME [ARG0 [ARG1]]`, `remove NAME`, `rollback NAME`, `flashtest`, `psram`, `reboot`,
/// `kinfo`. (`install FILE` and `kupdate FILE` read a file, so the host
/// tools expand them; see `install_commands`.)
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
        Some("rollback") => Command::Rollback { name: w.next().ok_or("rollback NAME")? },
        Some("flashtest") => Command::FlashTest,
        Some("psram") => Command::Psram,
        Some("reboot") => Command::Reboot,
        Some("kinfo") => Command::Kinfo,
        Some("clock") => Command::Clock { mhz: num(Some(w.next().ok_or("clock MHZ")?))? },
        Some("pvt") => match w.next() {
            Some("on") => Command::Pvt { on: true },
            Some("off") => Command::Pvt { on: false },
            _ => return Err("pvt on|off"),
        },
        Some("power") => Command::Power,
        Some("touch") => Command::Touch { seconds: num(Some(w.next().ok_or("touch SECONDS")?))? },
        Some("i2cscan") => Command::I2cScan,
        Some("display") => Command::Display,
        Some("sdinfo") => Command::SdInfo,
        Some("sdtest") => Command::SdTest,
        Some("wlanprobe") => Command::WlanProbe,
        Some("sdread") => Command::SdRead { lba: num(Some(w.next().ok_or("sdread LBA")?))? },
        Some("rotate") => Command::Rotate { degrees: num(Some(w.next().ok_or("rotate 0|90|180|270")?))? },
        Some("ppabench") => Command::PpaBench,
        Some("membench") => match w.next() {
            None => Command::Membench { pause: false },
            Some("pause") => Command::Membench { pause: true },
            _ => return Err("membench [pause]"),
        },
        Some("rngdump") => match w.next() {
            None => Command::RngDump { raw: false, us: 0 },
            Some("raw") => Command::RngDump { raw: true, us: num(w.next())? },
            _ => return Err("rngdump [raw [US]]"),
        },
        Some("prof") => match w.next() {
            Some("start") => {
                let pid = num(Some(w.next().ok_or("prof start PID US")?))?;
                Command::Prof { op: prof::START, a: pid, b: num(Some(w.next().ok_or("prof start PID US")?))? }
            }
            Some("stop") => Command::Prof { op: prof::STOP, a: 0, b: 0 },
            Some("dump") => Command::Prof { op: prof::DUMP, a: num(Some(w.next().ok_or("prof dump PART")?))?, b: 0 },
            _ => return Err("prof start PID US | prof stop | prof dump PART"),
        },
        _ => {
            return Err("commands: list | ps | run NAME [ARG0 [ARG1]] | kill PID | install FILE | remove NAME | rollback NAME | \
                 flashtest | psram | kupdate FILE | kinfo | reboot | clock MHZ | pvt on|off | power | touch SECONDS | i2cscan | display | rotate DEGREES | sdinfo | sdread LBA | sdtest | wlanprobe | prof start PID US | prof stop | prof dump PART | membench [pause] | ppabench | rngdump [raw [US]]");
        }
    };
    if w.next().is_some() {
        return Err("too many arguments");
    }
    Ok(c)
}

/// The commands that install `image`: begin, one data frame per
/// `INSTALL_CHUNK`, end. With `kernel`, `image` is a T5K kernel package.
pub fn install_commands(image: &[u8], kernel: bool) -> impl Iterator<Item = Command<'_>> {
    let length = image.len() as u32;
    core::iter::once(if kernel { Command::KupdateBegin { length } } else { Command::InstallBegin { length } })
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
        for line in ["list", "ps", "kill 3", "run ticker 1 0x1e", "remove hello", "rollback hello", "flashtest", "reboot", "kinfo", "clock 90", "pvt off", "pvt on", "power", "touch 5", "i2cscan", "display", "rotate 180", "sdinfo", "sdread 0x800", "sdtest", "wlanprobe", "prof start 104 500", "prof stop", "prof dump 3"] {
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
            let mut it = install_commands(&image, false);
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
        assert_eq!(install_commands(&image, true).next(), Some(Command::KupdateBegin { length: 600 }));
        let mut b = [0u8; MAX_COMMAND];
        let n = encode(8, &Command::KupdateBegin { length: 600 }, &mut b).unwrap();
        assert_eq!(decode(&b[..n]), Some((8, Command::KupdateBegin { length: 600 })));
        let too_big = [0u8; INSTALL_CHUNK + 1];
        assert!(encode(1, &Command::InstallData { offset: 0, data: &too_big }, &mut [0u8; 512]).is_none());
    }
}
