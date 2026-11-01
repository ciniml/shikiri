# third_party

外部のクレートを、直す必要があったものだけここに置く。`user/Cargo.toml` の `[patch.crates-io]` で使う。

| クレート | 元 | 変更 |
|---|---|---|
| `smoltcp-0.14.0` | crates.io の smoltcp 0.14.0（0BSD） | `src/iface/slaac.rs` の `Slaac::poll_at` だけ。ルータ要請（RS）を送り切った後、`retry_rs_at` が過去のまま返り、`Interface::poll_delay` が 0 になり続ける。ルータ広告（RA）が来るまで呼び出し側が回り続けた（`net` が Wi-Fi の接続待ちの間に約 7 秒の CPU を使った、doc/34）。送り切った後は時刻を返さない。上流の main も同じ（2026-11-01 に確認） |
