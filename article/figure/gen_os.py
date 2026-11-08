#!/usr/bin/env python3
"""Figures of os.md as SVG: the overview (kernel, services, apps) and a
process's slot with its PMP ranges.
    python3 figure/gen_os.py   (from article/)"""
import os

HERE = os.path.dirname(os.path.abspath(__file__))
FONT = 'font-family="Noto Sans CJK JP, sans-serif"'


def box(x, y, w, h, text, fill="#ffffff", size=13, bold=False):
    weight = ' font-weight="bold"' if bold else ""
    lines = text.split("\n")
    out = [f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="4" fill="{fill}" stroke="#333" stroke-width="1.2"/>']
    top = y + h / 2 - (len(lines) - 1) * size * 0.6
    for i, line in enumerate(lines):
        out.append(f'<text x="{x + w / 2}" y="{top + i * size * 1.2 + size * 0.35}" text-anchor="middle" font-size="{size}"{weight} {FONT}>{line}</text>')
    return "\n".join(out)


def label(x, y, text, size=12, anchor="start"):
    return f'<text x="{x}" y="{y}" font-size="{size}" text-anchor="{anchor}" {FONT}>{text}</text>'


def arrow(x1, y1, x2, y2):
    return f'<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" stroke="#333" stroke-width="1.2" marker-end="url(#a)"/>'


def svg(w, h, body):
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">\n'
        '<defs><marker id="a" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto">'
        '<path d="M0,0 L8,4 L0,8 z" fill="#333"/></marker></defs>\n'
        f'<rect width="{w}" height="{h}" fill="#ffffff"/>\n{body}\n</svg>\n'
    )


def overview():
    b = []
    # U mode
    b.append(label(10, 22, "Uモード (PMPで互いに隔離)", 13))
    apps = [("term", 20), ("shell", 120), ("mtdash", 220), ("ota", 320)]
    for name, x in apps:
        b.append(box(x, 34, 88, 36, name, "#eef5ff"))
    services = [("net", 20), ("wifi", 120), ("fs", 220), ("cfg", 320), ("matter", 420)]
    for name, x in services:
        b.append(box(x, 104, 88, 36, name, "#fff6e0"))
    b.append(label(430, 58, "アプリ", 12))
    b.append(label(520, 128, "サービス", 12))
    # channels app -> service
    b.append(arrow(164, 70, 64, 104))
    b.append(arrow(364, 70, 264, 104))
    b.append(label(180, 92, "チャネル (svc:)", 11))
    # syscall boundary
    b.append('<line x1="10" y1="160" x2="590" y2="160" stroke="#333" stroke-dasharray="6,4"/>')
    b.append(label(16, 178, "ecall (システムコール14個、ハンドル経由)", 12))
    # kernel
    b.append(box(10, 190, 580, 110, "", "#f0f0f0"))
    b.append(label(20, 210, "カーネル (Mモード)", 13))
    kparts = [("スケジューラ\n2コア", 20), ("ローダ\nPMP", 112), ("チャネル\n共有メモリ", 204), ("ストア・更新\n(A/B)", 296), ("構造化ログ", 388), ("ドライバ\n表示・SD・Wi-Fi", 480)]
    for t, x in kparts:
        b.append(box(x, 222, 86, 64, t, "#ffffff", 11))
    # hardware
    b.append(box(10, 316, 580, 34, "ESP32-P4 (RV32IMAFC×2、PMP 16本、MMUなし)  /  PSRAM 32MiB  /  フラッシュ16MB", "#e8f3e8", 12))
    return svg(600, 360, "\n".join(b))


def process():
    b = []
    x0, w = 150, 220
    parts = [
        ("text + rodata", 40, "#eef5ff", "R-X (PMP #2、TOR)"),
        ("data + bss", 30, "#fff6e0", "RW- (PMP #4、TOR)"),
        ("stack", 34, "#fff6e0", ""),
        ("(スロットの空き)", 40, "#f7f7f7", "どのエントリにも一致しない → 拒否"),
    ]
    y = 30
    b.append(label(x0 + w / 2, 20, "プロセスのスロット (PSRAM、2MiB)", 13, "middle"))
    b.append(label(x0 - 8, y + 4, "base", 11, "end"))
    for name, h, fill, note in parts:
        b.append(box(x0, y, w, h, name, fill, 12))
        if note:
            b.append(label(x0 + w + 12, y + h / 2 + 4, note, 11))
        y += h
    b.append(label(x0 - 8, y, "slot end", 11, "end"))
    y += 24
    b.append(box(x0, y, w, 30, "フレームバッファ (displayのMAP)", "#e8f3e8", 11))
    b.append(label(x0 + w + 12, y + 19, "RW- (PMP #6、TOR)", 11))
    y += 38
    b.append(box(x0, y, w, 30, "共有メモリの窓 ×2、フレームの輪", "#e8f3e8", 11))
    b.append(label(x0 + w + 12, y + 19, "RW- (PMP #8以降)", 11))
    y += 44
    b.append(label(x0, y, "カーネルのRAM、周辺のMMIO、ほかのスロット: Uモードからは拒否", 11))
    return svg(600, y + 14, "\n".join(b))


for name, body in [("os_overview.svg", overview()), ("os_process.svg", process())]:
    with open(os.path.join(HERE, name), "w") as f:
        f.write(body)
    print(name)
