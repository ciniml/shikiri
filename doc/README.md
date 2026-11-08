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
| [22](22-n2b-wifi.md) | N2b: Wi-Fi（ESP32-C6、SDIO、esp_hosted）。C6 の識別、esp_hosted の転送、`wifi` サービスで AP に接続、`net` の Wi-Fi のリンクで DHCP と外への TCP。データが通らなかった原因（GetMAC の引数の取り違え）と切り分け |
| [23](23-n3-forwarding.md) | N3: リンク間の転送（経路表、ARP、TTL、ICMP のエラー）、待ち受けのアドレスとリンクの指定、`net` の中の試験用 LAN。t5emu で PC → Tab5 → 試験用 LAN を確認 |
| [24](24-n4-filter.md) | N4: パケットのフィルタ（in/forward/out、規則、規定）と接続の追跡、`svc:netctl` と `netctl` アプリ、別のリンクの自分のアドレスへの配送。t5emu と実機で確認 |
| [25](25-n5-nat-dhcp.md) | N5: NAT（masquerade、TCP/UDP/ICMP echo、差分のチェックサム）と DHCP サーバ。試験用 LAN の相手が DHCP で借り、実機で Wi-Fi の NAT を通って外の Web に届いた |
| [26](26-ipv6.md) | IPv6 の V1〜V3: smoltcp の IPv6 と SLAAC、フィルタの IPv6、`svc:net` と `t5-rt` の IPv6、AAAA。実機で SLAAC のアドレス、LAN からの ping6 と TCP、IPv6 で外の Web |
| [27](27-ipv6-forwarding.md) | IPv6 の V4: IPv6 の転送と NDP proxy（下流への RA、上流と下流での代理 NA）。実機で下流の機器が上流の /64 のアドレスを作り、IPv6 で外の Web に届き、LAN から ping6 が届いた |
| [28](28-resources.md) | リソースの使用量: 内蔵 RAM（約 808 KiB、空き約 51%。カーネルの区画は 99%）、PSRAM、フラッシュ。`net` の初期値つきデータを .bss に移してイメージを 351→243 KB に |
| [29](29-dns-relay.md) | DNS の中継: DHCP サーバが Tab5 自身を DNS サーバとして渡し、下流の問い合わせを上流の DNS サーバへ中継。実機で試験用 LAN の相手が名前を解決 |
| [30](30-kernel-memory-autostart.md) | カーネルの .bss を L2MEM 上側へ、スタック 128 KiB、XIP のスロットを先頭アドレス基準に、プロセス 8 個、起動時の自動起動（wifi、fs、net、netctl）と SD の規則ファイル |
| [31](31-cpu-usage.md) | CPU 使用率: スケジューラでプロセスの時間と待ちの時間を数え、`ps` で窓の千分率を出す。実機で何もしないとき待ち 99.2%、Wi-Fi の TCP echo（片道 8 Mbps）で net 50%・カーネル 20% |
| [32](32-smp-proposal.md) | カーネルの SMP 対応の提案: Core1 の起動、共有の L1 データキャッシュとコアごとの命令キャッシュ、LR/SC、コア間の割り込み。方式は大きな鍵（BKL）、デバイスは Core0、t5emu に 2 つ目のコア、段階 S1〜S4 |
| [33](33-smp.md) | SMP の実装の記録。S1: Core1 の起動と待機（カーネル、t5emu の 2 つ目のコア、QEMU -smp 2）、実機で cores=2 |
| [34](34-net-cpu.md) | `net` の CPU を減らす: プロファイラ、時刻の読み方、SDIO の書き込みを待たない、Wi-Fi の送信の待ち行列、鍵を放して DMA を待つ、`memcpy`/`memset` のアセンブリ |
| [35](35-net-copies.md) | `net` の経路のコピーを減らす: ゼロコピーの検討、ABI を変えない Z0（直の読み書き、SDIO の bounce の上で分ける）、空の `READ` を減らす、Wi-Fi のフレームの輪（`MAP_RING`/`KICK`、ABI 小版 13） |
| [36](36-mmode-escape.md) | プロセスが M モードで動いていた: デバッガ（probe-rs）がハートを止めて再開したとき、カーネルからプロセスへの `mret` のところで。切り分け、`t5log` を止めない読み方に、カーネルの検出 |
| [37](37-wlan-tx-nowait.md) | Wi-Fi の送信でバスを待たない: Core0 の `KICK` は空いていれば 1 つ送って戻り、次は DMA の終わりの割り込みで。`KICK` のサイクル 313 M → 123 M、`net` の CPU 5.7 → 5.4 秒 |
| [38](38-init-spawn.md) | 起動するアプリを SD のファイルで決める: カーネルのオブジェクト `proc:spawn`（ABI 小版 14、待ち行列でサービスのループが起動）と、`T5OS/START.CFG` を読むアプリ `init` |
| [39](39-term-shell.md) | ターミナルとシェル: カーネルのキーボード（`kbd`、ABI 小版 15）、VT/xterm のアプリ `term`（`svc:tty`）、別プロセスの `shell`。実機で動作を確認 |
| [40](40-proc-info.md) | シェルの `ps`・`apps`・`kill`: カーネルのオブジェクト `proc:list`・`proc:apps`・`proc:kill`（ABI 小版 16、サービスのループが写しを取り、kill を行う） |
| [41](41-shell-net-top.md) | シェルの `ifconfig`・`top`・`nc`: ネットワークサービスの要求 `IF_INFO`（インターフェースのアドレスと数）、1 秒ごとの CPU 使用率、行ごとの TCP クライアント |
| [42](42-term-drawing.md) | ターミナルの描画を速くする: 隣り合う画素の向きに描く、見えているものと同じマスは描かない、スクロールはフレームバッファを動かす。全画面 223 → 60 ms、スクロール 50 → 33 ms/行 |
| [43](43-psram-bandwidth.md) | PSRAM の転送が遅い理由: ミスの待ち時間（1 ラインずつ）と表示の DMA。カーネルのベンチ `membench`、L2 のラインを 128 バイトに、キャッシュの自動先読み（フレームバッファより下）、`term` はフレームバッファの順に描く。読み 76 → 190 MB/s、書き 60 → 92 MB/s、全画面 60 → 35 ms |
| [44](44-ppa-2d.md) | PPA と 2D-DMA: 塗りつぶし・A8 の文字・矩形のコピーのドライバ（update ビットは別の書き込み）、`ppabench`、ディスプレイのハンドルへの `write` で FILL/COPY を出し `DONE`/`FAILED` を待つ（ABI 小版 17）、`term` の上スクロールをハードウェアで 28.6 → 18.0 ms/行、t5emu の模型 |
| [45](45-japanese-font.md) | ターミナルの日本語: フォントサービス `font`（XIP、共有メモリで字形を渡す。Noto Sans Mono CJK JP の JIS X 0208 全角 6649 字と Terminus の半角 836 字、OFL）、`t5_rt::Font`、`term` は全角を 2 マスに半分ずつ描く。署名の確かめでウォッチドッグを叩く |
| [46](46-ipv6-rest.md) | IPv6 の残り: 転送とフィルタの ICMPv6 のエラー（no route、reject、Time Exceeded、Packet Too Big、address unreachable）、RA の RDNSS を受けて下流へ広告し、名前解決と中継に使う、上流のプレフィックスが変わったら古いものを非推奨で広告。実機の網は O フラグ（DHCPv6）で RDNSS なし。一時アドレス（RFC 8981、smoltcp の送り元の選び方に規則 3 と 7）、カーネルの `random`（ABI 小版 18） |
| [47](47-entropy.md) | 乱数のエントロピー源: ESP-IDF の bootloader_random_enable と同じく、SAR ADC1 に内部の channel 10 を変換させ続け、RNG のレジスタを真の乱数にする。カーネルの起動時（clock_init の最初）に動かし、`rng.entropy` を出す。実機で ADC の値がばらつくことを確認。出力の検定（`rngdump`、`t5log read`、`tools/rngtest.py`。FIPS 140-2 と SP 800-22 の 8 つ）: `random` は全部合格、レジスタそのものは偏っている（11 ビットが 0.75 の確率で 1）。1 語の間隔を ESP-IDF と同じ 56 µs に。SP 800-90B のヘルステスト（繰り返しの数と適応的な割合、起動時に 1024 語）、`entropy on|off` で止めた状態と比べると統計は同じ（RC_FAST による第 2 のエントロピー源） |
| [48](48-ble.md) | Bluetooth LE の M0/M1: C6 の esp_hosted のまま、RPC の FeatureControl で BT を起動し（`wifi 1`）、SDIO の if_type 4 で HCI を通す。カーネルのオブジェクト `bt:hci`（ABI 小版 19）と試験アプリ `hcitest`。実機で HCI_Reset、版（Core 6.0、Espressif）、BD_ADDR、10 秒のスキャンで広告 244。BT を起動しただけでは Wi-Fi の ping は変わらず、窓 = 間隔のスキャン中は平均 31 ms。方針: bt-hci と TrouBLE、async は `ble` の中だけ。M2: `ble` サービス（TrouBLE と自前の小さな executor）と `svc:ble` のスキャン、`blescan`。実機で 10 秒に報告 245、捨てた数 0、待機中の CPU はほぼ 0。M3: セントラル（接続、サービスと特性の探索、読み、書き、購読）を `svc:ble` に、`blegatt`。実機で Nature Remo につなぎ Device Name を読んだ。プロセスの上限を 12 に。M4: ペリフェラル（実行時に組み立てる GATT サーバ、広告、通知と indication）、`bleecho`。PC の BlueZ から購読と書き込みをして、通知 9 つを受け取った。PC からの接続は 2〜3 割失敗する（C6 が接続の要求を受け損ねる。広告の間隔に依らず、モデムスリープでは悪化。未解決） |
| [49](49-matter.md) | Matter（simple-matter）: `matter` サービス 1 つにデバイスとコントローラ、on-network を先に、BLE は後（調査は research/matter-on-tab5-os.md）。MT0: git の依存（rev 固定）、`mtport`（`random` の Rng、SD の A/B の 2 ファイルの KVS）、`mtbench`。実機で PASE の検証子 219 ms、ECDSA の署名 49 ms、KVS の書き 23 ms。MT1: `matter` サービス（simple-matter-cffi を Rust から）、`net` の `UDP_JOIN`、共有メモリの窓を 2 つに。PC の smctl から on-network でコミッショニング（3.5 秒）、On/Off、再起動後も fabric が残る。cffi をやめてコアを直接使う形に（`device.rs`。コード 252 KB）、fabric の削除とコミッショニングのし直しも確かめた。MT2: コントローラ（`ControllerStack`、`Commissioner`）、`svc:matter` とシェルの `mt`、`mtreq`。実機で PC の試験用のデバイスをコミッショニング（3.8 秒）、On/Off と読み、再起動後も resumption で操作。MT3: コントローラから窓を開ける（`ocw`、`revoke`）、Tab5 のデバイスに 2 つの fabric、デバイスの再送バッファを 16 に。MT4（デバイスの側）: commissionable の間 `svc:ble` で広告し BTP でコミッショニング（`svc:ble` に `MTU` イベント）。PC の smctl から BLE でコミッショニングし UDP へ。simple-matter を 1544fae に上げ、デバイスは `MultiAdminStack` に戻した（smctl から約 13 秒）。MT4（コントローラの側）: `pairble`、Tab5 が PC の BLE のデバイスを BLE で AddNOC まで、CASE は UDP で（約 14 秒）、`forget`。引数つきの `invoke`、`write`、`sub`（レポートはログへ）。t5emu の試験（`matter 1`: メモリのストア、自分のデバイスを 127.0.0.1 でコミッショニング、購読、toggle）、`pairaddr`。`svc:net` の `UDP_WAIT`（データグラムが来たら答える。`matter` は 20 ms ごとに見に行くのをやめて `wait`）。BLE のコミッショニングで Wi-Fi を渡す（`netcred`、`svc:wifi` の `JOIN`、`svc:net` の `LINK_RENEW`、smctl の ble-wifi で Tab5 が `matter-test` へ移って 13.9 秒）、`install-apps` が入らなかったアプリを表示。§22: chip-tool から Tab5 をコミッショニングして操作、Tab5 から connectedhomeip の参照のライトをコミッショニングして操作・購読。mDNS の直し（ファイアウォールが IGMP を落としていた、つなぎ直しのたびに参加を報告、MLDv2 の記録の種類、告知と問い合わせを IPv6 にも、網を移ったら前の IPv6 アドレスを捨てる）。§23: REVERT を実機で、fail-safe の途中の起動し直しで確定前の fabric を消す、購読の値をテキストに・イベントの購読 `subev`、起動のときから ble と matter、起動時の Wi-Fi の再試行を速く、BLE の接続の失敗は Wi-Fi の共存ではない |
| [50](50-config.md) | 設定のサービス `cfg`: 名前つきの値を SD の `T5OS/CONFIG.A`/`.B`（A/B、世代と CRC）に持つ。`svc:cfg` は秘密の値（Wi-Fi の passphrase）を書けるが読めず、読めるのは `svc:cfgsec`（`wifi` だけに許可）。`t5_rt::Config`、シェルの `cfg`、試験の `cfgctl`、t5emu の試験。`wifi` は起動時に `wifi.ssid`/`wifi.psk` を読み、`svc:wifi` に `SAVE`/`REVERT`。`matter` は CommissioningComplete で `SAVE`、fail-safe が先に終われば `REVERT`。カーネルの listener の上限を 16 に。実機で smctl の ble-wifi で渡した `matter-test` が `wifi` の起動し直しと再起動の後も残った |
| [51](51-config-store-deps.md) | 設定の置き場所と暗号化、C6 の記憶、依存関係、権限。C6（esp_hosted 2.12.7）は `nvs_enable` = 1 だと網を自分の NVS に書く → `wifi` は 1 回だけ `WifiRestore` で消し、以後 `nvs_enable` = 0。`cfg` は `userdata` の先頭 64 KiB（カーネルの `flash:cfg`）に A/B の表と鍵、秘密の値は AES-256-GCM、SD の表を引き継いで消す。前のファームウェアの秘密が残っていた ESP-IDF の `nvs` 区画を `nvserase` で消した。依存は T5A の `Needs`（`svc:`/`ready:`）でカーネルが起動を待たせ、`wifi` が `ready:bt` を立てる。`ble` が回り続けた原因は trouble-host が使う embassy-sync の `OnceLock::get` の自己 wake（コントローラ待ち）→ Reset の確かめと実行器の待ち。今ある権限のまとめ。`NET.RUL` を 6 規則に。実機で passphrase がフラッシュに平文で 0 回、`wifi` の起動し直しと再起動で確かめた |
| [52](52-access-control.md) | アクセス制御。署名した許可に `svc:NAME/GRANT`、カーネルの `ident`（起動のときに許可を写す）と接続相手の `PEER_PID`/`PEER_NAME`/`PEER_GRANTS`、サーバが確かめる（fs のパス、net のポート、wifi、ble の役割、cfg のキーと秘密で `cfgsec` をやめた、matter の read/control/admin）。fs のアプリごとのフォルダ `T5OS/APPS/<名前>`（`~`）。`proc:kill/own`、`proc:spawn/NAME`。鍵を 2 つに（アプリの鍵の像は強い許可を持てない、カーネルの更新はシステムの鍵だけ）。自己試験に隣のスロットの読み書きとアプリの鍵の強い許可。ほかのアプリの RAM は読めない |
| [53](53-matter-endpoints.md) | アプリが持ち込む Matter のエンドポイントと画面（MT5）。`svc:matter/endpoint` の `endpoint KIND [LABEL]` で、エンドポイント 2〜5 の枠に light・plug・contact・temp・humidity（simple-matter のクラスタ）。持ち主（アプリの名前/ラベル）を KVS に残して番号を保つ、PartsList は枠の組ごとの静的な表。アプリは `set VALUE`、On/Off は `STATE` で知らされる。`eps`、`epdel`。試験の `mtep`、画面の `mtpanel`（タイルに触ると切り替え）。t5emu、実機で smctl と chip-tool から読み・toggle・購読 |
| [54](54-serial-capture-touch.md) | デバッグ: シリアル（USB Serial/JTAG）から制御コマンドを受け、画面のキャプチャ `capture`（RGB565 をラン長で縮めた 0xFE のフレーム、CRC-32）とタッチの入力 `tap`/`swipe`（利用者の座標、コントローラの読みの代わり）。PC は `t5log serial` で PNG に。t5emu の試験 |
| [55](55-gui-test.md) | 画面の自動の試験: `tools/guitest-mtpanel.py` が `t5log serial` で `tap`/`swipe`/`capture` を送り、PNG の画素とフォントのグリフ照合で `mtpanel` の画面を検査（端、swipe、smctl の toggle を含む 12 項目）。描いている途中のキャプチャで CRC が合わない不具合を直した（画素を 1 度だけ読む）。実機で 12/12 |
| [56](56-limits-restart-drop.md) | QNX などと比べて足りなかった 3 つ: アプリの鍵の像にチャネルの上限（1 プロセス 8、1 サービスへ 2、アプリ全体で半分、`Error::Limit`）、`init` のリストの常駐をカーネルが見張って起動し直す（フォールトか 0 以外の終わり、0.5〜4 秒、4 回まで、`kill` と 0 は起こさない）、許可を手放す `permit_drop`（開いたものはそのまま）。自己試験、t5emu、実機で `fstest` の 3 本目、`mtpanel` が手放して画面の試験 12/12 |
| [57](57-cpu-partitions.md) | CPU の取り分（QNX の adaptive partitioning に倣う）: system 30%、net 40%、apps 30% を 100 ms の窓で。取り分の内のパーティションを先に選び、空きは誰でも使う。入り方は署名した `cpu:NAME`（システムの鍵だけ）、既定は鍵で。`partitions on/off`、`partition PID NAME`、`ps` の `kernel.partition`。自己試験で `net` の spin が 400‰ → 782‰（実機）。Wi-Fi の echo は無線が律速で差が出なかった |
| [58](58-router-evb-plan.md) | 簡易ルーターと ESP32-P4 EVB の方針: 今の `net` はルーターの部品を持ち、足りないのは物理のリンク。EVB の調べ（IP101GRI の RMII、C6 の SDIO のピン、チップの版）、チップとボードに分ける、Ethernet のドライバはまずカーネルに `netdev:eth0`、WAN/LAN の役割と既定のフィルタ、段取り R0〜R6、場面 B（Wi-Fi の STA → 有線の LAN）を先に。利用者の判断（EVB v1.5.2 と v1.6.0、EVB の C6 は更新してよい）。R1 のソースの分割（`board-p4evb`）。R0（EVB v1.6.0 はチップ v3.2、v1.5.2 は v1.3）、R1 の実機（v1.5.2 で起動、PSRAM の VO2 の eFuse の値、2 つ目のコアの stall を解く、SD の VO4、Wi-Fi の STA と DHCP）、R2 の Ethernet（EMAC と IP101、`netdev:eth0`、`net` の `eth0`、100 Mbit/s 全二重、DHCP、ping 0.2〜0.4 ms）、R1b（チップ v3）の調べ。R3a（出口のリンクを有線にも、DNS の中継をどの LAN にも、`address on LINK`、`netctl` 8・9、実機で試験用 LAN の相手が `eth0` 経由で外へ） |
| [59](59-emu-speed.md) | t5emu の高速化（毎命令の付けたし: SipHash の `HashMap`、CLIC の線の作り直し、128 ビットの割り算、PMP の範囲の解き直しを除く。命令の数は同じまま 2.3 倍）と、`test-p4emu` の 26 回を別のターゲットにして `make -j` で並列（約 40 分 → 約 160 秒） |
| [60](60-foreground-seats.md) | フォアグラウンドアプリの切り替え: カーネルの席（`display` を開いたプロセスごとに席と専用のフレームバッファ、前の 1 枚だけを表示、`kbd`/`touch` は前の席にだけ）。案の比べ（合成、席、`term` の多重）と段取り F1〜F4 |
| [61](61-memory-in-top.md) | `top` のメモリ表示: PSRAM（アプリの領域、共有メモリ、表示）と内蔵 SRAM（カーネル、スタック、リング）、プロセスごとの RAM とスロットの割合。`proc:mem`、`display::FRAMES`（ABI 24） |
| [62](62-term-history-completion.md) | `term` の履歴（Alt+↑/↓、200 行）と `shell` の Tab 補完（コマンド、アプリ、プロセス、ファイル）、`more` |
| [21](21-n2a-hostlink.md) | N2a: ホストリンク（コンソールの線の上の IP、`netdev:host`、`t5log hostlink` と TUN）。実機で ping（0.4ms）と TCP 1MiB の echo（約 2.3Mbps）を確認 |
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
