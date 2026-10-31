# 21. N2a: ホストリンク（コンソールの線の上の IP）

[research/net-stack.md](research/net-stack.md) §4 の段階 N2a。Tab5 の最初の実リンクで、PC と Tab5 の間で IP パケットを通す。線は USB Serial/JTAG（USJ）のコンソール（PC からは CDC-ACM の `/dev/ttyACM3`）で、構造化ログ（[12](12-structured-logging.md)）と同じ線に相乗りする。

## 1. 線の上の形式
- 両方向ともログと同じ枠: COBS で符号化し、フレームの間に 0x00 を置く（`t5_abi::log` の `cobs_encode`/`cobs_decode`）。
- **パケットのフレーム**は、復号した先頭の 1 バイトが `0xFF`（`PACKET`）で、残りが IPv4 のパケットそのまま（Ethernet のヘッダなし）。ログのレコードの先頭のバイトは `level<<5|count` で、level は 4 以下なので 0xFF にならない。これで受け手はフレームの種類を見分ける（判断）。
- MTU 1500。符号化したフレームは最大 `1500 + 1 + 1500/254 + 2` バイト（kernel/src/hostlink.rs:22）。
- PC から Tab5 へは、今はパケットのフレームだけを送る。他のフレームは数えて捨てる（`other`）。

## 2. カーネル（kernel/src/hostlink.rs、ABI 0.11）
- 受信: USJ の OUT エンドポイント。`EP1_CONF.SERIAL_OUT_EP_DATA_AVAIL`（bit2）が立っている間 `EP1` から 1 バイトずつ読む（kernel/src/platform/esp32p4v1.rs:476、出典は usb_serial_jtag_reg.h:50）。
- `hostlink::poll()` をスケジューラのループから呼ぶ（kernel/src/sched.rs:126）。1 回に 64 バイト × 32 回まで読む（洪水でループを止めないため）。**サービスが開いている間だけ**読む。開いていなければ、バイトはハードウェアに残る。
- **受信で眠りから覚める**: 開いている間は USJ の `SERIAL_OUT_RECV_PKT` 割り込み（INT_ENA/INT_CLR の bit2、usb_serial_jtag_reg.h:319,439）を有効にする。割り込み元は `ETS_USB_SERIAL_JTAG_INTR_SOURCE` = 22（interrupts.h。`ETS_TEMPERATURE_SENSOR_INTR_SOURCE` は別名なので数えない）で、CPU の線 3（CLIC の番号 19）に回す。タイマと同じく MIE=0 のままで、`wfi` を起こすためだけに使う。
  - `console_read` は読む**前に**割り込みを消す。読んだ後に届いたパケットは割り込みをもう一度立てるので、その後の `wfi` が取り逃がさない。
  - 読み残し（上限に達したとき）があれば、スケジューラは眠らずにループを回す（`hostlink::pending()`）。
  - U モードの実行中に来たら、`handle_irq` で消してそのプロセスに戻る。読むのは次のループ。
  - これが無い間は、パケットはタイマで起きるまで（`POLL_MS` = 10ms か、`net` の待ちの時間切れ）待たされていた。
- 受け取ったパケットは固定の待ち行列（1500 バイト × 8）に入れる。一杯なら捨てて `dropped` を数える。
- オブジェクト `netdev:host`（`t5_abi::netdev`）: 一度に一人だけが開ける（2 人目は `Busy`）。権限は READ/WRITE/WAIT。
  - `read`: 1 パケットを取る。無ければ `Busy`。バッファが小さければ `Size`（パケットは残る）。
  - `write`: 1 パケットを `[0xFF] + packet` で符号化してコンソールに書く。
  - `wait`: 待っているパケットがあれば `READABLE`（値はその数）。
  - 閉じると `hostlink.closed`（received、dropped、bytes、other）を出す。最初のバイトと最初のパケットでも 1 回ずつイベントを出す（`hostlink.first_bytes`、`hostlink.first_packet`）。
- 作業用のバッファ（復号・符号化）は静的に置く。最初はスタックに置いていて、カーネルのスタックの使用が 49152 バイト中 43840 まで達した。静的にして 41120 に下がった（実機の `kernel.stack`）。

## 3. ネットワークサービス `net`
- 許可に `--permit netdev:host` を足した（Makefile の `PERMIT_net`）。`netdev:host` を開けたときだけ、ホストのリンクを使う。
- 1 つの smoltcp のインタフェースに 2 つのアドレス（127.0.0.1/8 と 10.55.0.2/24）を持たせ、その下の装置 `Links` が 2 本のリンクを束ねる（user/apps/net/src/main.rs）。
  - 受信: ループバックの輪を先に見て、次に `netdev:host` を `read` する。
  - 送信: IPv4 の宛先（16〜19 バイト目）が 127/8 でなければホストのリンクへ、そうでなければループバックへ。経路表は N3 で作る（判断）。
- `wait` の対象に `netdev:host` を加えた。パケットが来たら起きて smoltcp を `poll` する。
- smoltcp の feature `auto-icmp-echo-reply` を有効にした。これが無いと smoltcp は ICMP の echo に答えない（smoltcp 0.14.0 src/iface/interface/ipv4.rs:345 の `#[cfg]`）。最初の実機の試験で ping が通らなかったのはこれが原因。
- 調べるために、ホストのリンクの最初の数パケットを Debug で記録する（`net.host_rx`: 長さ・版・プロトコル・宛先、`net.host_tx`: 長さ・成否）。送信に失敗したときは毎回記録する。

## 4. PC 側: `t5log hostlink`（tools/t5log/src/hostlink.rs）
- シリアルを raw モードで開き、TUN の装置（`IFF_TUN | IFF_NO_PI`）につなぐ。
  - TUN → シリアル: 別のスレッドでパケットを `[0xFF] + packet` の COBS フレームにして書く。
  - シリアル → TUN: パケットのフレームは TUN へ書き、それ以外のフレームはログとして `t5log decode` と同じように表示する。
- TUN の装置は一度だけ root で作り、利用者のものにしておく。その後は root なしで動く。

```
sudo ip tuntap add dev t5 mode tun user $USER
sudo ip addr add 10.55.0.1/24 dev t5
sudo ip link set t5 up
tools/target/release/t5log hostlink --port /dev/ttyACM3 --tun t5 --elf build/t5-kernel-esp32p4v1.elf --elf <net の ELF> ...
```

- 注意: `t5log hostlink` が動いている間はシリアルのポートを握っている。その間 `espflash`（`make flash-kernel-p4`）は接続できずに止まる。実際に一度、otadata の消去の前で何時間も止まった（何も書かれていなかった）。書き込みの前に `t5log hostlink` を止めること。RTT（`t5log rtt`）は JTAG なので並行して使える。

## 5. 実機の結果（2026-10-31、360MHz）
TCP の echo は `nettest 1`（ポート 7777）に PC から乱数を送り、戻りが一致することを確かめた（scratchpad の小さな Python）。

| | ping（5 回） | TCP 64KiB | TCP 1MiB |
|---|---|---|---|
| 受信の割り込みなし | 平均 2.4ms（0.97〜5.1ms） | 646ms | 10.0 秒（約 0.84Mbps） |
| 受信の割り込みあり（§2） | **平均 0.41ms**（0.38〜0.51ms） | 266ms | **3.6 秒（約 2.3Mbps）** |

- どちらも全部一致した。`net.host_tx` に失敗はなく、`hostlink.closed` の `dropped` は 0。
- 割り込みありで research の見込み（2〜6Mbps）の範囲に入った。echo なので、同じ線の上で両方向に同時に流れている。
- 割り込みを足した後も `make test-p4emu` は全部通る（t5emu の USJ は割り込みを出さないので、そこでは前と同じ動き）。
- ログは同じ線で読めている。ただし `t5log hostlink` が動いていない間（誰も読まない間）は、コンソールのログは捨てられる（`log.dropped sink=console`）。RTT のほうには残る。
- 書き込み・インストール・実行は、利用者が足した許可（`t5log rtt --probe <Tab5>`）で Claude が行った。TUN の装置は利用者が作った。

## 5.1 t5emu の中の PC 役（`--host-peer BYTES`）
- emu/src/hostpeer.rs: smoltcp 0.14（`net` と同じ版、ホスト側は std）のインタフェース 10.55.0.1/24。パケットは `t5log hostlink` と同じ形のフレームで USJ を行き来する。PC からの分は 64 バイト（USB の 1 パケット）ずつ入れる。
- USJ のモデル（emu/src/periph.rs）に受信を足した: OUT の FIFO（`EP1` の読み出し、`EP1_CONF` の DATA_AVAIL）、`INT_RAW`/`INT_ST`/`INT_ENA`/`INT_CLR` の `SERIAL_OUT_RECV_PKT`、割り込み元 22。Tab5 からのパケットのフレームはログの出力から外すので、RTT との突き合わせはそのまま通る。
- 手順: `nettest.listening` が出たら ping を 3 回、次に 7777 番へ 16KiB を送り、戻りを照合する。終わったら走行を止める。答えが 5 秒（TCP は 20 秒）来なければ失敗。
- `make test-p4emu` に入れた。結果（40MHz）: ping 0〜1ms、16KiB の echo が 69ms（模擬時間）。

## 6. まだ
- **さらに速く**: 今は受信を 1 バイトずつレジスタから読み、送信も 1 バイトずつ書いている。`net` は要求ごとに 1 パケットずつ `read`/`write` する。どこが効いているかは測っていない（推測の候補: USB FS の 64 バイトのパケット、システムコールの回数、TCP の窓）。今の用途（開発中の PC とのやりとり）には足りるので、後回しにする（判断）。
- PC から Tab5 へログ以外の制御（コマンド）を同じ線で送ること。今は RTT を使っている。
- 次は N2b（Wi-Fi）。エクスパンダ #2 の bit0（WLAN_PWR_EN）の書き込みには利用者の承認が要る。C6 のファームウェアの版も確かめる必要がある。
