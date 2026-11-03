# 48. Bluetooth LE（HCI の通り道、M0 と M1）

利用者の指示（2026-11-04）:「並行して、BLEスタックまわりの実装方針について調べて」、「1. 実行してよいです。2. 一旦bleからasync始めましょう」

## 1. 方針
- **コントローラ**: ESP32-C6 の上の BLE のコントローラを使う。C6 のファームウェア（M5Stack が入れた esp_hosted 2.12.7）はそのまま使い、書き換えない。
  - 実機の INIT イベントの能力ビットは `capabilities=0x0D`（doc/22 §5.1）。esp_hosted の定義（`esp_hosted_transport_init.h`）では、bit0 が Wi-Fi over SDIO、bit2 が BT over SDIO、bit3 が BLE only。
  - P4 と C6 の間に UART はない（doc/06）。HCI は Wi-Fi と同じ SDIO を通す。
- **ホストスタック**: HCI の型は `bt-hci`、その上は TrouBLE（`trouble-host`）を、`ble` サービス（U モード）の中で試す（M2 以降）。
  - TrouBLE は async が前提（`embassy-sync`、`embassy-time`、`Runner::run()`）。この OS はどこにも async を使っていない。ABI は `wait`/`poll` のイベント待ち（doc/04）。
  - 利用者の判断: async は、まず `ble` の中だけで始める。最小の executor（`wait` を Waker の元に）、`embassy-time` の時刻ドライバ、何もしない `critical-section` を `ble` の中に置く。ほかに使うものが出たら、`t5_rt` に移すかを決める。
  - アプリには `svc:ble` の決まりごとだけを見せる。TrouBLE が合わなければ、`ble` の中身だけを自作のポーリング型に替える。
- **段取り**:

  | 段 | 内容 | 状態 |
  |---|---|---|
  | M0 | 実機でコントローラを起動し、HCI が往復する | 済み（§4） |
  | M1 | カーネルのオブジェクト `bt:hci` と、生の HCI を送る試験アプリ | 済み（§3、§4） |
  | M2 | `ble` サービス（TrouBLE）とスキャン | まだ |
  | M3 | セントラルと GATT クライアント | まだ |
  | M4 | ペリフェラル（広告、GATT サーバ） | まだ |
  | M5 | ペアリング（LE Secure Connections） | まだ |
  | M6 | on/off と、Wi-Fi との共存 | 測っただけ（§5） |

## 2. esp_hosted の HCI
- if_type 4（`ESP_HCI_IF`）。ヘッダは Wi-Fi と同じ 12 バイト。
- **ホストから C6 へ**: H4 のパケットの種類（コマンド 1、ACL 2、…）を、ヘッダの最後のバイト（オフセット 11、`hci_pkt_type`）に置く。本体は種類のバイトを除いたもの。
  - C6 は本体の 1 バイト前（ヘッダの最後）を含めて、コントローラに渡す（`slave/main/slave_bt.c` の `process_hci_rx_pkt`、`payload--`）。
- **C6 からホストへ**: 本体の先頭が種類のバイト（`host_rcv_pkt`）。
- **コントローラの起動**: esp_hosted 2.5.2 から、ホストが RPC で頼む。
  - `Req_FeatureControl`（387）、`feature = 1`（Bluetooth）。`command = 1`（BT の Init）、続けて `command = 3`（BT の Enable）。
  - 応答 `Resp_FeatureControl`（643）のフィールド 1 が esp_err_t。
  - 定義は手元の esp_hosted 2.12.7（C6 と同じ版）の `common/proto/esp_hosted_rpc.proto:55-84, 262, 1674-1685`。
- **送信の抑制**: C6 が「待って」と言っている間（throttle）、今まではどの種類の送信も断っていた。ESP-IDF のホストと同じく、局（STA）のフレームだけを止めるようにした（`sdio::send`、`transport_drv.c:341`）。

## 3. 実装
- **カーネル**（`kernel/src/wlan.rs`）:
  - `wlan::poll` の振り分けに if_type 4 を足した。開いていれば `bt:hci` の待ち行列（6 個 × 258 バイト）に入れる。最初の 4 つを Debug の `wlan.hci_rx` に出す。
  - `sdio::send` と `Platform::wlan_send` に、ヘッダの最後のバイトの引数を足した。
  - オブジェクト `bt:hci`（`BtHci`、権限 `bt:hci`、同時に 1 つ）。`read`/`write` は H4 の形（種類のバイト、パケット）で 1 つずつ。`write` は種類のバイトをヘッダへ移す。C6 が動く前は `NoDevice`。閉じると `wlan.hci_closed`（受信、送信、落とした数）。
  - ABI 小版 19（`t5_abi::bt`: `HCI`、`CMD`〜`ISO`、`PKT_MAX` = 258）。
- **`wifi`**: 引数 `arg0` の bit 0 が立っていれば、チップの INIT イベントの後、Wi-Fi の初期化の前に、BT の Init と Enable を送る（`start_bluetooth`）。成功で `wifi.bt_started`、失敗は `wifi.bt_failed`（Wi-Fi は続ける）。
  - 既定（引数なし）は今までどおり、BT を起動しない。起動時に BT を使うには、`START.CFG` の行を `wifi 1` にする。
- **`hcitest`**（`--permit bt:hci`）: `run hcitest [SECONDS]`。
  - HCI_Reset、Read_Local_Version_Information、Read_BD_ADDR を送り、結果を出す。
  - SECONDS があれば、イベントマスクに LE Meta を足し、能動スキャン（間隔と窓 30 ms、重複を除く）を SECONDS 秒して、最初の 12 の広告主（アドレス、種類、RSSI）と報告の数を出す。

## 4. 実機の結果（2026-11-04）
- `kill`（`wifi`）、`run wifi 1`: 3 秒後に `wifi.bt_started`（Init、Enable とも 0）。続く Wi-Fi の初期化（動いている C6 にもう一度）も通り、5 秒後に再接続した。
- `run hcitest 10`:
  - HCI_Reset に Command Complete（`04 0e 04 01 03 0c 00`）。
  - 版: HCI 14、LMP 14（Core 6.0）、製造者 741（0x02E5、Espressif）。
  - BD_ADDR: `e4:b3:23:9a:1f:26`（Wi-Fi の MAC の 2 つ先）。
  - 10 秒のスキャンで、広告主 12 以上、広告の報告 244。`bt:hci` で落としたものは 0（`wlan.hci_closed rx=251 tx=7 dropped=0`）。
- コントローラは、C6 を再起動する（Tab5 の再起動）と止まる。

## 5. Wi-Fi との共存（測っただけ）
PC（有線、同じ LAN）から Tab5 へ、50 ms おきに ping を 200 回。測る間だけ `run netctl 3` でフィルタを外し、`run netctl 2` で戻した。

| 状態 | 平均 | 最大 | ロス |
|---|---|---|---|
| BT なし（再起動の後、2 回目） | 2.3 ms | 12 ms | 0 % |
| BT あり、何もしない（2 回目） | 2.5 ms | 13 ms | 0 % |
| BT あり、スキャン中（窓 = 間隔 = 30 ms） | 30.9 ms | 282 ms | 1 % |

- どちらの状態でも、Wi-Fi の（再）接続の後の 1 回目は遅かった（平均 7〜11 ms、最大約 290 ms、ロス 1〜1.5 %）。BT によらない。
- BT を起動しただけでは、Wi-Fi は遅くならない。調査で心配した「BT を有効にすると `WIFI_PS_NONE` が効かなくなる」は、この測り方では見えなかった。
- 窓と間隔が同じスキャンは、無線を BLE がずっと使うので、Wi-Fi が大きく遅れる。M2 のスキャンは、窓を間隔より短くする（例えば窓 30 ms、間隔 100 ms 以上）。
- 測り方の失敗: 3 回目の ping の後、フィルタを戻すコマンドが、裏で動いていた別の t5log（`hcitest` を走らせたもの）とプローブを取り合って届かなかった。気づいて戻すまでの 1〜2 分、フィルタが外れていた。プローブを使う t5log は 1 つずつにする。

## 6. 次（M2）
- `ble` サービス: `bt-hci` と TrouBLE を `third_party/` に置き、版を固定する。`ble` の中に executor、時刻ドライバ、`critical-section`。
- `svc:ble` の決まりごと（スキャンの開始と結果、の最小から）。
- コードの大きさと RAM、取りこぼしを測り、TrouBLE を続けるかを決める（判定の基準: 土台が無理なく書けること、取りこぼしや遅れがないこと、大きさ）。
