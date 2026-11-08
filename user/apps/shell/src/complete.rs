//! Tab completion (doc/62): the word at the end of the line, from the
//! commands (first word), the store's apps (`run`), the processes (`fg`),
//! the settings' subcommands (`cfg`), or the SD card's files (`ls`, `cat`,
//! `write`, `rm`, `mkdir`).

use t5_rt::{Files, proc};

use crate::{name_of, snapshot};

/// The commands `help` lists.
pub const COMMANDS: [&str; 22] = [
    "help", "echo", "clear", "uptime", "ls", "cat", "write", "rm", "mkdir", "run", "apps", "ps", "kill", "fg", "ping", "ifconfig", "top",
    "nc", "mt", "cfg", "exit", "more",
];

const CFG: [&str; 5] = ["list", "get", "set", "secret", "rm"];

const MAX: usize = 48;
const NAME: usize = 32;

/// The candidates for the word being typed.
pub struct Candidates {
    names: [[u8; NAME]; MAX],
    lens: [u8; MAX],
    /// A directory (completed with `/`, not a space).
    dirs: [bool; MAX],
    pub n: usize,
    /// More matched than were kept.
    pub more: bool,
}

impl Candidates {
    fn new() -> Candidates {
        Candidates { names: [[0; NAME]; MAX], lens: [0; MAX], dirs: [false; MAX], n: 0, more: false }
    }

    pub fn name(&self, i: usize) -> &str {
        core::str::from_utf8(&self.names[i][..self.lens[i] as usize]).unwrap_or("")
    }

    pub fn is_dir(&self, i: usize) -> bool {
        self.dirs[i]
    }

    /// Adds `name` if it starts with `prefix` (files: either case, as the
    /// file service makes paths upper case) and is not there yet.
    fn add(&mut self, name: &str, prefix: &str, dir: bool, any_case: bool) {
        let starts = name.len() >= prefix.len()
            && if any_case { name.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes()) } else { name.starts_with(prefix) };
        if !starts || name.is_empty() || name.len() > NAME || (0..self.n).any(|i| self.name(i) == name) {
            return;
        }
        if self.n == MAX {
            self.more = true;
            return;
        }
        self.names[self.n][..name.len()].copy_from_slice(name.as_bytes());
        self.lens[self.n] = name.len() as u8;
        self.dirs[self.n] = dir;
        self.n += 1;
    }

    /// The length of the prefix all candidates share (either case for files).
    pub fn common(&self) -> usize {
        if self.n == 0 {
            return 0;
        }
        let first = self.name(0).as_bytes();
        let mut len = first.len();
        for i in 1..self.n {
            let b = self.name(i).as_bytes();
            len = len.min(b.len());
            len = (0..len).find(|&k| !b[k].eq_ignore_ascii_case(&first[k])).unwrap_or(len);
        }
        len
    }
}

/// What completes `line` (the whole line typed so far): the candidates
/// and where the word being completed starts (after a file's directory).
pub fn candidates(line: &str) -> (Candidates, usize) {
    let mut c = Candidates::new();
    let start = line.rfind(' ').map_or(0, |i| i + 1);
    let word = &line[start..];
    let mut words = line[..start].split_whitespace();
    let Some(cmd) = words.next() else {
        for name in COMMANDS {
            c.add(name, word, false, false);
        }
        return (c, start);
    };
    let index = 1 + words.count();
    match (cmd, index) {
        ("run", 1) => {
            let mut buf = [0u8; proc::APP_RECORD * proc::MAX_APPS];
            if let Ok(len) = snapshot(proc::APPS, &mut buf) {
                for r in buf[..len].chunks(proc::APP_RECORD) {
                    c.add(name_of(&r[0..32]), word, false, false);
                }
            }
        }
        ("fg", 1) => {
            let mut buf = [0u8; proc::PROC_RECORD * proc::MAX_PROCS];
            if let Ok(len) = snapshot(proc::LIST, &mut buf) {
                for r in buf[..len].chunks(proc::PROC_RECORD) {
                    c.add(name_of(&r[16..32]), word, false, false);
                }
            }
        }
        ("cfg", 1) => {
            for name in CFG {
                c.add(name, word, false, false);
            }
        }
        ("ls" | "cat" | "rm" | "mkdir" | "more", 1) | ("write", 1) => {
            // The directory part stays as typed; the name after it is
            // completed.
            let cut = word.rfind('/').map_or(0, |i| i + 1);
            let (dir, name) = word.split_at(cut);
            if let Ok(f) = Files::connect() {
                let _ = f.list(dir.trim_end_matches('/'), |n, _, is_dir| {
                    if n != "." && n != ".." {
                        c.add(n, name, is_dir, true);
                    }
                });
            }
            return (c, start + cut);
        }
        _ => {}
    }
    (c, start)
}
