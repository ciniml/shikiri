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

記述ルール:
- 事実には出典（URL、TRM の章・ページ、ローカルのパス:行）を付ける。
- 推測には「（推測）」、設計判断には「（判断）」と付ける。
- 章どうしで食い違う箇所には「統合時注記」を入れ、07 で決着させる。

調査日: 2026-10-29。esp-hal、Zephyr、M5GFX などは動きが速いので、使う前に最新状況を確認すること。
