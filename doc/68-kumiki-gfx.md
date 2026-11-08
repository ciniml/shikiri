# 68. kumiki-gfx を tab5-os のアプリで使う

利用者の指示（2026-11-08）:「Rust用ドライバ群 kumiki を作っています。現在P4対応中です。tab5-osで使えるかコードを確認してみてください。」→ 方針の検討（kumiki のセッションと共有）→「すすめて」。

## 1. 方針（kumiki のセッションと合意）
- embedded-hal と kumiki のトレイト（`FramebufferPanel` など）を、kumiki と各プラットフォームの境目にする。kumiki のドライバは資源（バス、ピン、`DelayNs`、パネル）を渡されて使い、自分でレジスタやグローバルな状態に取りに行かない。ベアメタル（`kumiki-p4v1-hal`）、esp-hal、tab5-os の同じコードで動く。
- tab5-os 向けの実装は tab5-os 側に置く（tab5-os の ABI はよく変わるので、kumiki を tab5-os に依存させない）。
- kumiki-p4v1-hal はアプリでは使えない（U モードは MMIO に触れない）。カーネルで使うには、`rt` のフィーチャ化（済み、kumiki 68db066）、コンソールの差し替え（済み）、DMA と PPA の開始／確認／待ちの分割（済み）、低い層の切り出しとキャッシュ保守のトレイト（まだ）が要る。DMA を持つものはカーネルに残す（P4 に IOMMU がない）。

## 2. 段 1: `kumiki-t5os::FbPanel`（2026-11-08）
- `user/kumiki-t5os`: tab5-os の `display` を kumiki-gfx の `FramebufferPanel` にする。`t5_rt` には入れない（全部のアプリが kumiki に依存しないように）。
  - `back_buffer`: 写像したフレームバッファ（パネルの 720 x 1280、RGB565 の little-endian = `Rgb565Le`）。
  - `flush_cache`: 矩形の行の `FLUSH`。
  - `present`: 何もしない（席ごとに 1 枚）。
  - `hw_fill` / `hw_copy`: 2D の要求 `FILL` / `COPY`（パネルの座標で直に組む。`t5_rt` の `start_fill` は利用者の座標を取るため）と `DONE` を待つ。カーネルが 2D の前に行をキャッシュから書き戻し、宛て先の行を捨てるので、kumiki の契約を満たす。
  - CPU に任せる（`false`）: パネルの座標で幅 64 画素未満（書き戻しは行の単位なので、狭くて高い矩形は高くつく）、右への重なりのあるコピー（カーネルで細切れになり CPU より遅い、doc/44 §3）、カーネルが断ったもの、失敗したもの。`hw_threshold` は 64 x 64 = 4096 画素（kumiki の DSI パネルと同じ。往復は数十 µs の推定）。
  - `rotation()`: `Display::quarter` を kumiki の `Rotation` に（`FbDisplay::with_rotation` に渡す）。Tab5 の 270° は kumiki のベアメタルと同じ向き。
  - 席の取り上げ（doc/60 §10）: 2D の要求を待つ間に来た `SHOWN` 値 1 を覚え、`take_redraw` で返す。アプリは `FbDisplay::invalidate`（kumiki 19f5e1d）の後に全部描き直す。
  - `FRAME` の事象は切る（`display::FRAMES` 0）。
- `kgfx`（デモ、`--permit display`）: kumiki の `FbDisplay` で、背景、上の帯、色の帯、利用者の左上の白い四角、行き来する四角を描く。`SHOWN` 1 で描き直す（`kgfx.redraw`）。
- **依存**: kumiki は tab5-os の隣（`../m5-rs`）に置く前提の path 依存（`user/kumiki-t5os/Cargo.toml`、`user/apps/kgfx/Cargo.toml`）。kumiki を公開したら git かレジストリの依存に替える。

## 3. 試験
- p4emu-run-40: `kgfx` の写し（利用者の向き）の左上が白、背景、左下の赤の帯。3 秒で 72 フレーム（エミュレータの 40 MHz）。
- p4emu-run-41: `kgfx` が前（0 枚目）、`paint` 102 を前（1 枚目）にして、もう 1 つの `paint` が 0 枚目を `kgfx` から取る（`screen.taken_away`）。`kgfx` は描いている最中だったので次の書き込みで止まり（`screen.no_screen`）、`focus 101` で続きから動き、`SHOWN` 1 で全部描き直した（`kgfx.redraw`）。写しに `kgfx` の画面。

## 4. 実機（Tab5、2026-11-09）
- `run kgfx 120` と `focus`（ホストから）で前に出し、利用者が画面で確かめた（「うごいてます」）: 利用者の向きで上の帯と左上の白い四角、下の色の帯、行き来する四角。30 秒で 747 フレーム（約 25fps、待ちの 40ms の刻みで決まる）。

## 5. 次
- IMU: カーネルの `i2c` オブジェクト（デバイスのアドレスごとの許可、例 `i2c:int/0x68`）、`t5_rt::I2cDevice`（embedded-hal の `I2c`）、`kumiki-bmi270` をアプリで。
- 2D の閾値の実測。
