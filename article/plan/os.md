# tab5-os の OS としての機能の記事の計画 (os.md)

対象: このリポジトリ (tab5-os) の kernel/、abi/、user/、emu/、tools/ と doc/。
書き方は gowin_serdes_booklet の GUIDE.md と usb3.md に合わせる (です・ます、1 文 1 行、英数字と和文の間を空けない、pandoc-crossref の図表)。
ESP-IDF の 2nd stage bootloader、smoltcp、embedded-sdmmc、simple-matter、esp_hosted (C6 のファーム) は筆者の設計ではないので「使った」とだけ書く。

## 読者と分量
- C か Rust が読め、マイコンのレジスタを触ったことがある人。OS の内部 (特権モード、MMU/PMP、システムコール) は前提にせず、基礎の節で説明する。
- 700〜830 行。

## (a) 伝えたいこと (節ごとに 1 つ)
1. 導入: MMU のないマイコン (ESP32-P4) でも、PMP と署名でアプリを互いに守る OS が作れる。仕様の表。
2. 基礎: M モードと U モード、PMP、ecall。MMU がないので仮想記憶はなく、アドレスは物理のまま、守るのは PMP。
3. プロセスとメモリ: アプリは静的 PIE、カーネルが PSRAM のスロットにコピーして再配置し、TOR 3 本で text R-X、data RW。
   出典: doc/07 §1、§3 (プロセスの配置と PMP)、kernel/src/loader.rs、kernel/src/arch/riscv32/pmp.rs、kernel/src/sched.rs (MAX_PROCS = 12)
4. スケジューラ: SYSTIMER のアラームで 10ms の時分割、2 コアは大きなロック 1 つ、CPU の取り分 (system 30%、net 40%、apps 30%、100ms の窓)。
   出典: kernel/src/sched.rs (SLICE_MS = 10)、doc/10、doc/33、doc/57
5. システムコールとハンドル: 呼び出しは 14 個 (EXIT〜CTL)。デバイスもサービスもタイマも、名前で open してハンドルで扱う。1 プロセス 16 個。
   出典: abi/src/lib.rs (sys、MAX_HANDLES)、kernel/src/handle.rs
6. IPC: チャネル (256 バイト × 4 通)、サービス (listen:/svc:)、共有メモリ。ドライバの多くはカーネル、ネットワークやファイルや設定はユーザー空間のサービス。
   出典: abi/src/lib.rs (chan)、doc/19、kernel/src/channel.rs (MAX_CHANNELS = 32)
7. 権限: アプリの像 (T5A) は Ed25519 で署名し、許可 (Permissions TLV) を名乗る。システムの鍵とアプリの鍵、強い許可、サービスの grant。
   出典: doc/14 §1.3、doc/52、abi/src/t5a.rs (strong)
8. 更新: アプリの A/B (確かめ待ち、落ちたら戻す)、カーネルの A/B (自己試験で確定、だめなら戻す)、OTA (update オブジェクト、網で取ったカーネルは網に戻れてから確定)。
   出典: doc/16、doc/65、doc/67、kernel/src/store.rs、kernel/src/kupdate.rs、kernel/src/update.rs
9. 構造化ログ: テキストのログをやめ、event! の値だけを送る。形式は ELF に置き、ホストで戻す。
   出典: doc/12、t5_abi::log
10. 画面の席: 表示と入力を前のアプリだけに。
   出典: doc/60
11. 検証: 自作エミュレータ t5emu が実機用のカーネルを無改造で動かす。p4emu の試験 39 本、起動時の自己試験。
   出典: doc/11、doc/59、Makefile (P4EMU_RUNS)、kernel/src/main.rs (selftest)
12. 実機で見つかった不具合 (症状・原因・対策):
    - ブートローダが PMP をロックして U モードに RW を与えていた → ブートローダを作り直した (doc/07 §0、doc/09)
    - デバッガで止めて再開すると、プロセスが M モードで動いた (doc/36)
    - ストアの目録が満ちて更新が入らない (doc/65)
    - ota がスタックを溢れさせて落ちた (doc/67 §5)
13. 性能: 表示 64fps (doc/17)、PPA の全画面の塗りつぶし 5.1ms (doc/44)、何もしないときの待ち 99.2% (doc/31)、有線の TCP echo 約 36Mbps (doc/58)、メモリ (doc/61)。
14. 残っている課題: 標準入出力 (doc/66)、ドライバの分離 (kumiki との議論)、MMU がないので DMA を持つドライバはカーネルに置くしかない。

## (b) 図の一覧
- fig:os_overview: カーネル (M モード) と、その上のサービス・アプリ (U モード)、ハンドルとチャネル。figure/gen_os.py で SVG を生成。
- fig:os_process: プロセスのスロットと PMP の範囲。figure/gen_os.py。

## (c) 要確認事項
- 記事の題、掲載先、読者 (FPGA の本に入れるのか、別の本か)。
- 筆者名の扱い (書かない決まり)。
- システムコールの時間 (doc/10 の 7.5µs は 360MHz にする前の値なので書かない)。
