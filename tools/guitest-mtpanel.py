#!/usr/bin/env python3
# Copyright (c) 2026 Kenta IDA
# Distributed under the Boost Software License, Version 1.0.
# (See accompanying file LICENSE or copy at https://www.boost.org/LICENSE_1_0.txt)
# SPDX-License-Identifier: BSL-1.0

"""Checks `mtpanel` (doc/53 §4) on the Tab5 through the console line alone
(doc/54, doc/55): taps and swipes go in as `tap`/`swipe`, and what the
screen shows is read back from `capture`: the colours by pixel, the text
by matching the cells against the Terminus font `mtpanel` draws with.

usage: tools/guitest-mtpanel.py [--port /dev/ttyACM3] [--out DIR]
                                [--smctl "SMCTL --state-dir D"] [--node 1]

`term` is stopped for the test (`display` has one owner) and started
again at the end; the tile is left as it was. With `--smctl`, a
controller's On/Off is also checked (`smctl ... onoff toggle NODE EP`).
Exit 0 when every check passed.
"""

import argparse
import json
import queue
import shlex
import subprocess
import sys
import threading
import time
import zlib

T5LOG = "tools/target/release/t5log"
SCHEMAS = "build/t5log.schemas"
KERNEL = "build/t5-kernel-esp32p4v1.elf"
FONT = "third_party/terminus-font/ascii-16x32.bin"
CW, CH = 16, 32

# As user/apps/mtpanel/src/main.rs.
BAR_H = 64
TILE = (32, BAR_H + 32, 320, 320)
LIST_X = TILE[0] + TILE[2] + 48
LIST_LINES = 8


def rgb(r, g, b):
    """`t5_rt::rgb` and back to 8 bits as `t5log` writes the PNG."""
    p = (r >> 3) << 11 | (g >> 2) << 5 | b >> 3
    r5, g6, b5 = p >> 11 & 0x1F, p >> 5 & 0x3F, p & 0x1F
    return (r5 << 3 | r5 >> 2, g6 << 2 | g6 >> 4, b5 << 3 | b5 >> 2)


BACKGROUND = rgb(20, 22, 30)
BAR = rgb(30, 70, 150)
TEXT = rgb(235, 235, 235)
DIM = rgb(150, 150, 160)
ON = rgb(255, 196, 40)
OFF = rgb(70, 72, 84)


class Image:
    """An 8-bit RGB PNG as `t5log` writes it (filter 0 on every row)."""

    def __init__(self, path):
        b = open(path, "rb").read()
        assert b[:8] == b"\x89PNG\r\n\x1a\n", path
        o, idat = 8, b""
        while o < len(b):
            n = int.from_bytes(b[o:o + 4], "big")
            kind, data = b[o + 4:o + 8], b[o + 8:o + 8 + n]
            if kind == b"IHDR":
                self.w, self.h = int.from_bytes(data[0:4], "big"), int.from_bytes(data[4:8], "big")
            elif kind == b"IDAT":
                idat += data
            o += 12 + n
        raw = zlib.decompress(idat)
        stride = 1 + 3 * self.w
        assert all(raw[y * stride] == 0 for y in range(self.h)), "PNG row filter"
        self.rows = [raw[y * stride + 1:(y + 1) * stride] for y in range(self.h)]
        self.path = path

    def px(self, x, y):
        r = self.rows[y]
        return tuple(r[3 * x:3 * x + 3])


class Font:
    def __init__(self, path):
        b = open(path, "rb").read()
        self.glyphs = {}
        for i in range(len(b) // (CW // 8 * CH)):
            g = b[i * 64:(i + 1) * 64]
            bits = tuple(int.from_bytes(g[2 * r:2 * r + 2], "big") for r in range(CH))
            self.glyphs.setdefault(bits, chr(0x20 + i))

    def read(self, img, x, y, n, scale, fg, bg):
        """`n` cells of text at (x, y), as `mtpanel`'s `text` draws them;
        `?` for a cell that is no glyph in `fg` on `bg`."""
        out = ""
        for i in range(n):
            gx = x + i * CW * scale
            if gx + CW * scale > img.w or y + CH * scale > img.h:
                break
            rows, ok = [], True
            for row in range(CH):
                line = 0
                for col in range(CW):
                    p = img.px(gx + col * scale, y + row * scale)
                    if p == fg:
                        line |= 0x8000 >> col
                    elif p != bg:
                        ok = False
                rows.append(line)
            out += self.glyphs.get(tuple(rows), "?") if ok else "?"
        return out


class Tab5:
    """`t5log serial` in its interactive form: commands on its standard
    input, events (JSON) and its own notes merged into one queue."""

    def __init__(self, args, log):
        cmd = [T5LOG, "serial", "--port", args.port, "--schemas", SCHEMAS, "--elf", KERNEL,
               "--json", "--capture-dir", args.out]
        self.p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True, bufsize=1)
        self.q = queue.Queue()
        # Items a command's answer was waited past, for the next reads.
        self.back = []
        self.log = log
        for stream, kind in ((self.p.stdout, "ev"), (self.p.stderr, "note")):
            threading.Thread(target=self._read, args=(stream, kind), daemon=True).start()

    def _read(self, stream, kind):
        for line in stream:
            self.log.write(line)
            self.log.flush()
            if kind == "ev":
                try:
                    line = json.loads(line)
                except ValueError:
                    continue
            self.q.put((kind, line))
        self.q.put(("eof", None))

    def next(self, deadline):
        if self.back:
            return self.back.pop(0)
        try:
            item = self.q.get(timeout=max(0.0, deadline - time.monotonic()))
        except queue.Empty:
            return None
        if item[0] == "eof":
            raise Fail("t5log ended")
        return item

    def expect(self, pred, timeout, what, keep=False):
        """The first event (or note) for which `pred` holds; with `keep`,
        the ones before it are read again afterwards."""
        deadline, skipped = time.monotonic() + timeout, []
        try:
            while (item := self.next(deadline)) is not None:
                if pred(*item):
                    return item[1]
                if keep:
                    skipped.append(item)
            raise Fail(f"no {what} in {timeout} s")
        finally:
            self.back[:0] = skipped

    def gather(self, seconds):
        """Every event in the next `seconds`."""
        deadline, seen = time.monotonic() + seconds, []
        while (item := self.next(deadline)) is not None:
            if item[0] == "ev":
                seen.append(item[1])
        return seen

    def cmd(self, line, timeout=15):
        """Sends a command; its `ctl.done` (`ctl.failed` raises)."""
        self.p.stdin.write(line + "\n")
        self.p.stdin.flush()
        note = self.expect(lambda k, v: k == "note" and v.startswith("t5log: -> #") and v.rstrip().endswith(line),
                           5, f"send of {line!r}", keep=True)
        rid = int(note.split("#")[1].split()[0])
        ev = self.expect(lambda k, v: k == "ev" and v.get("event") in ("ctl.done", "ctl.failed")
                         and v.get("request_id") == rid, timeout, f"answer to {line!r}", keep=True)
        if ev["event"] == "ctl.failed":
            raise Fail(f"{line}: {ev.get('error', ev)}")
        return ev

    def capture(self):
        self.p.stdin.write("capture\n")
        self.p.stdin.flush()
        # A failed one (`t5log: capture N: ...`) is None.
        note = self.expect(lambda k, v: (k == "note" and v.startswith("t5log: capture "))
                           or (k == "ev" and v.get("event") == "capture.failed"), 15, "capture")
        if isinstance(note, dict) or " -> " not in note:
            print(f"  ({note.strip()})", flush=True)
            return None
        return Image(note.rsplit(" -> ", 1)[1].strip())

    def close(self):
        self.p.stdin.close()
        try:
            self.p.wait(5)
        except subprocess.TimeoutExpired:
            self.p.kill()


class Fail(Exception):
    pass


def event(name, **fields):
    return lambda k, v: k == "ev" and v.get("event") == name and all(v.get(f) == x for f, x in fields.items())


def screen_errors(img, font, ep, on):
    """What differs from `mtpanel`'s `draw` with endpoint `ep` in state `on`."""
    errs = []
    if (img.w, img.h) != (1280, 720):
        return [f"size {img.w} x {img.h}"]
    title = f"Matter  endpoint {ep}"
    if (t := font.read(img, 16, 16, len(title), 1, TEXT, BAR)) != title:
        errs.append(f"title {t!r}")
    for x, y in ((4, 4), (1270, 60)):
        if img.px(x, y) != BAR:
            errs.append(f"bar at ({x}, {y}) {img.px(x, y)}")
    if img.px(700, 680) != BACKGROUND:
        errs.append(f"background {img.px(700, 680)}")
    x, y, tw, th = TILE
    color = ON if on else OFF
    for px, py in ((x, y), (x + tw - 1, y), (x, y + th - 1), (x + tw - 1, y + th - 1), (x + 8, y + th // 2)):
        if img.px(px, py) != color:
            errs.append(f"tile at ({px}, {py}) {img.px(px, py)}, expected {color}")
    for px, py in ((x - 1, y), (x + tw, y + th - 1)):
        if img.px(px, py) != BACKGROUND:
            errs.append(f"outside the tile at ({px}, {py}) {img.px(px, py)}")
    label = "ON" if on else "OFF"
    fg = BACKGROUND if on else TEXT
    lx, ly = x + (tw - len(label) * CW * 3) // 2, y + (th - CH * 3) // 2
    if (t := font.read(img, lx, ly, len(label), 3, fg, color)) != label:
        errs.append(f"label {t!r}")
    if (t := font.read(img, x, y + th + 16, 15, 1, DIM, BACKGROUND)) != "touch to switch":
        errs.append(f"hint {t!r}")
    if (t := font.read(img, LIST_X, BAR_H + 32, 9, 1, DIM, BACKGROUND)) != "endpoints":
        errs.append(f"list heading {t!r}")
    lines = list_lines(img, font)
    mine = [l for l in lines if l.startswith(f"{ep} light ")]
    want = f"{ep} light {int(on)}"
    if mine != [want]:
        errs.append(f"list {lines}, expected {want!r} in it")
    return errs


def list_lines(img, font):
    out = []
    for i in range(LIST_LINES):
        ly = BAR_H + 80 + i * (CH + 8)
        s = font.read(img, LIST_X, ly, (img.w - LIST_X) // CW, 1, TEXT, BACKGROUND).rstrip()
        if not s:
            break
        out.append(s)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", default="/dev/ttyACM3")
    ap.add_argument("--out", default=".")
    ap.add_argument("--smctl", help="the smctl command line up to its subcommand")
    ap.add_argument("--node", default="1", help="the Tab5's node id in smctl's fabric")
    args = ap.parse_args()
    font = Font(FONT)
    log = open(f"{args.out}/guitest-mtpanel.log", "w")
    t = Tab5(args, log)
    results = []
    state = {"ep": None, "on": None, "pid": None}

    def check(name, f):
        t0 = time.monotonic()
        try:
            f()
            results.append((name, None))
            print(f"PASS {name} ({time.monotonic() - t0:.1f} s)", flush=True)
        except Fail as e:
            results.append((name, str(e)))
            print(f"FAIL {name}: {e}", flush=True)

    def screen_shows(on, timeout=8.0):
        """Captures until the screen is `draw`'s for `on` (drawing takes a
        moment; the list follows every 3 s)."""
        deadline = time.monotonic() + timeout
        while True:
            img = t.capture()
            if img is None:
                if time.monotonic() > deadline:
                    raise Fail("the captures failed")
                continue
            errs = screen_errors(img, font, state["ep"], on)
            if not errs:
                return img
            if time.monotonic() > deadline:
                raise Fail(f"{img.path}: " + "; ".join(errs))

    def tap_switches(x, y):
        on = not state["on"]
        t.cmd(f"tap {x} {y}")
        t.expect(event("mtpanel.touched", on=on), 3, f"mtpanel.touched on={on}")
        t.expect(event("mtpanel.state", on=on), 5, f"mtpanel.state on={on}")
        state["on"] = on
        screen_shows(on)

    def no_switch(line, seconds=1.5):
        t.cmd(line)
        seen = t.gather(seconds)
        if not any(e.get("event") == "touch.injected" for e in seen):
            raise Fail("no touch.injected")
        if bad := [e for e in seen if e.get("event") in ("mtpanel.touched", "mtpanel.state")]:
            raise Fail(f"switched: {bad}")
        screen_shows(state["on"], timeout=0)

    def smctl_switches():
        on = not state["on"]
        cmd = shlex.split(args.smctl) + ["onoff", "toggle", args.node, str(state["ep"])]
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
        if r.returncode != 0:
            raise Fail(f"smctl rc={r.returncode}: {r.stderr.strip()[-200:]}")
        seen = t.gather(2.0)
        if not any(e.get("event") == "mtpanel.state" and e.get("on") == on for e in seen):
            raise Fail(f"no mtpanel.state on={on}")
        if any(e.get("event") == "mtpanel.touched" for e in seen):
            raise Fail("mtpanel.touched without a touch")
        state["on"] = on
        screen_shows(on)

    def swipe_once():
        # Only a finger coming down on the tile counts, once.
        on = not state["on"]
        t.cmd("swipe 192 256 700 256 500")
        seen = t.gather(2.0)
        touched = [e.get("on") for e in seen if e.get("event") == "mtpanel.touched"]
        if touched != [on]:
            raise Fail(f"mtpanel.touched {touched}, expected [{on}]")
        if not any(e.get("event") == "mtpanel.state" and e.get("on") == on for e in seen):
            raise Fail(f"no mtpanel.state on={on}")
        state["on"] = on
        screen_shows(on)

    term_was = False
    try:
        t.cmd("ps")
        procs = {}
        for e in t.gather(0.5):
            if e.get("event") == "proc.info":
                procs.setdefault(e["name"], []).append(e["pid"])
        for pid in procs.get("mtpanel", []):
            t.cmd(f"kill {pid}")
        for pid in procs.get("term", []):
            t.cmd(f"kill {pid}")
            term_was = True
        t.gather(0.5)

        def start():
            ev = t.cmd("run mtpanel 0")
            state["pid"] = ev.get("value")
            ready = t.expect(event("mtpanel.ready"), 10, "mtpanel.ready")
            state["ep"] = ready["ep"]
            st = t.expect(event("mtpanel.state"), 5, "mtpanel.state")
            state["on"] = st["on"]
            state["on0"] = st["on"]
            screen_shows(state["on"], timeout=10)

        check("起動の画面", start)
        if state["ep"] is None:
            raise Fail("mtpanel did not start")
        check("タイルの中央の tap で切り替わる", lambda: tap_switches(TILE[0] + TILE[2] // 2, TILE[1] + TILE[3] // 2))
        check("もう 1 度の tap で戻る", lambda: tap_switches(TILE[0] + TILE[2] // 2, TILE[1] + TILE[3] // 2))
        check("タイルの外の tap では変わらない", lambda: no_switch("tap 700 600"))
        check("タイルの左上の端（内）で切り替わる", lambda: tap_switches(TILE[0], TILE[1]))
        check("タイルの右の 1 画素外では変わらない", lambda: no_switch(f"tap {TILE[0] + TILE[2]} {TILE[1] + TILE[3] // 2}"))
        check("タイルの右下の端（内）で切り替わる", lambda: tap_switches(TILE[0] + TILE[2] - 1, TILE[1] + TILE[3] - 1))
        check("外から入る swipe では変わらない", lambda: no_switch("swipe 700 256 192 256 500", 2.0))
        check("中から出る swipe で 1 度だけ切り替わる", swipe_once)
        if args.smctl:
            check("コントローラ（smctl）の toggle で描き直す", smctl_switches)
            check("コントローラ（smctl）の toggle で戻る", smctl_switches)
        if state["on"] != state.get("on0"):
            check("元の状態へ戻す", lambda: tap_switches(TILE[0] + TILE[2] // 2, TILE[1] + TILE[3] // 2))
    except Fail as e:
        results.append(("準備", str(e)))
        print(f"FAIL 準備: {e}", flush=True)
    finally:
        try:
            if state["pid"]:
                t.cmd(f"kill {state['pid']}")
            if term_was:
                t.cmd("run term")
        except Fail as e:
            print(f"後片付け: {e}", flush=True)
        t.close()
    failed = [r for r in results if r[1]]
    print(f"{len(results) - len(failed)}/{len(results)} passed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
