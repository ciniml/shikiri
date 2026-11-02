# 40. シェルの `ps`・`apps`・`kill`（`proc:list`、`proc:apps`、`proc:kill`）

利用者の指示（2026-11-03）:「シェル動いてます。つぎにすすんでください」。シェル（doc/39）を実用に近づけるため、動いているプロセス、ストアのアプリを見る手段と、プロセスを止める手段を足した（判断）。これまでは、どれもホストからの制御コマンド（`ps`、`list`、`kill`、doc/14 §3）で、結果はログに出るだけだった。

## 1. 形（ABI 小版 16、`t5_abi::proc`）
カーネルのオブジェクトを 3 つ足した。どれもイメージの許可（`--permit proc:list` など）が要る。

| 名前 | 権利 | 内容 |
|---|---|---|
| `proc:list` | READ | プロセスの一覧。1 件 32 バイト: `pid`、状態（ready/sleeping/waiting/exited）、ハンドルの数、CPU 時間（ms）、RAM（data・bss・スタックのバイト数）、名前（16 バイトまで） |
| `proc:apps` | READ | ストアのアプリ。1 件 52 バイト: 名前（32）、長さ、版（3 つ）、状態（現在・前の版・未確定）、自動起動の印 |
| `proc:kill` | WRITE | `u32` の pid を書くと、そのプロセスを止める |

- **写し**: `proc:list` と `proc:apps` は、開いた後のサービスのループ（10 ms ごと）でカーネルが取る写しを読む。それまでの `read` は `Busy`。`read` はいつも写しの先頭から、入るだけの件数を返す。新しい写しは、開き直して得る。
  - 写しは、開いているハンドルがあるときだけ取る（`kbd` と同じ考え）。
- **kill**: 書き込みは待ち行列（8 個）に入る。カーネルは次の回で `sched::kill`（ホストの `kill` と同じ）を呼び、`proc.killed`（頼んだ pid、止めた pid）か `proc.kill_failed` を出す。
- **待ち行列と写しにした理由**: `proc:spawn`（doc/38 §2）と同じく、システムコールの中ではスケジューラとストアに触れない。
- **権限**: `proc:kill` は、どのプロセスでも止められる（`fs` や `net` も）。許可を持つアプリに限るので、今は十分とした（判断）。止められるのは、ストアの署名済みのイメージで許可を与えたアプリだけ。
- 実装: `kernel/src/procinfo.rs`、`kernel/src/handle.rs`（`Object::List`、`Object::Kill`）、`kernel/src/main.rs` のサービスのループ。

## 2. シェル
- `ps`: PID、名前、状態、ハンドルの数、RAM（KB）、CPU 時間（秒、起動からの合計）。
- `apps`: 名前、版、大きさ（KB）、印（`previous`、`unconfirmed`、`autostart`）。
- `kill PID`: 頼んだことを表示する。結果はログ（`proc.killed`）。
- 一覧を出したとき、`shell.listed`（`ps` か `apps`、件数）をログに出す。
- シェルの許可に `proc:list`、`proc:apps`、`proc:kill` を足した（Makefile の `PERMIT_shell`）。

## 3. 試験
- `make test-p4emu` の term/shell の場合に、`ps`、`apps`、`kill 99` を打つ操作を足した。`shell.listed` と `proc.kill_failed` を待つ。
