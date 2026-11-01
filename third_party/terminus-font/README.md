# term のフォント

`ascii-16x32.bin` は、Terminus Font（Dimitar Toshkov Zhekov、SIL Open Font License 1.1、`OFL.txt`）の 16x32 から作った。U+0020〜U+007E と、ない文字の代わりの 1 字だけを抜き出し、形式を変えている。

- 元: Ubuntu の console-setup の `/usr/share/consolefonts/Lat15-Terminus32x16.psf.gz`
- 作り方: `tools/psf2font.py /usr/share/consolefonts/Lat15-Terminus32x16.psf.gz third_party/terminus-font/ascii-16x32.bin`
- 抜き出しは OFL の改変版（Modified Version）に当たる。そのため、予約されたフォント名「Terminus Font」をこのファイルには使わない。ライセンスは OFL 1.1 のまま。
