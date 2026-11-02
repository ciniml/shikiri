# 39. ターミナルとシェル（`kbd`、`term`、`shell`）

利用者の指示（2026-11-02）:「そろそろ実用的なアプリケーション例が一つ欲しいところです。まずはターミナルエミュレーターとシェルを実装してみてください。ターミナルエミュレーターは tab5 ssh client に C 実装があるはずです。」

利用者の選択（同日）:
- 入力は、まず Tab5 の付け外しできるキーボード。
- ターミナルとシェルは別のプロセスにする。

## 1. 構成
```
キーボード ──I2C0──> カーネル kbd ──read──> term ──svc:tty──> shell
                                            │  <──出力──────┘
                                            └──> display（80 x 22 字）
```
- **カーネル**: キーボードの事象を `kbd` オブジェクトで渡す（ABI 小版 15）。
- **`term`**（ユーザーモード）が、次のことをする:
  - キーの事象をバイト列にし、キーの繰り返しをする。
  - VT/xterm の解析と画面の状態を持ち、字を描く。
  - pty にあたるサービス `svc:tty` を出す。
- **`shell`** は `svc:tty` につなぐ、別のプロセス。後で SSH のクライアントなども、同じ `svc:tty` の形で `term` を使える。
- `term` は `proc:spawn`（doc/38）で `shell` を起動する。`shell` が終わったら、1 秒後にもう一度起動する。

## 2. キーボード（カーネル）
- Tab5 の付け外しできる QWERTY キーボード。ExtPort1 の I2C（アドレス 0x6D）につながり、INT は GPIO50（ロー アクティブ）。
  - ファームウェアを HID モード（`MODE` = 1）にすると、事象を（修飾キー、USB HID のキーボードページの usage）の組で待ち行列に入れる。usage 0 は離したこと。
  - レジスタは tab5_claude_client `main/input_tab5_kbd.cpp:26-34` の引用（M5Stack の `unit_Tab5Keyboard.hpp`）による。
  - キーボードの 5 V（IO エクスパンダ #1 の bit2 EXT5V_EN）は、パネルを上げるときにもう立てている（`tab5.rs` の 0x44/0x64）。新しくエクスパンダに書くことはない。
- **I2C の 2 本目**: `i2c.rs` の `Bus` で、I2C1（内部バス、GPIO31/32）と I2C0（ExtPort1、SDA = GPIO0、SCL = GPIO1）を扱う。
  - I2C0 の値は IDF6 のヘッダによる: `hp_sys_clkrst_reg.h` の PERI_CLK_CTRL10 の bit0（クロック源）、bit1（有効）、bit2-9（分周）、SOC_CLK_CTRL2 の bit12、HP_RST_EN1 の bit22。`gpio_sig_map.h` の信号 68/69。
  - どちらも 100 kHz。GPIO0/1 は LP IO の範囲だが、`LP_IOMUX.pad[n].mux_sel` の既定（0）で HP 側にある（`rtc_io_ll.h:126-131`）。
- `kbd.rs`（プラットフォーム）:
  - 最初の読み出しのときに I2C0 と INT のピンを用意する。キーボードがなければ 1 秒ごとに探す（`kbd.found`）。
  - INT が低いとき（取りこぼしに備えて 0.5 秒ごとにも）`EVENT_NUM` と `HID_EVENT` を読み、`INT_STAT` に 0 を書く。読めなくなったら `kbd.lost` を出して、また探す。
- **オブジェクト `kbd`**（`t5_abi::kbd`、許可が要る、一度に 1 人）:
  - `read` は 2 バイト（修飾、usage）の事象を返す。`wait` の `READABLE` で待てる。
  - カーネルは、開かれている間だけ、サービスのループ（10 ms ごと）でキーボードを読む（`touch` と同じ）。待ち行列は 32 で、あふれたら古いものを捨て、閉じるときに `kbd.dropped` を出す。
  - キーボードがなくても開ける。付けた後に事象が来る。
- **t5emu**: I2C0 に、0x6D のキーボードを置いた。`--ctl "keys TEXT"` で文字を打つ（`\n` は Enter）。30 ms に 1 キーで、押すと離すの事象を出す。INT（GPIO_IN1 の bit18）も、事象があれば低くなる。

## 3. `term`
- **VT の解析と画面**（`vt.rs`）: tab5_claude_client の `components/term_core`（Boost Software License 1.0）の `parser.cpp`、`screen.cpp`、`east_asian_width.hpp` を Rust に移した。
  - ヒープを使わない。80 x 22 の固定の格子 2 つ（主画面と、DECSET 47/1047/1049 の代替画面）を、0 で埋めた静的な領域に置く（`'\0'` の字を空白として描く。これで `.bss` に入り、イメージが 84 KB 増えない）。
  - 扱うもの: カーソルの移動（CUU/CUD/CUF/CUB/CNL/CPL/CHA/VPA/CUP）、ED/EL、IL/DL/ICH/DCH/ECH、SU/SD、DECSTBM、SGR（16/256 色と RGB、太字、反転、下線など）、DECSC/DECRC、DECTCEM、DECAWM、DSR（5 と 6）、IND/RI/NEL/RIS、UTF-8、全角の 2 マス。
- **字**: 16 x 32 のビットマップ。横置き 1280 x 720（270°）に 80 字 x 22 行。
  - 字形は、Terminus Font の 16x32（SIL OFL 1.1）から ASCII の 95 字と代わりの 1 字を抜いたもの（`third_party/terminus-font/`、`tools/psf2font.py`）。
  - 抜き出しは OFL の改変版にあたるので、予約名「Terminus Font」はこのファイルには使わない。
  - ASCII 以外は、代わりの 1 字で描く（日本語は未対応）。
- **キー**（`keys.rs`）: tab5_claude_client の `main/hid_translate.cpp` と同じ変換。
  - US 配列。Enter は CR、Backspace は DEL（0x7F）。
  - 矢印・Home/End・Ins/Del・PgUp/PgDn・F1-F12 は xterm の列。修飾キーがあれば `CSI 1;m X` / `CSI n;m ~` の形にする。
  - Ctrl で制御文字、Alt で ESC を前に付ける。Shift+Tab は `CSI Z`。
  - キーの繰り返しは `term` がする: 押して 400 ms 後から 60 ms ごと（同じ値）。
- **`svc:tty`**:
  - `shell` から `term` へのメッセージは、画面への出力（バイト列）。
  - `term` から `shell` へのメッセージは、キーのバイト列と、DSR の答え。
  - 一度に 1 つのセッションだけ。2 つ目の接続は閉じる。
- **描画**: 変わったマス（解析が記録する範囲）とカーソルの前後のマスを描き、その範囲のフレームバッファの行だけを流す（`flush`）。
- **イベント**: `term.ready`（字の数、キーボードを開けたか）、`term.session`、`term.session_ended`。

## 4. `shell`
- `svc:tty` につなぎ、プロンプト `t5$ ` を出し、行を編集する:
  - 文字はその場で表示する。
  - Backspace、Ctrl-U（行を消す）、Ctrl-C（行をやめる）、Ctrl-L（画面を消す）、上下の矢印（履歴 8 行）。
- 組み込みのコマンド:

  | コマンド | 内容 |
  |---|---|
  | `help` | 一覧 |
  | `echo`、`clear`、`uptime` | |
  | `ls`、`cat`、`write`、`rm`、`mkdir` | SD カード（`svc:fs`） |
  | `run APP [A0 [A1]]` | ストアのアプリを起動（`proc:spawn`） |
  | `ping HOST` | 4 回の ping。名前は DNS で引く（`svc:net`） |
  | `exit` | 終わる（`term` が新しいシェルを起動する） |

- 打ったコマンドは `shell.exec`（コマンド名、引数の数）としてログに出す。引数そのものは出さない。

## 5. 起動
- `init` の組み込みの表（SD に `START.CFG` がないとき）に `term` を足した。
- SD に `START.CFG` があれば、そこに `term` の行を書く。

## 6. 試験
- `make test-p4emu`:
  - `run keylog 10 6` の後に `keys aB\n` と打ち、`kbd.found`、`keylog.key`、`keylog.done` を待つ（事象は 6 つ。B は Shift 付き）。
  - `run term` の後に `keys echo hi\n` と打ち、`term.ready`、`term.session`、`shell.exec` を待つ。
- 実機: 次の節。
- 実機（2026-11-02）:
  - カーネル（ota_seq=57）と全アプリを入れ、`run init 1` で `START.CFG` を書き直して再起動した。
  - 3.16 秒に `init` が `term` を起動した。3.19 秒に `kbd.found`（ファームウェア 1）、3.65 秒に `term.ready`、3.72 秒に `shell` のセッション。
  - 利用者が画面とキーボードでシェルを使い、動くことを確かめた（2026-11-03、「シェル動いてます」）。
- 続き: `ps`・`apps`・`kill` は doc/40。
