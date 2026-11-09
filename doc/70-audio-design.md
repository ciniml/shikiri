# 70. オーディオ（スピーカーとマイク）の設計

利用者の指示（2026-11-09）: kumiki の作業（kumiki-p4v1-hal の I2S と、カーネル向けの依頼 1〜5、doc/69 の続きの相談）の後、「Tab5 を書き戻して、オーディオの設計を」。IO エクスパンダ #1 の bit1（SPK_EN）の書き込みは許可（同日）。

**実装の状況**: 段 A1 のうちコーデックを除く部分（§7、I2S を起動時に無音で回す、Tab5 で確認）と、段 A2（§8、`audio:out` / `audio:in`、混ぜる、`beep` と `mic`、エミュレータで確認）。コーデックと SPK_EN は kumiki のコーデックのクレートを待つ。§3 の「共有メモリの輪」は、実装では `write` / `read` にした（§8）。

## 1. Tab5 のオーディオ（kumiki の調べと実機の確認による）
| 部品 | 内容 |
|---|---|
| I2S0 | 全二重。MCLK GPIO30、BCK 27、WS 29、DOUT 26、DIN 28。48 kHz、16 ビットのステレオ、MCLK = 256 fs（水晶から） |
| ES8388（I2C 0x10） | DAC → アンプ NS4150B → スピーカー |
| アンプの有効 | IO エクスパンダ #1（0x43）の bit1（SPK_EN）。読んで bit1 だけ変えて書き戻す |
| ES7210（I2C 0x40） | マイクの ADC |
| AHB-GDMA | I2S の DMA。tab5-os のカーネルは今使っていない（表示は 0x5008_1000 の GDMA、2D は 2D-DMA、Ethernet は自分の DMA） |

- **順番の注意**（kumiki の実機での確認、理由は不明）: ES8388 は、画面の電源投入（#1 の出力を書き直す、LCD とタッチのリセット）より前に設定しないと鳴らない。tab5-os の画面の手順は今、#1 の出力を 0x64 で丸ごと書き、bit1 を 0 にしている。

## 2. 方針
- **I2S と DMA は kumiki-p4v1-hal を使う**（`--no-default-features`、`rt` なし。doc/68 の方針で、新しいドライバから kumiki を使う）。kumiki はカーネル向けに、割り込みの API、DMA のチャネルの選択、待ちの上限、パニックをなくすこと、`ahb_dma::assume_initialized` を入れてある（kumiki 73dd3f9、5263cf4）。
- **コーデック（ES8388、ES7210）の設定はカーネルの起動時に**、画面の電源投入より前に行う（§1 の順番）。ユーザー空間のサービスでは画面より後になるので、この形にした。設定は kumiki が管理するドライバを使う（§6 の 4）。
- **画面の手順は #1 の bit1 を保つ**（読んで、bit1 以外を書く）。
- **アプリには `audio` オブジェクト**。データは共有メモリの輪で渡す（`netdev` の輪、doc/35 と同じ考え）。

## 3. カーネル
- 起動（`tab5.rs` の板の初期化、画面より前）: I2C で ES8388 と ES7210 を設定、SPK_EN を立てる。AHB-GDMA のブロックをリセットし、窓を L2MEM（kumiki の I2S のリングはカーネルの `.bss`、0x4FF4_0000 付近）を含むように設定して、`ahb_dma::assume_initialized()`。`I2s::new`（チャネル 0）と `start`。誰も鳴らしていないときは無音を流し続ける（kumiki の作り）。
- 割り込み: `I2s::interrupt_sources()`（rev v1 では IN = 56 + n、OUT = 59 + n）をコア 0 の CLIC の空いた線へ。`I2s::on_interrupt()` を呼び、送りの空きにアプリの輪から写し、受けたものをアプリの輪へ写す（512 バイトごと、48 kHz のステレオで約 2.7 ms ごと）。
- `audio` オブジェクト（ABI の小版を上げる）:
  - 複数のプロセスが開け、カーネルが再生を混ぜる（§6 の 1。最初は 1 つずつでもよい）。録音は開いた全員に同じものを渡す。
  - `ctl(MAP)`: 再生と録音の輪（各 16 KiB 程度、約 85 ms）を写像する。輪は先頭に書き位置と読み位置（`netdev` の輪と同じ形）。
  - `ctl(VOLUME, 0..100)`: ES8388 の音量。
  - `wait` の事象: `WRITABLE`（再生の輪に空きが閾値以上）、`READABLE`（録音の輪にデータ）、`UNDERRUN` / `OVERRUN` の数。
  - 閉じたら無音に戻す。
- **許可**: `audio:out`（スピーカー）は普通の許可、`audio:in`（マイク）は **強い許可**（盗み聞きを防ぐ。システムの鍵だけ）。前の席かどうかには関係しない（§6 の 2）。
- `ctl(INFO)`: サンプリング周波数、ビット数、チャネル数（§6 の 3）。

## 4. アプリ
- `t5_rt::Audio`: `open`、`frames_free()`、`write_frames(&[[i16; 2]])`、`read_frames`、`set_volume`。
- 試験用のアプリ `beep`（正弦波を鳴らす）と `mic`（マイクの音量を記録する、`audio:in` なのでシステムの鍵）。
- kumiki-class の `Speaker` / `Microphone` トレイトを `kumiki-t5os` に実装すれば、kumiki のアプリのコードも動く（後で）。

## 5. 段取りと試験
| 段 | 中身 | 確かめ |
|---|---|---|
| A1 | 起動時のコーデックの設定、SPK_EN、画面の手順の bit1 を保つ、I2S を無音で回す | エミュレータ（I2C の記録）、実機で画面とタッチが今までどおり、コーデックが ACK を返す |
| A2 | `audio` オブジェクト、輪、割り込み、`beep`、`mic` | エミュレータ（I2S と AHB-GDMA のモデル: 記述子を 48 kHz で読み、EOF の割り込み） |
| A3 | 実機 | 利用者が `beep` の音を聞く。`mic` の値が声で変わる |
| A4 | 複数のプロセスの再生を混ぜる | エミュレータで 2 つの `beep` の和、実機で聞く |

- エミュレータのモデル（t5emu）: I2S0（クロックの設定は受け流し、送りの記述子を時間に合わせて読み進め、受けはゼロか正弦波を書く）、AHB-GDMA の記述子の輪と EOF の割り込み、ES8388 と ES7210 は書き込みを記録するだけ。
- カーネルが kumiki-p4v1-hal に依存すると、tab5-os のカーネルのビルドにも `../m5-rs` が要る（今はアプリだけ）。

## 6. 決めたこと（利用者、2026-11-09）
1. **音を混ぜる**: 最終的にできるようにする。`audio` を複数のプロセスが開け、カーネルが割り込みで各プロセスの再生の輪を足し合わせて（飽和させて）I2S に送る形にする。最初の実装は 1 つずつでもよいが、輪とオブジェクトの作りは複数を前提にする（段 A4）。
2. **前の席かどうかに関わらない**: 許可（`audio:out`、`audio:in`）を持っていれば、画面の席（doc/60）に関係なく使える。
3. **サンプリング周波数はハードウェアに依る**: カーネルは板とコーデックが扱える周波数で I2S を回し、`ctl(INFO)` で周波数と形式（16 ビットのステレオなど）をアプリに知らせる。変換はアプリの側で。
4. **コーデックの設定は kumiki が管理する**: ES8388 と ES7210 の設定（表とドライバ）を、カーネルからも使える形（no_std、esp-hal に依存しない、embedded-hal の I2C の上）で kumiki に出してもらう。カーネルは自分の I2C に embedded-hal の `I2c` を実装して使う。
5. **既定で有効**: 起動のたびに、画面より前にコーデックを設定し、I2S を回す。

## 7. 実装（段 A1 の前半、2026-11-09）
- **kumiki 側**（tab5-os の依頼で kumiki が入れた）: AHB-GDMA の割り込みの API と割り込み源の番号（rev v1 は IN_CHn 56 + n、OUT_CHn 59 + n）、`i2s::Config::dma_channel`、待ちのループの上限、`steal` を `Option` に（73dd3f9）。`ahb_dma::assume_initialized`（5263cf4）。`I2s::new_in` と公開の `DmaMem`（輪のメモリを呼び出し側が渡す、d6ea120）。
- **カーネル**: `kernel/src/platform/esp32p4v1/audio.rs`（Tab5 で chip v1 だけ。EVB と chip-v3 には入らない）。kumiki-p4v1-hal は `default-features = false` の任意の依存（`platform-esp32p4v1` で入る、`../../m5-rs` の path）。
  - 起動の順: `clock_init` の後、`board_init`（エクスパンダと画面の電源）の前に `audio_init`。コーデックの設定もここに入れる。
  - AHB-GDMA はカーネルが設定する（バスクロック、ブロックのリセット、窓 0x4000_0000〜0x4FFC_0000）。その後 `ahb_dma::assume_initialized()`。
  - 輪（約 33.5 KiB）は、PSRAM があるとき空く L2MEM（`APP_POOL`、0x4FF8_0000〜）の Wi-Fi の輪の後ろ（64 バイト境界、今は 0x4FF8_C100）。カーネルの .bss の残り（約 26 KiB）には入らないため。PSRAM がなければオーディオは始めない。
  - 割り込み: TX と RX の割り込み源をコア 0 の CPU の線 8 へ。`on_irq` が kumiki の `on_interrupt`（要因を消して poll）を呼ぶ。512 バイト（送り）と 256 バイト（受け）ごとで、48 kHz では毎秒約 1,100 回。**A1 では割り込みを有効にしない**（送りの輪は無音で始まり、誰も書かないので、poll することがない）。有効にしたら、40 MHz のエミュレータでは割り込みの道（トラップとスケジューラ）が CPU を大きく取り、matter の起動が約 3.2 秒から 5.5 秒に延びて p4emu-run-26 の競争に負けた。A2 では `audio` を開いたプロセスがいる間だけ有効にし、割り込みの回数も減らす（kumiki に、何枚かの記述子ごとに EOF を立てる形を相談する）。
  - 事象 `audio.started`（`rate_millihz`、`ring_us`、`area`、割り込み源）、`audio.failed`、`audio.stats`（ctl の新しい命令 `audio`、番号 37）。
  - `audio.stats` は先に poll する。割り込みがない間は poll がまばらで、kumiki の `tx_descs` は輪の周回を数え落とす（記述子の番地の差から数えるため）。
- **エミュレータ**: `emu/src/audio.rs`。AHB-GDMA（割り込みのブロック、リンク）と I2S0（`C_UPDATE`、`C_RESET`、`C_FIFO_RESET` は自分で消える）。I2S の `C_START` の間、48 kHz で記述子を進め、EOF の割り込みを上げる。受けは 440 Hz の音。`--audio-out FILE.wav` で鳴らしたものを残す。単体試験 `audio::tests`。
- **試験 p4emu-run-43**: `audio.started` が `board.ready` より前、約 48 kHz、`audio.stats` で割り込みが 0、`tx_underruns=0`、エミュレータの送りの記述子が 4 桁以上、鳴らしたものが全部無音。

### 7.1 Tab5 での確認（2026-11-09）
- `make flash-p4` で書き、起動した。ctl `audio` に `audio.stats irqs=0 tx_descs=25 tx_underruns=0` が返り、流れは動いている。音はまだ出ない（SPK_EN もコーデックも設定していない）。

## 8. 段 A2（2026-11-09、エミュレータで確認）
- **ABI 小版 27**: `t5_abi::audio`。`audio:out`（`write` で PCM のフレーム、16 ビットのステレオ、リトルエンディアン、左が先）と `audio:in`（`read`）。`ctl`: `RATE`（Hz）、`QUEUED`（out は空き、in は溜まり）、`VOLUME`（out、0〜100、既定 100）、`LOST`（in、捨てたフレーム）。`wait`: `WRITABLE`（キューの半分以上が空き）、`READABLE`（1024 バイトかキューの半分以上）。`audio:in` は強い許可（t5a.rs、試験 `audio_permits`）。前の席かどうかには関わらない。
- **設計からの変更**: §3 の `ctl(MAP)` の共有メモリの輪をやめ、ハンドルの `write` / `read` で写す。192 KB/s では数 ms ごとのシステムコールの費用は小さく、PMP の領域が要らず、混ぜるのもカーネルの中で済む。
- **カーネル** `kernel/src/audio.rs`: キューは out 4 個・in 2 個、各 8000 バイト（約 42 ms）。場所は DMA の輪の後ろの L2MEM（`audio.queues base=0x4ff94400`）。`poll`（スケジューラのループ、ストリームの割り込みの後、`write` / `read` の中）が、out のキューを音量を掛けて足し（飽和）、ストリームの空きに押し込む。受けたものは全部の in のキューへ写し、入らない分は数える。最初の `audio:in` を開くと、それまでに溜まった録音を捨てる。
- **割り込み**: 誰かが開いている間と、最後に押し込んでから 200 ms（`QUIET_MS`、送りの輪 2 周＋余り）は有効。kumiki は poll のたびに鳴らし終えた記述子を無音にする。poll がないと送りの輪は最後の 1 周を繰り返し続ける。
- **見つけた問題と対処**:
  1. **スケジューラのループは割り込みを止めたまま休む**（`smp::idle` の `wfi`）。割り込みは、プロセスが動いている間にしか受けない。最後のハンドルを閉じた後は何も kumiki を poll せず、輪が繰り返した（エミュレータの WAV で、85.33 ms ごとに同じ波形が続いた）。`audio::poll` が、割り込みが有効な間はループからも `Platform::audio_service`（kumiki の `on_interrupt`）を呼ぶ。
  2. **長い停止で kumiki が位置を見失う**。kumiki は、EOF の記述子の番地の差から済んだ記述子を数える。そのため、1 周（85 ms）より長く poll がないと周回を落とし、違う記述子を消したり書いたりする（最後の 1 周が繰り返す）。カーネルは長く止まることがある。アプリを読み込むときは、ロックを持ったまま署名を確かめる。その時間は Tab5 で小さいアプリで約 26 ms、大きいアプリ（blebk）では約 140 ms、40 MHz のエミュレータでは約 170 ms。前の poll から 3/4 周（64 ms）を過ぎていたら、poll の代わりにストリームを始め直す（輪を無音で作り直す。短く途切れる）。音を押し込んだ後の始め直しだけを数えて `audio.restart`（Debug）と `audio.stats` の `restarts` に出す。kumiki の `start` はチャネルの割り込みを切るので、始め直した後に有効に戻す。kumiki 側で、経った時間から周回を数えて poll を頑丈にしてもらう案もある（相談はこれから）。
- **アプリ**: `beep HZ MS`（正弦波、フルスケールの 1/4、キューが空くまで待って閉じる。`--permit audio:out`）。`mic MS`（100 ms ごとに左チャネルのピークと RMS と、それまでに落としたフレーム数を `mic.level` に出す。`--permit audio:in`、システムの鍵）。
- **試験 p4emu-run-44**: `beep 1000 1500` の 300 ms 後に `mic 500`。`beep.done frames=72000`、`mic.level` のピーク約 4096・RMS 約 2896（エミュレータの 440 Hz）、`mic.done lost=0`、`mic` の読み込みによる始め直しが 1 回。`tools/wavcheck.py` で、WAV の音が 1 kHz（±2 %）、ピーク 8000 以上、長さ 1700 ms 以下（実測 1591 ms、1.5 秒＋始め直しの途切れ）。
- **残り**: Tab5 で音を確かめるのは、コーデックと SPK_EN の後（段 A1 の残り）。割り込みの回数を減らす件（kumiki に相談済み）。段 A4（混ぜる）の試験。混ぜるのはもう実装してあるので、残るのは 2 つの `beep` の和を確かめることだけ。

### 8.1 Tab5 での確認（2026-11-09）
- カーネルを `make kupdate T5K=build/kupdate/good.t5k` で入れ（自己試験が通り `kupdate.otadata state=valid`）、`make install-apps`（入らなかったものなし）、`fault` を入れ直して再起動した。起動で `audio.started rate_millihz=48000000 ring_us=85333`、`audio.queues size=8000`、画面は今までどおり `display.ready`。
- `run beep 1000 1500` に続けて `run mic 500`: `beep.done frames=72000`、`mic.level` は 5 回とも `lost=0`、ピーク 200 前後・RMS 60 前後（コーデックを設定していないので、ES7210 が前の設定を保っているか雑音。声で変わるかは段 A1 の残りの後）。`audio.stats irqs=135 tx_underruns=1 restarts=0`（`rx_overruns` は誰も読んでいない間の受けの溢れで、kumiki の数え方）。音はまだ出ない。

