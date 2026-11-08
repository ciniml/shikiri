//! Tab completion (doc/62): the word at the end of the line, from the
//! commands (first word), the store's apps (`run`), the processes (`fg`),
//! the settings' subcommands (`cfg`), or the SD card's files (`ls`, `cat`,
//! `write`, `rm`, `mkdir`).

use t5_rt::{Files, proc};

use crate::{name_of, snapshot};

/// The commands `help` lists.
pub const COMMANDS: [&str; 23] = [
    "help", "echo", "clear", "uptime", "ls", "cat", "write", "rm", "mkdir", "run", "apps", "ps", "kill", "fg", "ping", "ifconfig", "top",
    "nc", "mt", "cfg", "exit", "more", "ota",
];

const CFG: [&str; 5] = ["list", "get", "set", "secret", "rm"];
const OTA: [&str; 3] = ["app", "kernel", "reboot"];

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
        // Not inside a character.
        while !self.name(0).is_char_boundary(len) {
            len -= 1;
        }
        len
    }
}

/// Where the last word of `line` starts: after the last space that is
/// neither escaped (`\ `) nor in double quotes.
pub fn last_word(line: &str) -> usize {
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (i, b) in line.bytes().enumerate() {
        match b {
            _ if escaped => escaped = false,
            b'\\' => escaped = true,
            b'"' => quoted = !quoted,
            b' ' if !quoted => start = i + 1,
            _ => {}
        }
    }
    start
}

/// Splits `line` into words at spaces, honouring `\ ` and double quotes
/// (doc/63): the words, unescaped into `buf`, as ranges of it in `out`.
/// Returns how many (at most `out.len()`).
pub fn split(line: &str, buf: &mut [u8; crate::LINE_MAX], out: &mut [(usize, usize)]) -> usize {
    let (mut n, mut words) = (0, 0);
    let (mut quoted, mut escaped) = (false, false);
    // Where the word being read starts in `buf`, if one is.
    let mut start: Option<usize> = None;
    for b in line.bytes() {
        if !escaped && !quoted && b == b' ' {
            if let Some(s) = start.take()
                && words < out.len()
            {
                out[words] = (s, n);
                words += 1;
            }
            continue;
        }
        start.get_or_insert(n);
        match b {
            _ if escaped => escaped = false,
            b'\\' => {
                escaped = true;
                continue;
            }
            b'"' => {
                quoted = !quoted;
                continue;
            }
            _ => {}
        }
        if n < buf.len() {
            buf[n] = b;
            n += 1;
        }
    }
    if let Some(s) = start
        && words < out.len()
    {
        out[words] = (s, n);
        words += 1;
    }
    words
}

/// `word` without its escapes and quotes, in `buf`.
pub fn unescape<'b>(word: &str, buf: &'b mut [u8; crate::LINE_MAX]) -> &'b str {
    let mut n = 0;
    let mut escaped = false;
    for b in word.bytes() {
        match b {
            _ if escaped => {
                escaped = false;
                buf[n] = b;
                n += 1;
            }
            b'\\' => escaped = true,
            b'"' => {}
            _ => {
                buf[n] = b;
                n += 1;
            }
        }
    }
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

/// What completes `line` (the whole line typed so far): the candidates
/// and where the word being completed starts in `line` (after a file's
/// directory).
pub fn candidates(line: &str) -> (Candidates, usize) {
    let mut c = Candidates::new();
    let start = last_word(line);
    let mut buf = [0u8; crate::LINE_MAX];
    let word = unescape(&line[start..], &mut buf);
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
        ("ota", 1) => {
            for name in OTA {
                c.add(name, word, false, false);
            }
        }
        ("ls" | "cat" | "rm" | "mkdir" | "more", 1) | ("write", 1) | ("ota", 2) => {
            // The directory part stays as typed; the name after it is
            // completed.
            let cut = word.rfind('/').map_or(0, |i| i + 1);
            let (dir, name) = word.split_at(cut);
            // `/` is never escaped: the same place in the line as typed.
            let raw_cut = line[start..].rfind('/').map_or(0, |i| i + 1);
            if let Ok(f) = Files::connect() {
                let _ = f.list(dir.trim_end_matches('/'), |n, _, is_dir| {
                    if n != "." && n != ".." {
                        c.add(n, name, is_dir, true);
                    }
                });
            }
            return (c, start + raw_cut);
        }
        _ => {}
    }
    (c, start)
}
