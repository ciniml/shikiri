#![no_std]
#![no_main]

//! A command shell on `svc:tty` (doc/39): reads keys from the terminal,
//! edits a line (Backspace, Ctrl-U, Ctrl-C, Up/Down for history) and runs
//! built-in commands; `help` lists them.
//!
//! Built with `--permit svc:tty --permit svc:fs --permit svc:net --permit
//! proc:spawn`. Exits 0 on `exit`, or 1 if there is no terminal.

use t5_rt::{Channel, Error, Files, Handle, Net, chan, event, fs, rights, sleep_ms, spawn, time_ns};

t5_rt::entry!(main);

const LINE_MAX: usize = 160;
const HISTORY: usize = 8;
const PROMPT: &str = "t5$ ";

fn main(_: usize, _: usize) -> usize {
    let mut tty = None;
    for _ in 0..50 {
        if let Ok(c) = Channel::connect("tty") {
            tty = Some(c);
            break;
        }
        sleep_ms(100);
    }
    let Some(tty) = tty else {
        event!(Error, "shell.no_tty");
        return 1;
    };
    let mut sh = Shell { out: Out { tty, buf: [0; chan::MSG_MAX], len: 0 }, line: Line::default(), history: History::default(), esc: Esc::None };
    sh.out.str("tab5-os shell. Type help for the commands.\n");
    sh.out.str(PROMPT);
    sh.out.flush();
    let mut buf = [0u8; chan::MSG_MAX];
    loop {
        let n = match sh.out.tty.recv(&mut buf) {
            Ok(n) => n,
            Err(_) => return 0,
        };
        for &b in &buf[..n] {
            if sh.key(b) {
                sh.out.flush();
                return 0;
            }
        }
        sh.out.flush();
    }
}

/// Output to the terminal, a message at a time; `\n` becomes `\r\n`.
struct Out {
    tty: Channel,
    buf: [u8; chan::MSG_MAX],
    len: usize,
}

impl Out {
    fn byte(&mut self, b: u8) {
        if b == b'\n' {
            self.raw(b'\r');
        }
        self.raw(b);
    }

    fn raw(&mut self, b: u8) {
        if self.len == self.buf.len() {
            self.flush();
        }
        self.buf[self.len] = b;
        self.len += 1;
    }

    fn str(&mut self, s: &str) {
        for &b in s.as_bytes() {
            self.byte(b);
        }
    }

    fn num(&mut self, v: u64) {
        if v >= 10 {
            self.num(v / 10);
        }
        self.raw(b'0' + (v % 10) as u8);
    }

    fn ip(&mut self, ip: u32) {
        for (i, o) in ip.to_be_bytes().iter().enumerate() {
            if i > 0 {
                self.raw(b'.');
            }
            self.num(*o as u64);
        }
    }

    fn flush(&mut self) {
        if self.len > 0 {
            let _ = self.tty.send(&self.buf[..self.len]);
            self.len = 0;
        }
    }
}

struct Line {
    buf: [u8; LINE_MAX],
    len: usize,
}

impl Default for Line {
    fn default() -> Line {
        Line { buf: [0; LINE_MAX], len: 0 }
    }
}

impl Line {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

#[derive(Default)]
struct History {
    lines: [Line; HISTORY],
    count: usize,
    /// While browsing: how far back (0 = the line being typed).
    back: usize,
}

impl History {
    fn push(&mut self, l: &Line) {
        if l.len == 0 {
            return;
        }
        self.lines.rotate_right(1);
        self.lines[0].buf = l.buf;
        self.lines[0].len = l.len;
        self.count = (self.count + 1).min(HISTORY);
    }
}

/// Where in an escape sequence from the terminal we are (arrow keys).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Esc {
    None,
    Esc,
    Csi,
}

struct Shell {
    out: Out,
    line: Line,
    history: History,
    esc: Esc,
}

impl Shell {
    /// One byte from the terminal; true on `exit`.
    fn key(&mut self, b: u8) -> bool {
        match (self.esc, b) {
            (Esc::None, 0x1B) => self.esc = Esc::Esc,
            (Esc::Esc, b'[' | b'O') => self.esc = Esc::Csi,
            (Esc::Esc, _) => self.esc = Esc::None,
            (Esc::Csi, b'0'..=b'9' | b';') => {}
            (Esc::Csi, f) => {
                self.esc = Esc::None;
                match f {
                    b'A' => self.recall(1),
                    b'B' => self.recall(-1),
                    _ => {}
                }
            }
            (Esc::None, b'\r' | b'\n') => {
                self.out.str("\n");
                let line = core::mem::take(&mut self.line);
                self.history.push(&line);
                self.history.back = 0;
                if self.run(line.as_str()) {
                    return true;
                }
                self.out.str(PROMPT);
            }
            (Esc::None, 0x7F | 0x08) => {
                if self.line.len > 0 {
                    self.line.len -= 1;
                    self.out.str("\x08 \x08");
                }
            }
            // Ctrl-U: the whole line.
            (Esc::None, 0x15) => self.erase_line(),
            // Ctrl-C: give up the line.
            (Esc::None, 0x03) => {
                self.out.str("^C\n");
                self.line.len = 0;
                self.out.str(PROMPT);
            }
            // Ctrl-L: clear the screen.
            (Esc::None, 0x0C) => {
                self.out.str("\x1b[H\x1b[2J");
                self.out.str(PROMPT);
                let len = self.line.len;
                for i in 0..len {
                    self.out.raw(self.line.buf[i]);
                }
            }
            (Esc::None, 0x20..=0x7E) if self.line.len < LINE_MAX => {
                self.line.buf[self.line.len] = b;
                self.line.len += 1;
                self.out.raw(b);
            }
            _ => {}
        }
        false
    }

    fn erase_line(&mut self) {
        for _ in 0..self.line.len {
            self.out.str("\x08 \x08");
        }
        self.line.len = 0;
    }

    /// Up (1) or down (-1) through the history.
    fn recall(&mut self, dir: i32) {
        let back = self.history.back as i32 + dir;
        if back < 0 || back > self.history.count as i32 {
            return;
        }
        self.history.back = back as usize;
        self.erase_line();
        if back > 0 {
            let h = &self.history.lines[back as usize - 1];
            self.line.buf = h.buf;
            self.line.len = h.len;
            for i in 0..h.len {
                self.out.raw(h.buf[i]);
            }
        }
    }

    /// Runs a command line; true on `exit`.
    fn run(&mut self, line: &str) -> bool {
        let mut words = line.split_whitespace();
        let Some(cmd) = words.next() else { return false };
        let mut args = [""; 8];
        let mut n = 0;
        for w in words.by_ref().take(args.len()) {
            args[n] = w;
            n += 1;
        }
        let args = &args[..n];
        event!(Info, "shell.exec", cmd = cmd, args = n as u32);
        let o = &mut self.out;
        match cmd {
            "help" => o.str(
                "help                  this list\n\
                 echo WORDS            print the words\n\
                 clear                 clear the screen\n\
                 uptime                time since boot\n\
                 ls [DIR]              list a directory on the SD card\n\
                 cat FILE              print a file\n\
                 write FILE WORDS      write the words to a file\n\
                 rm FILE               delete a file\n\
                 mkdir DIR             make a directory\n\
                 run APP [A0 [A1]]     start an app from the store\n\
                 ping HOST             ping an address or name\n\
                 exit                  end the shell (term starts a new one)\n",
            ),
            "echo" => {
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        o.raw(b' ');
                    }
                    o.str(a);
                }
                o.str("\n");
            }
            "clear" => o.str("\x1b[H\x1b[2J"),
            "uptime" => {
                let s = time_ns() / 1_000_000_000;
                o.num(s / 3600);
                o.str("h ");
                o.num(s / 60 % 60);
                o.str("m ");
                o.num(s % 60);
                o.str("s\n");
            }
            "ls" => ls(o, args.first().copied().unwrap_or("")),
            "cat" => match args.first() {
                Some(f) => cat(o, f),
                None => o.str("usage: cat FILE\n"),
            },
            "write" => match args.split_first() {
                Some((f, words)) => write(o, f, words),
                None => o.str("usage: write FILE WORDS\n"),
            },
            "rm" | "mkdir" => match args.first() {
                Some(p) => {
                    let r = Files::connect().and_then(|f| if cmd == "rm" { f.delete(p) } else { f.mkdir(p) });
                    if r.is_err() {
                        o.str(cmd);
                        o.str(": failed\n");
                    }
                }
                None => {
                    o.str("usage: ");
                    o.str(cmd);
                    o.str(" PATH\n");
                }
            },
            "run" => match args.first() {
                Some(app) => run_app(o, app, args.get(1).and_then(|a| a.parse().ok()).unwrap_or(0), args.get(2).and_then(|a| a.parse().ok()).unwrap_or(0)),
                None => o.str("usage: run APP [A0 [A1]]\n"),
            },
            "ping" => match args.first() {
                Some(h) => ping(o, h),
                None => o.str("usage: ping HOST\n"),
            },
            "exit" => return true,
            _ => {
                o.str(cmd);
                o.str(": no such command (try help)\n");
            }
        }
        false
    }
}

fn ls(o: &mut Out, dir: &str) {
    let Ok(f) = Files::connect() else {
        o.str("ls: no file service\n");
        return;
    };
    let r = f.list(dir, |name, size, is_dir| {
        o.str(name);
        if is_dir {
            o.str("/\n");
        } else {
            o.str("  ");
            o.num(size as u64);
            o.str("\n");
        }
    });
    if r.is_err() {
        o.str("ls: cannot list ");
        o.str(dir);
        o.str("\n");
    }
}

fn cat(o: &mut Out, path: &str) {
    let Ok(mut f) = Files::connect() else {
        o.str("cat: no file service\n");
        return;
    };
    let Ok(id) = f.open(path, fs::flag::READ) else {
        o.str("cat: cannot open ");
        o.str(path);
        o.str("\n");
        return;
    };
    let mut buf = [0u8; 512];
    while let Ok(n) = f.read(id, &mut buf) {
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            o.byte(b);
        }
    }
    let _ = f.close(id);
}

fn write(o: &mut Out, path: &str, words: &[&str]) {
    let r = Files::connect().and_then(|mut f| {
        let id = f.open(path, fs::flag::WRITE | fs::flag::CREATE)?;
        for (i, w) in words.iter().enumerate() {
            if i > 0 {
                f.write(id, b" ")?;
            }
            f.write(id, w.as_bytes())?;
        }
        f.write(id, b"\n")?;
        f.close(id)
    });
    if r.is_err() {
        o.str("write: failed\n");
    }
}

fn run_app(o: &mut Out, app: &str, a0: u32, a1: u32) {
    let mut req = [0u8; spawn::ARGS + spawn::NAME_MAX];
    if app.len() > spawn::NAME_MAX {
        o.str("run: name too long\n");
        return;
    }
    req[0..4].copy_from_slice(&a0.to_le_bytes());
    req[4..8].copy_from_slice(&a1.to_le_bytes());
    req[8..8 + app.len()].copy_from_slice(app.as_bytes());
    match Handle::open(spawn::NAME, rights::WRITE).and_then(|h| h.write(&req[..8 + app.len()])) {
        // The kernel starts it on its next round; the log says how it went.
        Ok(_) => {
            o.str(app);
            o.str(": started\n");
        }
        Err(Error::Busy) => o.str("run: busy, try again\n"),
        Err(_) => o.str("run: failed\n"),
    }
}

fn parse_ip(s: &str) -> Option<u32> {
    let mut ip = 0u32;
    let mut parts = 0;
    for p in s.split('.') {
        ip = ip << 8 | p.parse::<u8>().ok()? as u32;
        parts += 1;
    }
    (parts == 4).then_some(ip)
}

fn ping(o: &mut Out, host: &str) {
    let Ok(mut net) = Net::connect() else {
        o.str("ping: no network service\n");
        return;
    };
    let ip = match parse_ip(host) {
        Some(ip) => ip,
        None => match net.resolve(host, 3000) {
            Ok(ip) => ip,
            Err(_) => {
                o.str("ping: cannot resolve ");
                o.str(host);
                o.str("\n");
                return;
            }
        },
    };
    for _ in 0..4 {
        o.str("ping ");
        o.ip(ip);
        match net.ping(ip, 2000) {
            Ok(us) => {
                o.str(": ");
                o.num(us as u64 / 1000);
                o.raw(b'.');
                o.num(us as u64 / 100 % 10);
                o.str(" ms\n");
            }
            Err(_) => o.str(": no answer\n"),
        }
        o.flush();
        sleep_ms(500);
    }
}
