# フォントサービスの字形（doc/45）

`glyphs.bin` は、アプリ `font` が共有メモリで渡す字形のファイル。`tools/mkfont.py third_party/jp-font/glyphs.bin` で作った。形式は `tools/mkfont.py` の説明と doc/45 §2 にある。

| 部分 | 元 | ライセンス |
|---|---|---|
| 半角（16 x 32）836 字（ASCII を除く。ラテン文字、ギリシャ文字、キリル文字、罫線など） | Terminus Font（Dimitar Toshkov Zhekov）。Ubuntu の console-setup の `/usr/share/consolefonts/{Lat15,Uni2,Uni3,FullGreek,FullCyrSlav}-Terminus32x16.psf.gz` | SIL OFL 1.1（`OFL-Terminus.txt`） |
| 全角（32 x 32）6649 字（JIS X 0208 のうち East Asian Width が W/F のもの）と、半角カタカナ（16 x 32）63 字 | Noto Sans Mono CJK JP Regular（Ubuntu の fonts-noto-cjk 1:20230817+repack1-3、`/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc`）を、Pillow で 28 画素の大きさに描いて 2 値にした | SIL OFL 1.1（`OFL-Noto.txt`。Debian の copyright ファイルからの写し） |

- どちらも、抜き出して形式を変えたもの。OFL の改変版（Modified Version）に当たる。このため、元の名前（予約されたフォント名を含む）をこのファイルには使わない。ライセンスは OFL 1.1 のまま。
- Noto CJK の上流（github.com/notofonts/noto-cjk）の LICENSE は、Adobe の著作権と、予約されたフォント名 "Source" を挙げている（作成者の知識による。上の写しは Debian の記載）。
- 作り直すには、上の 2 つのパッケージ（console-setup のフォント、fonts-noto-cjk）と Pillow が要る。
