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
  | M2 | `ble` サービス（TrouBLE）とスキャン | 済み（§6） |
  | M3 | セントラルと GATT クライアント | 済み（§7。通知の受け取りは未確認） |
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

## 6. M2: `ble` サービスとスキャン
利用者の指示（2026-11-04）:「すすめて」

- **`ble`**（`user/apps/ble`、`--permit bt:hci --permit listen:ble --stack 65536`）:
  - TrouBLE（`trouble-host` 0.8.0、features: central、scan、gatt、default-packet-pool）と `bt-hci` 0.10.1。crates.io から（`Cargo.lock` で固定）。
  - **Transport**（`hci.rs`）: bt-hci の `Transport` を `bt:hci` の上に。`read` は 1 パケットを読み、種類のバイトから `PacketToHost::read_hci` で解く。`write` は種類のバイトと `write_hci` の結果を 1 回の `write` で送る（`Busy` なら 1 ms 待って再送）。
  - **executor**（`exec.rs`、この OS で初めての async。この中だけ）:
    - トップレベルの Future（TrouBLE の `Runner::run_with_handler` と、サービスの処理の `select`）を 1 つ回す。
    - Waker が呼ばれたら、すぐもう一度 poll する。呼ばれなければ、Pending を返した読み出しが登録したハンドル（`want`）だけを `wait` する。待ち時間は embassy-time の一番早い期限まで。
    - チャネルの相手が閉じたことは、`wait` のイベント（`PEER_CLOSED`）でしか分からない（`read` は `Busy` のまま）。executor が覚えておき（`closed`）、読み出しは `None` を返す。
    - embassy-time の時刻ドライバ（カーネルの時刻、1 MHz）と、何もしない `critical-section`（1 プロセス 1 スレッド）。
  - **`svc:ble`**（`t5_abi::ble`）: 今は `SCAN`（秒、能動か、間隔、窓）と `STOP` だけ。報告は `ADV`（アドレスの種類、アドレス、種類、RSSI、データ）を、頼んだクライアントへ 1 つずつ送る。チャネルが一杯なら、その報告は捨てて数える（ホストを止めない）。終わると `DONE`（送った数、捨てた数）。スキャンは同時に 1 つ。
  - 大きさ: コード 74 KB、データ 7.8 KB（`net` は 235 KB）。
- **`blescan`**（`--permit svc:ble`）: `run blescan [SECONDS [WINDOW_MS]]`。間隔 100 ms、窓 WINDOW_MS（既定 30 ms）で能動スキャンし、最初の 16 の広告主とデータ、合計を出す。
- **実機**（2026-11-04、`wifi 1` の後に `run ble`、`run blescan 10`）:
  - `ble.ready` から 4 ms で `ble.scan`。10 秒で報告 245、捨てた数 0。広告主のデータ（名前の入った広告など）も届いた。
  - `ble` は待っている間 CPU をほとんど使わない（起動から約 50 秒で累計 19 ms、`cpu_pm=0`）。executor は空回りしていない。
- **Wi-Fi との共存**（PC から ping を 50 ms おきに 200 回。§5 と同じ測り方）:

  | 状態 | 平均 | 最大 |
  |---|---|---|
  | スキャンなし（直前） | 16.0 ms | 281 ms |
  | 窓 30 ms、間隔 100 ms | 46.2 ms | 296 ms |
  | スキャンなし（直後） | 28.8 ms | 291 ms |
  | 窓 10 ms、間隔 100 ms | 26.0 ms | 64 ms |

  - この日の後半は、スキャンなしでも平均 16〜29 ms で揺れていた（§5 の朝の測定では 2.3 ms）。網そのものが揺れていて、細かい比較はできない。
  - それでも、スキャン中は遅くなり、窓を短くすると最大が下がる。既定の窓（30 ms）は見直す余地がある。
- **測り方の失敗（2 回目）**: フィルタを外している間に `blescan` を走らせたら、プロセスのスロット 8 つが全部埋まり（fs、wifi、net、font、term、shell、ble、blescan）、`run netctl 2` が `no free slot` で始まらなかった。気づいて戻すまでの約 1.5 分、フィルタが外れていた。戻した後は、`netctl.done mode=2 rules=4` と、PC からの ping が届かないことを確かめた。
  - スロット 8 つは、常駐のプロセスが増えると足りなくなる（Matter の `matter` サービスを足すとさらに）。上限を上げるかどうかは別に決める。

## 7. M3: セントラルと GATT クライアント
利用者の指示（2026-11-04）:「上限あげてよいです。BLEを先にすすめてください。Matterでいずれにせよ必要になります。」

- **プロセスの上限**を 8 から 12 に上げた（doc/30 §4）。1 スロット 2 MiB。`ppabench` は 2 つの続いたスロットを使う。
- **`svc:ble` の要求**（`t5_abi::ble`、接続は同時に 1 つ）:

  | 要求 | 答え |
  |---|---|
  | `CONNECT`（アドレスの種類、アドレス、秒） | `CONNECTED`（ATT MTU） |
  | `DISCONNECT` | `DISCONNECTED` |
  | `SERVICES` | `SERVICE`（番号、ハンドルの範囲、UUID）…、`END` |
  | `CHARS`（サービスの番号） | `CHAR`（値のハンドル、CCCD、性質、UUID）…、`END` |
  | `READ`（ハンドル） | `VALUE` |
  | `WRITE`（ハンドル、応答の有無、データ） | `WRITTEN` |
  | `SUBSCRIBE`（ハンドル、indication か） | `SUBSCRIBED`、その後 `NOTIFY` |

  - 失敗は `FAILED`（要求、理由: 1 不正、2 今はできない、3 スタックの失敗、4 時間切れ、5 見つからない）。
  - 相手が切れると `DISCONNECTED`。
  - Matter の BLE コミッショニング（BTP）は、C1 への書き込み（応答あり）と C2 の indication を使う。これで足りる見込み。
- **作り**（`ble`）:
  - `CONNECT` で TrouBLE の `Central::connect`（受け入れの表に相手だけ）。つながったら `GattClient::new`（ATT MTU の交換）。
  - つないでいる間は、`GattClient::task`（応答と通知を振り分ける）と、要求を処理する部分（`gatt`）を `select` で回す。リンクが切れると `task` が終わり、`DISCONNECTED` を送る。
  - 要求と通知の待ちも `select`。購読は同時に 1 つ（TrouBLE の既定）。通知の待ち行列は 4（feature `gatt-client-notification-queue-size-4`）。
  - `CHARS` で見つけた特性は、次の `SERVICES` まで積み上げておく。`WRITE`（応答なし）と `SUBSCRIBE` はここから探す。最初の版は最後の `CHARS` の分だけを持っていて、前のサービスの特性への `SUBSCRIBE` が「見つからない」になった。
  - `ble` のコードは 103 KB（M2 の 74 KB から増えた）。
- **`blegatt`**（`--permit svc:ble`）: `run blegatt [SECONDS]`。
  - 受け身のスキャン（間隔 100 ms、窓 20 ms）をして、一番強いつながる広告主（ADV_IND）につなぐ。
  - サービスと特性を一覧にし、読めるもの（12 まで）を読む。
  - 通知か indication のできる最初の特性を購読し（CCCD だけを書く）、3 秒待ってから切る。
  - よその機器なので、ほかの書き込みはしない。
- **実機**（2026-11-04）: 近くの Nature Remo（`Nature-BG189D970`、公開アドレス、RSSI -35）に:
  - 0.18 秒でつながり、ATT MTU 247。
  - サービス 3（GAP 0x1800、GATT 0x1801、0xFFFF）、特性 7。
  - Device Name（0x2A00）の値「Nature-BG189D970」など 5 つを読んだ。
  - Service Changed（0x2A05、ハンドル 8）の indication を購読できた。3 秒の間に indication は来なかった（来るのは GATT の表が変わったときだけ）。通知を受け取る道筋は、まだ確かめていない。
  - `DISCONNECT` で切れて、`DISCONNECTED` が返った。

## 8. 次
- 通知の受け取りを確かめる（通知を出す機器か、M4 のペリフェラルどうしで）。
- M4: ペリフェラル（広告、GATT サーバ）。Matter のデバイス側の BLE コミッショニングで要る。
- Matter（MT0〜）の BLE は、M3（コントローラ側）と M4（デバイス側）の上に作る。
