# tab5-os 調査ドキュメント

M5Stack Tab5（ESP32-P4）で動く Rust 製の自作 OS（カーネルは M モード、アプリは U モード、アプリの動的ロード、syscall、更新）に向けた調査と提案をまとめたもの。

| 章 | 内容 |
|---|---|
| [01](01-cpu-privilege-and-protection.md) | CPU の特権モード、PMP/PMA、APM、CLIC、ecall、ESP-TEE、ESP Privilege Separation |
| [02](02-memory-map.md) | アドレスマップ、L2MEM、キャッシュ、外部メモリ MMU、アプリをどこに置くか |
| [03](03-rust-ecosystem.md) | esp-hal の P4 対応（v3.0 以上のみ）、ターゲット、riscv クレート、既存 OS（Tock、Hubris、Xous など）の比較 |
| [04](04-dynamic-loading-and-abi.md) | 動的ロードの方式（静的 PIE）、アプリフォーマット T5A 案、syscall ABI 案、Zephyr のユーザモード |
| [05](05-update-and-boot.md) | ESP-IDF のブートローダと OTA、署名、ロールバック、アプリ単体の更新 |
| [06](06-tab5-hardware.md) | Tab5 のハードウェア、ピン割り当て、初期化手順、コンソール、ドライバの有無 |
| [07](07-architecture-proposal.md) | **結論: アーキテクチャ案、メモリマップ、syscall 初版、マイルストーン** |
| [08](08-emulator.md) | エミュレータ（Espressif QEMU は P4 非対応、上流 QEMU virt でアーキ層を検証、Wokwi、Renode、Hosted） |
| [09](09-bringup-m1.md) | M1 の実機立ち上げの記録（U モードと PMP が実機で動いた。キャッシュ同期とブートローダの 2 セグメント要件） |
| [10](10-m2-scheduling.md) | M2: SYSTIMER と CLIC によるプリエンプション、sleep/yield/time、FPU の遅延切り替え（QEMU と実機で PASS） |
| [11](11-emulator.md) | 自作エミュレータ t5emu（P4 v1.x）。実機用カーネルを無改造で実行して PASS。キャッシュ非一貫性、RWDT、lock-up の検出 |
| [12](12-structured-logging.md) | 構造化ログサービス（テキストログを廃止。スキーマは ELF、LP SRAM のリング、USB/RTT/前回起動の回収）。L1（USB）、L2（RTT を probe-rs で回収）、L3（lock-up → RWDT のリセット → 次の起動で直前のログを回収）を実装済み |
| [13](13-probe-rs-p4.md) | probe-rs の P4 対応の調査（上流は 0.31.0 から対応済み。v1.x の L2MEM はプログラムバッファ経由、LP SRAM はメモリマップにない）。実機で LP SRAM の RTT をコアを止めずに読めた |
| [19](19-n0-ipc-design.md) | N0: サービスのための IPC。N0a（サービスチャネル `listen:`/`svc:`、チャネル 32 本）と N0b（共有メモリ。切り出して渡す）を実装 |
| [20](20-n1-net-service.md) | N1: ネットワークサービス `net`（smoltcp、ループバック、TCP/UDP の socket API）。t5emu で確認 |
| [research/net-stack](research/net-stack.md) | ネットワーク・スタックの検討（smoltcp を U モードの `net` サービスで使い、ルーティング・転送・フィルタは自作。最初のリンクは USB Serial/JTAG 越しのホスト、本命は ESP32-C6 の Wi-Fi。段階 N0〜N5） |
| [18](18-m7-sdcard-design.md) | M7: microSD とファイル。M7a〜M7c を実装し実機で確認（4 ビット 40MHz、`sd` オブジェクト、U モードのファイルサービス `fs` と `Files`）。調査は [research/m7-sdcard.md](research/m7-sdcard.md) |
| [17](17-m6-display-design.md) | M6 の設計（表示とタッチ。M6a: I2C・IO エクスパンダ・パネルの判別・タッチ、M6b: VO3・DSI・DW-GDMA で表示、M6c: アプリへの display/touch）。調査は [research/m6-dsi.md](research/m6-dsi.md) と [research/m6-panel-touch.md](research/m6-panel-touch.md) |
| [16](16-m5-update-design.md) | M5 の設計（カーネルの A/B 更新とロールバック、T5K パッケージ、アプリの A/B、IPC）。M5a（カーネルの A/B）は実機で更新・確定・3 通りのロールバックを確認。M5b（アプリの A/B）は実機でも確認。M5c（IPC チャネル）を実装し、QEMU・t5emu・実機で確認 |
| [15](15-m4b-xip.md) | M4b: アプリの text をフラッシュから直接実行し、data を PSRAM に置く（XIP）。実装し、実機で確認。M4c（PVT と 360MHz）は [research/m4-psram-and-clock.md](research/m4-psram-and-clock.md) §10 |
| [14](14-m3-design.md) | M3 の設計（T5A v1 の署名、アプリストア、制御チャネル、インストール、ハンドル。フラッシュの MMU と書き込みの手順）。M3a（署名と検証）、M3b（フラッシュのアプリストアから起動）、M3c（RTT のコマンドで、再起動なしに起動・停止）、M3d（制御チャネルからのインストール、ハンドルとタイマ）を実装済み。M3 完了。M4a（PSRAM）は [research/m4-psram-and-clock.md](research/m4-psram-and-clock.md) |

記述ルール:
- 事実には出典（URL、TRM の章・ページ、ローカルのパス:行）を付ける。
- 推測には「（推測）」、設計判断には「（判断）」と付ける。
- 章どうしで食い違う箇所には「統合時注記」を入れ、07 で決着させる。

調査日: 2026-10-29。esp-hal、Zephyr、M5GFX などは動きが速いので、使う前に最新状況を確認すること。
