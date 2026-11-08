#![no_std]
#![no_main]

//! A command shell on `svc:tty` (doc/39): reads keys from the terminal,
//! edits a line (Backspace, Ctrl-U, Ctrl-C, Up/Down for history, Tab to
//! complete, doc/62) and runs built-in commands; `help` lists them.
//!
//! Built with `--permit svc:tty --permit svc:fs --permit svc:net --permit
//! proc:spawn --permit proc:list --permit proc:apps --permit proc:kill`.
//! Exits 0 on `exit`, or 1 if there is no terminal.

use t5_rt::{CfgError, Channel, Config, Error, Files, Handle, Net, chan, display, event, fs, net, proc, rights, sleep_ms, spawn, time_ns, wait_raw};

t5_rt::entry!(main);

mod complete;

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

    /// `s`, then spaces up to `width` columns.
    fn col(&mut self, s: &str, width: usize) {
        self.str(s);
        for _ in s.len()..width {
            self.raw(b' ');
        }
    }

    /// `v` right-aligned in `width` columns.
    fn num_col(&mut self, v: u64, width: usize) {
        let digits = (v.max(1).ilog10() + 1) as usize;
        for _ in digits..width {
            self.raw(b' ');
        }
        self.num(v);
    }

    fn hex2(&mut self, b: u8) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        self.raw(HEX[(b >> 4) as usize]);
        self.raw(HEX[(b & 15) as usize]);
    }

    /// An IPv6 address, the longest run of zero groups as `::`.
    fn ip6(&mut self, a: [u8; 16]) {
        let g = |i: usize| u16::from_be_bytes([a[2 * i], a[2 * i + 1]]);
        let (mut best, mut best_len, mut i) = (8, 0, 0);
        while i < 8 {
            let start = i;
            while i < 8 && g(i) == 0 {
                i += 1;
            }
            if i - start > best_len && i - start > 1 {
                (best, best_len) = (start, i - start);
            }
            i += 1;
        }
        let mut i = 0;
        while i < 8 {
            if i == best {
                self.str("::");
                i += best_len;
                continue;
            }
            if i > 0 && i != best + best_len {
                self.raw(b':');
            }
            let v = g(i);
            let mut started = false;
            for shift in [12, 8, 4, 0] {
                let d = (v >> shift) & 15;
                if d != 0 || started || shift == 0 {
                    self.raw(b"0123456789abcdef"[d as usize]);
                    started = true;
                }
            }
            i += 1;
        }
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

#[derive(Clone, Copy)]
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
                    // A whole character (completion may have put UTF-8 in).
                    self.line.len -= 1;
                    while self.line.len > 0 && self.line.buf[self.line.len] & 0xC0 == 0x80 {
                        self.line.len -= 1;
                    }
                    self.out.str("\x08 \x08");
                }
            }
            // Ctrl-U: the whole line.
            (Esc::None, 0x15) => self.erase_line(),
            (Esc::None, b'\t') => self.complete(),
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

    /// Tab (doc/62): completes the word at the end of the line, as far as
    /// the candidates agree; lists them when that adds nothing.
    fn complete(&mut self) {
        let line = self.line;
        let text = line.as_str();
        let (c, start) = complete::candidates(text);
        let word = &text[start..];
        let mut ubuf = [0u8; LINE_MAX];
        let typed = complete::unescape(word, &mut ubuf).len();
        if c.n == 0 {
            return;
        }
        let (take, end) = if c.n == 1 { (c.name(0).len(), Some(if c.is_dir(0) { b'/' } else { b' ' })) } else { (c.common(), None) };
        if take > typed || end.is_some() {
            // The candidate's spelling (a file's case) replaces the word,
            // a space in it escaped.
            for _ in 0..word.chars().count() {
                self.out.str("\x08 \x08");
            }
            self.line.len = start;
            let name = c.name(0).as_bytes();
            for &b in &name[..take] {
                if b == b' ' {
                    self.put(b'\\');
                }
                self.put(b);
            }
            if let Some(e) = end {
                self.put(e);
            }
            return;
        }
        // Nothing to add: show them, then the line again.
        self.out.str("\n");
        let mut col = 0;
        for i in 0..c.n {
            let w = c.name(i).len() + c.is_dir(i) as usize + 2;
            if col > 0 && col + w > 80 {
                self.out.str("\n");
                col = 0;
            }
            self.out.str(c.name(i));
            self.out.str(if c.is_dir(i) { "/  " } else { "  " });
            col += w;
        }
        if c.more {
            self.out.str("...");
        }
        self.out.str("\n");
        self.out.str(PROMPT);
        self.out.str(text);
    }

    /// Adds `b` to the line and shows it.
    fn put(&mut self, b: u8) {
        if self.line.len < LINE_MAX {
            self.line.buf[self.line.len] = b;
            self.line.len += 1;
            self.out.raw(b);
        }
    }

    fn erase_line(&mut self) {
        for _ in 0..self.line.as_str().chars().count() {
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
        // Words at spaces; `\ ` and "..." keep spaces in one (doc/63).
        let mut buf = [0u8; LINE_MAX];
        let mut ranges = [(0, 0); 9];
        let count = complete::split(line, &mut buf, &mut ranges);
        let mut all = [""; 9];
        for (w, &(a, b)) in all.iter_mut().zip(&ranges[..count]) {
            *w = core::str::from_utf8(&buf[a..b]).unwrap_or("");
        }
        let Some((&cmd, args)) = all[..count].split_first() else { return false };
        let n = args.len();
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
                 apps                  list the apps in the store\n\
                 ps                    list the processes\n\
                 kill PID              stop a process\n\
                 fg PID|APP            put its screen in front\n\
                 ping HOST             ping an address or name\n\
                 ifconfig              the network interfaces\n\
                 top                   CPU use per process (any key ends)\n\
                 nc HOST PORT          a TCP connection, a line at a time\n\
                                       (Ctrl-C or Ctrl-D ends)\n\
                 mt REQUEST            ask the Matter service (mt help)\n\
                 cfg [list]            the settings (secrets hidden)\n\
                 cfg get KEY           one setting\n\
                 cfg set KEY [VALUE]   change a setting (words joined by a space)\n\
                 cfg secret KEY VALUE  a setting only wifi may read back\n\
                 cfg rm KEY            delete a setting\n\
                 more FILE             a file a screen at a time\n\
                 exit                  end the shell (term starts a new one)\n\
                 Tab completes commands, apps (run), processes (fg) and files.\n\
                 A space in a name: \\  or \"...\" (cat My\\ File.txt).\n",
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
            "more" => match args.first() {
                Some(f) => more(o, f),
                None => o.str("usage: more FILE\n"),
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
            "ps" => ps(o),
            "top" => top(o),
            "ifconfig" => ifconfig(o),
            "nc" => match (args.first(), args.get(1).and_then(|p| p.parse::<u16>().ok())) {
                (Some(h), Some(port)) => nc(o, h, port),
                _ => o.str("usage: nc HOST PORT\n"),
            },
            "apps" => apps(o),
            "kill" => match args.first().and_then(|a| a.parse::<u32>().ok()) {
                Some(pid) => kill(o, pid),
                None => o.str("usage: kill PID\n"),
            },
            "fg" => match args.first() {
                Some(a) => fg(o, a),
                None => o.str("usage: fg PID|APP\n"),
            },
            "ping" => match args.first() {
                Some(h) => ping(o, h),
                None => o.str("usage: ping HOST\n"),
            },
            "mt" => mt(o, args),
            "cfg" => cfg(o, args),
            "exit" => return true,
            _ => {
                o.str(cmd);
                o.str(": no such command (try help)\n");
            }
        }
        false
    }
}

/// `cfg ...`: the settings service (doc/50).
fn cfg(o: &mut Out, args: &[&str]) {
    let Ok(c) = Config::connect() else {
        o.str("cfg: no settings service (run cfg)\n");
        return;
    };
    // The value: the words after the key, joined by a space.
    let mut value = [0u8; t5_rt::cfg::VALUE_MAX];
    let mut n = 0;
    for (i, w) in args.iter().skip(2).enumerate() {
        let sep = (i > 0) as usize;
        if n + sep + w.len() > value.len() {
            o.str("cfg: the value is too long\n");
            return;
        }
        value[n] = b' ';
        value[n + sep..n + sep + w.len()].copy_from_slice(w.as_bytes());
        n += sep + w.len();
    }
    let mut v = [0u8; t5_rt::cfg::VALUE_MAX];
    let r = match (args.first().copied().unwrap_or("list"), args.get(1).copied()) {
        ("list", _) => {
            let mut key = [0u8; t5_rt::cfg::KEY_MAX];
            let mut r = Ok(());
            for i in 0..=u8::MAX {
                match c.entry(i, &mut key, &mut v) {
                    Ok(Some((k, g))) => {
                        o.str(core::str::from_utf8(&key[..k]).unwrap_or("?"));
                        show(o, g, &v);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        r = Err(e);
                        break;
                    }
                }
            }
            r
        }
        ("get", Some(k)) => match c.get(k, &mut v) {
            Ok(Some(g)) => {
                o.str(k);
                show(o, g, &v);
                Ok(())
            }
            Ok(None) => {
                o.str("cfg: no such setting\n");
                Ok(())
            }
            Err(e) => Err(e),
        },
        ("set", Some(k)) => c.set(k, &value[..n], false),
        ("secret", Some(k)) if n > 0 => c.set(k, &value[..n], true),
        ("rm", Some(k)) => c.remove(k),
        _ => {
            o.str("usage: cfg [list] | get KEY | set KEY [VALUE] | secret KEY VALUE | rm KEY\n");
            Ok(())
        }
    };
    if let Err(e) = r {
        use t5_rt::cfg::err;
        o.str(match e {
            CfgError::Code(err::BAD) => "cfg: a key is a-z 0-9 . _ - only\n",
            CfgError::Code(err::TOO_LONG) => "cfg: too long\n",
            CfgError::Code(err::FULL) => "cfg: no room left\n",
            CfgError::Code(err::NOT_SAVED) => "cfg: changed, but not saved\n",
            CfgError::Code(err::DENIED) => "cfg: this app may not use that key\n",
            _ => "cfg: the service failed\n",
        });
    }
}

/// ` = VALUE`, or ` (secret)`.
fn show(o: &mut Out, g: t5_rt::CfgValue, v: &[u8]) {
    if g.flags & t5_rt::cfg::SECRET != 0 {
        o.str(" (secret)\n");
    } else {
        o.str(" = ");
        o.str(core::str::from_utf8(&v[..g.len]).unwrap_or("?"));
        o.str("\n");
    }
}

/// `mt REQUEST`: one request to the Matter service (doc/49 MT2), its
/// answer printed line by line.
fn mt(o: &mut Out, args: &[&str]) {
    let mut req = [0u8; 120];
    let mut n = 0;
    for (i, a) in args.iter().enumerate() {
        let sep = (i > 0) as usize;
        if n + sep + a.len() > req.len() {
            break;
        }
        if sep == 1 {
            req[n] = b' ';
        }
        req[n + sep..n + sep + a.len()].copy_from_slice(a.as_bytes());
        n += sep + a.len();
    }
    let Ok(c) = Channel::connect(t5_rt::matter::SVC) else {
        o.str("mt: no Matter service (run matter)\n");
        return;
    };
    if c.send(&req[..n]).is_err() {
        o.str("mt: failed\n");
        return;
    }
    let mut m = [0u8; 128];
    loop {
        match c.recv(&mut m) {
            Ok(k) if k >= 1 && m[0] == t5_rt::matter::LINE => {
                o.str(core::str::from_utf8(&m[1..k]).unwrap_or("?"));
                o.str("\n");
            }
            Ok(k) if k >= 1 && m[0] == t5_rt::matter::END => return,
            Ok(_) => {}
            Err(_) => {
                o.str("mt: the service went away\n");
                return;
            }
        }
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
    let mut total = 0;
    while let Ok(n) = f.read(id, &mut buf) {
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            o.byte(b);
        }
        total += n;
    }
    let _ = f.close(id);
    event!(Info, "shell.cat", bytes = total as u32);
}

/// Rows `more` shows before it asks: the terminal's, but the prompt's.
const MORE_ROWS: usize = 21;

/// `more FILE` (doc/62): a screen at a time. Space shows the next screen,
/// Enter the next line, q or Ctrl-C stops.
fn more(o: &mut Out, path: &str) {
    let Ok(mut f) = Files::connect() else {
        o.str("more: no file service\n");
        return;
    };
    let Ok(id) = f.open(path, fs::flag::READ) else {
        o.str("more: cannot open ");
        o.str(path);
        o.str("\n");
        return;
    };
    let mut buf = [0u8; 512];
    // Rows still to show before asking, and the column (lines wrap at 80).
    let (mut left, mut col) = (MORE_ROWS, 0);
    'file: while let Ok(n) = f.read(id, &mut buf) {
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            if left == 0 {
                o.str("\x1b[7m--More-- (space, Enter, q)\x1b[m");
                o.flush();
                let mut k = [0u8; chan::MSG_MAX];
                let key = loop {
                    match keys(o, &mut k, 1000) {
                        Ok(0) => {}
                        Ok(_) => break k[0],
                        Err(_) => break b'q',
                    }
                };
                o.str("\r\x1b[K");
                match key {
                    b' ' => left = MORE_ROWS,
                    b'\r' | b'\n' => left = 1,
                    _ => break 'file,
                }
            }
            o.byte(b);
            if b == b'\n' {
                (left, col) = (left - 1, 0);
            } else if b != b'\r' {
                col += 1;
                if col == 80 {
                    (left, col) = (left - 1, 0);
                }
            }
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

/// `host`'s IPv4 address: as written, or from DNS.
fn lookup(o: &mut Out, net: &mut Net, cmd: &str, host: &str) -> Option<u32> {
    if let Some(ip) = parse_ip(host) {
        return Some(ip);
    }
    let r = net.resolve(host, 3000).ok();
    if r.is_none() {
        o.str(cmd);
        o.str(": cannot resolve ");
        o.str(host);
        o.str("\n");
    }
    r
}

fn ping(o: &mut Out, host: &str) {
    let Ok(mut net) = Net::connect() else {
        o.str("ping: no network service\n");
        return;
    };
    let Some(ip) = lookup(o, &mut net, "ping", host) else { return };
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

/// A snapshot from `proc:list` or `proc:apps` into `buf`: its length.
fn snapshot(name: &str, buf: &mut [u8]) -> Result<usize, Error> {
    let h = Handle::open(name, rights::READ)?;
    // The kernel takes it on its next service round.
    for _ in 0..100 {
        match h.read(buf) {
            Err(Error::Busy) => sleep_ms(10),
            r => return r,
        }
    }
    Err(Error::Busy)
}

fn word(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// A `proc:list` record's memory in KiB: text in RAM, data, bss, stack.
fn ram_kb(r: &[u8]) -> u64 {
    u16::from_le_bytes([r[6], r[7]]) as u64 + word(r, 12).div_ceil(1024) as u64
}

/// `proc:mem` (doc/61): the app pool, a slot, the slots, shared memory,
/// the display, the PSRAM, the internal RAM, the kernel's image, its stacks,
/// the network rings (bytes).
fn mem_info() -> Option<[u32; proc::MEM_INFO / 4]> {
    let h = Handle::open(proc::MEM, rights::READ).ok()?;
    let mut b = [0u8; proc::MEM_INFO];
    h.read(&mut b).ok()?;
    let mut v = [0u32; proc::MEM_INFO / 4];
    for (o, c) in v.iter_mut().zip(b.chunks(4)) {
        *o = word(c, 0);
    }
    Some(v)
}

fn name_of(b: &[u8]) -> &str {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    core::str::from_utf8(&b[..n]).unwrap_or("?")
}

fn ps(o: &mut Out) {
    let mut buf = [0u8; proc::PROC_RECORD * proc::MAX_PROCS];
    let Ok(len) = snapshot(proc::LIST, &mut buf) else {
        o.str("ps: cannot list the processes\n");
        return;
    };
    o.str("  PID NAME             STATE     HANDLES    RAM KB    CPU s\n");
    for r in buf[..len].chunks(proc::PROC_RECORD) {
        o.num_col(word(r, 0) as u64, 5);
        o.raw(b' ');
        o.col(name_of(&r[16..32]), 17);
        o.col(
            match r[4] {
                proc::STATE_READY => "ready",
                proc::STATE_SLEEPING => "sleeping",
                proc::STATE_WAITING => "waiting",
                _ => "exited",
            },
            9,
        );
        o.num_col(r[5] as u64, 8);
        o.num_col(ram_kb(r), 10);
        let ms = word(r, 8) as u64;
        o.num_col(ms / 1000, 7);
        o.raw(b'.');
        o.num(ms / 100 % 10);
        o.str("\n");
    }
    event!(Info, "shell.listed", what = "ps", count = (len / proc::PROC_RECORD) as u32);
}

fn apps(o: &mut Out) {
    let mut buf = [0u8; proc::APP_RECORD * proc::MAX_APPS];
    let Ok(len) = snapshot(proc::APPS, &mut buf) else {
        o.str("apps: cannot list the store\n");
        return;
    };
    o.str("NAME             VERSION     SIZE KB  NOTE\n");
    for r in buf[..len].chunks(proc::APP_RECORD) {
        o.col(name_of(&r[0..32]), 17);
        let mut v = [0u8; 24];
        let mut n = 0;
        for i in 0..3 {
            if i > 0 {
                v[n] = b'.';
                n += 1;
            }
            let mut x = word(r, 36 + 4 * i);
            let start = n;
            loop {
                v[n] = b'0' + (x % 10) as u8;
                n += 1;
                x /= 10;
                if x == 0 {
                    break;
                }
            }
            v[start..n].reverse();
        }
        o.col(core::str::from_utf8(&v[..n]).unwrap_or("?"), 10);
        o.num_col(word(r, 32).div_ceil(1024) as u64, 9);
        o.str("  ");
        match r[48] {
            proc::APP_PREVIOUS => o.str("previous "),
            proc::APP_UNCONFIRMED => o.str("unconfirmed "),
            _ => {}
        }
        if r[49] & proc::APP_AUTOSTART != 0 {
            o.str("autostart");
        }
        o.str("\n");
    }
    event!(Info, "shell.listed", what = "apps", count = (len / proc::APP_RECORD) as u32);
}

fn kill(o: &mut Out, pid: u32) {
    match Handle::open(proc::KILL, rights::WRITE).and_then(|h| h.write(&pid.to_le_bytes())) {
        // Done on the kernel's next round; the log says how it went.
        Ok(_) => {
            o.str("kill ");
            o.num(pid as u64);
            o.str(": asked\n");
        }
        Err(Error::Busy) => o.str("kill: busy, try again\n"),
        // `proc:kill/own` (doc/52): only what this shell started.
        Err(Error::Denied) => o.str("kill: not started from this shell\n"),
        Err(_) => o.str("kill: failed\n"),
    }
}

/// `fg PID|APP` (doc/60 F3): puts the screen of the process (or of the
/// first one running APP) in front.
fn fg(o: &mut Out, arg: &str) {
    let pid = match arg.parse::<u32>() {
        Ok(p) => p,
        Err(_) => {
            let mut buf = [0u8; proc::PROC_RECORD * proc::MAX_PROCS];
            let Ok(len) = snapshot(proc::LIST, &mut buf) else {
                o.str("fg: cannot list the processes\n");
                return;
            };
            match buf[..len].chunks(proc::PROC_RECORD).find(|r| name_of(&r[16..32]) == arg) {
                Some(r) => word(r, 0),
                None => {
                    o.str("fg: no such process\n");
                    return;
                }
            }
        }
    };
    match Handle::open(display::FOCUS, rights::WRITE).and_then(|h| h.write(&pid.to_le_bytes())) {
        Ok(_) => event!(Info, "shell.fg", pid = pid),
        Err(Error::Invalid) => o.str("fg: it has no screen\n"),
        Err(_) => o.str("fg: failed\n"),
    }
}

/// Keys from the terminal, waiting up to `ms`: how many bytes; Err when
/// the terminal is gone.
fn keys(o: &Out, buf: &mut [u8], ms: u64) -> Result<usize, Error> {
    match o.tty.0.read(buf) {
        Err(Error::Busy) => {}
        r => return r,
    }
    let _ = wait_raw(&[o.tty.0.raw()], ms * 1_000_000);
    match o.tty.0.read(buf) {
        Err(Error::Busy) => Ok(0),
        r => r,
    }
}

fn top(o: &mut Out) {
    // CPU time (ms) per pid at the last round.
    let mut last = [(0u32, 0u32); proc::MAX_PROCS];
    let mut last_at = time_ns();
    let mut buf = [0u8; proc::PROC_RECORD * proc::MAX_PROCS];
    let mut key = [0u8; chan::MSG_MAX];
    let mut first = true;
    loop {
        let Ok(len) = snapshot(proc::LIST, &mut buf) else {
            o.str("top: cannot list the processes\n");
            return;
        };
        let now = time_ns();
        let window_ms = ((now - last_at) / 1_000_000).max(1) as u32;
        last_at = now;
        o.str("\x1b[H\x1b[2J");
        o.str("top: CPU per process over ");
        o.num(window_ms as u64);
        o.str(" ms (100% = one core). Any key ends.\n");
        // Memory (doc/61): what the processes use of the app pool, where
        // each has a slot.
        let used: u64 = buf[..len].chunks(proc::PROC_RECORD).map(ram_kb).sum();
        let mem = mem_info();
        if let Some([pool, slot, slots, shm, fb, psram, sram, kernel, stacks, rings]) = mem {
            let kb = |v: u32| v as u64 / 1024;
            o.str(if psram > 0 { "PSRAM " } else { "pool " });
            o.num(kb(if psram > 0 { psram } else { pool }));
            o.str(" KB: apps ");
            o.num(used);
            o.str(" of ");
            o.num(kb(pool));
            o.str(" (");
            o.num(slots as u64);
            o.str(" x ");
            o.num(kb(slot));
            o.str("), shm ");
            o.num(kb(shm));
            o.str(", fb ");
            o.num(kb(fb));
            o.str("\n");
            if sram > 0 {
                o.str("SRAM ");
                o.num(kb(sram));
                o.str(" KB: kernel ");
                o.num(kb(kernel));
                o.str(", stacks ");
                o.num(kb(stacks));
                o.str(", net rings ");
                o.num(kb(rings));
                o.str(", other ");
                o.num(kb(sram.saturating_sub(kernel + stacks + rings)));
                o.str("\n");
            }
        }
        o.str("\n  PID NAME             STATE        CPU%    CPU s  RAM KB  SLOT%\n");
        let slot_kb = mem.map_or(0, |m| m[1] as u64 / 1024);
        if first {
            event!(Info, "shell.top", used_kb = used as u32, pool_kb = mem.map_or(0, |m| m[0] / 1024), slot_kb = slot_kb as u32);
        }
        let mut next = [(0u32, 0u32); proc::MAX_PROCS];
        for (i, r) in buf[..len].chunks(proc::PROC_RECORD).enumerate() {
            let (pid, ms) = (word(r, 0), word(r, 8));
            next[i] = (pid, ms);
            o.num_col(pid as u64, 5);
            o.raw(b' ');
            o.col(name_of(&r[16..32]), 17);
            o.col(
                match r[4] {
                    proc::STATE_READY => "ready",
                    proc::STATE_SLEEPING => "sleeping",
                    proc::STATE_WAITING => "waiting",
                    _ => "exited",
                },
                9,
            );
            match last.iter().find(|l| l.0 == pid && l.0 != 0) {
                Some(&(_, before)) if !first => {
                    // Per mille of one core.
                    let pm = (ms.saturating_sub(before) as u64 * 1000 / window_ms as u64).min(9999);
                    o.num_col(pm / 10, 6);
                    o.raw(b'.');
                    o.num(pm % 10);
                }
                _ => o.str("     -  "),
            }
            o.num_col(ms as u64 / 1000, 7);
            o.raw(b'.');
            o.num(ms as u64 / 100 % 10);
            o.num_col(ram_kb(r), 8);
            if slot_kb > 0 {
                o.num_col(ram_kb(r) * 100 / slot_kb, 7);
            }
            o.str("\n");
        }
        o.flush();
        last = next;
        first = false;
        match keys(o, &mut key, 1000) {
            Ok(0) => {}
            _ => return,
        }
    }
}

fn ifconfig(o: &mut Out) {
    let Ok(net) = Net::connect() else {
        o.str("ifconfig: no network service\n");
        return;
    };
    for k in 0.. {
        let len = match net.if_info(k) {
            Ok(Some(len)) => len,
            Ok(None) => continue,
            Err(_) => break,
        };
        let r = &net.slice[..len];
        o.str(name_of(&r[0..8]));
        o.str(":\n");
        if r[8..14] != [0; 6] {
            o.str("    ether ");
            for (i, b) in r[8..14].iter().enumerate() {
                if i > 0 {
                    o.raw(b':');
                }
                o.hex2(*b);
            }
            o.str("\n");
        }
        if r[14] > 0 {
            o.str("    inet  ");
            o.ip(u32::from_le_bytes(r[16..20].try_into().unwrap()));
            o.raw(b'/');
            o.num(r[14] as u64);
            o.str("\n");
        }
        for (label, at) in [("    router ", 20), ("    dns   ", 24)] {
            let v = u32::from_le_bytes(r[at..at + 4].try_into().unwrap());
            if v != 0 {
                o.str(label);
                o.ip(v);
                o.str("\n");
            }
        }
        for i in 0..r[15] as usize {
            let e = &r[net::iface::HEADER + i * net::iface::V6_ENTRY..];
            o.str("    inet6 ");
            o.ip6(e[..16].try_into().unwrap());
            o.raw(b'/');
            o.num(e[16] as u64);
            o.str("\n");
        }
        o.str("    frames rx ");
        o.num(u32::from_le_bytes(r[28..32].try_into().unwrap()) as u64);
        o.str("  tx ");
        o.num(u32::from_le_bytes(r[32..36].try_into().unwrap()) as u64);
        o.str("\n");
    }
    event!(Info, "shell.listed", what = "ifconfig", count = 0u32);
}

/// A TCP connection: what arrives is shown; a line typed is sent with
/// CRLF when Enter is pressed. Ctrl-C or Ctrl-D ends it.
fn nc(o: &mut Out, host: &str, port: u16) {
    let Ok(mut net) = Net::connect() else {
        o.str("nc: no network service\n");
        return;
    };
    let Some(ip) = lookup(o, &mut net, "nc", host) else { return };
    let s = match net.tcp_connect(ip, port).and_then(|s| net.wait_established(s, 5000).map(|_| s)) {
        Ok(s) => s,
        Err(_) => {
            o.str("nc: cannot connect\n");
            return;
        }
    };
    o.str("connected to ");
    o.ip(ip);
    o.str("\n");
    o.flush();
    event!(Info, "shell.nc", port = port as u32);
    let mut line = [0u8; LINE_MAX + 2];
    let mut n = 0;
    let mut key = [0u8; chan::MSG_MAX];
    let mut data = [0u8; 1024];
    loop {
        let Ok(k) = keys(o, &mut key, 10) else { break };
        let mut quit = false;
        for &b in &key[..k] {
            match b {
                0x03 | 0x04 => quit = true,
                b'\r' | b'\n' => {
                    o.str("\n");
                    line[n..n + 2].copy_from_slice(b"\r\n");
                    if net.send_all(s, &line[..n + 2]).is_err() {
                        quit = true;
                    }
                    n = 0;
                }
                0x7F | 0x08 if n > 0 => {
                    n -= 1;
                    o.str("\x08 \x08");
                }
                0x20..=0x7E if n < LINE_MAX => {
                    line[n] = b;
                    n += 1;
                    o.raw(b);
                }
                _ => {}
            }
        }
        if quit {
            o.str("^C\n");
            break;
        }
        match net.recv_now(s, &mut data) {
            Ok(r) => {
                for &b in &data[..r.len] {
                    // Lone LFs as CRLF; CRs as they are.
                    if b == b'\n' {
                        o.raw(b'\r');
                    }
                    o.raw(b);
                }
                if r.ended {
                    o.str("\nconnection closed\n");
                    break;
                }
            }
            Err(_) => {
                o.str("\nconnection lost\n");
                break;
            }
        }
        o.flush();
    }
    let _ = net.close(s);
}
