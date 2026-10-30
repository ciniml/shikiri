//! Control commands as typed on the host (doc/14 §3), turned into the
//! frames the kernel reads from the RTT down-channel. Shared by `t5log rtt`
//! and t5emu.

use t5_abi::ctl::{self, Command};

/// The frames of one command line.
pub struct Frames {
    /// COBS frames, each ending in 0x00.
    pub bytes: Vec<u8>,
    /// Request ids the kernel always answers (`ctl.done`/`ctl.failed`).
    /// `install_data` frames are answered only when they fail, and `reboot`
    /// not at all (the kernel logs `kernel.reboot` and restarts).
    pub answered: Vec<u32>,
}

/// Checks a line without reading files or numbering it.
pub fn check_line(line: &str) -> Result<(), String> {
    match line.split_whitespace().collect::<Vec<_>>()[..] {
        ["install" | "kupdate", _] => Ok(()),
        ["install", ..] => Err("install FILE".into()),
        ["kupdate", ..] => Err("kupdate FILE".into()),
        _ => ctl::parse_line(line).map(|_| ()).map_err(str::to_string),
    }
}

/// Encodes `line`, numbering its frames from `*next_id`. `install FILE`
/// reads the signed image and becomes install_begin, install_data...,
/// install_end; `kupdate FILE` the same with a T5K kernel package and
/// kupdate_begin.
pub fn frames(line: &str, next_id: &mut u32) -> Result<Frames, String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let image;
    let commands: Vec<Command> = match words[..] {
        [verb @ ("install" | "kupdate"), file] => {
            image = std::fs::read(file).map_err(|e| format!("{file}: {e}"))?;
            ctl::install_commands(&image, verb == "kupdate").collect()
        }
        _ => vec![ctl::parse_line(line)?],
    };
    let mut f = Frames { bytes: Vec::new(), answered: Vec::new() };
    for c in &commands {
        let id = *next_id;
        *next_id += 1;
        let mut raw = [0u8; ctl::MAX_COMMAND];
        let n = ctl::encode(id, c, &mut raw).ok_or(format!("{line}: too long"))?;
        let mut frame = [0u8; ctl::MAX_COMMAND + ctl::MAX_COMMAND / 254 + 2];
        let k = t5_abi::log::cobs_encode(&raw[..n], &mut frame).ok_or("COBS")?;
        f.bytes.extend_from_slice(&frame[..k]);
        if !matches!(c, Command::InstallData { .. } | Command::Reboot) {
            f.answered.push(id);
        }
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_expands() {
        let dir = std::env::temp_dir().join(format!("t5log-ctl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x.t5a");
        std::fs::write(&file, vec![0xAB; 600]).unwrap();
        let mut id = 10;
        let f = frames(&format!("install {}", file.display()), &mut id).unwrap();
        // begin, 3 data, end
        assert_eq!(id, 15);
        assert_eq!(f.answered, [10, 14]);
        assert_eq!(f.bytes.iter().filter(|&&b| b == 0).count(), 5);
        assert!(check_line("install a b").is_err());
        assert!(check_line("ps").is_ok());
        let f = frames("ps", &mut id).unwrap();
        assert_eq!(f.answered, [15]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
