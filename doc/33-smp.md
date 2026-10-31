# 33. SMP の実装

doc/32 の提案（方式 A: 大きなカーネルの鍵、デバイスは Core0、t5emu に 2 つ目のコア）を、利用者が承認した（2026-11-01「すすめて」）。段階ごとに記録する。

## 1. 状態
| 段 | 内容 | 状態 |
|---|---|---|
| S1 | Core1 を起動し、カーネルの中で待たせる。t5emu と QEMU に 2 つ目のコア | QEMU（`-smp 2`）、t5emu、実機で確認（§2〜§4） |
| S2 | 大きな鍵とスケジューラ。両コアでアプリを動かす | 未 |
| S3 | コア間の割り込み、フラッシュの操作中の停止、両コアの命令キャッシュ | 未 |
| S4 | 実機でネットワークを含めて試す | 未 |

## 2. S1: カーネル（`kernel/src/smp.rs`、`arch/riscv32/mod.rs`、リンカスクリプト）
- **入口**: `_start` は `mhartid` を見る。0 でない hart（QEMU はすべての hart をここから始める）は、`SECONDARY_GO` が 0 でなくなるまで待ち、`_start_secondary` へ進む。P4 の Core1 は、ROM から直接 `_start_secondary` に来る。
- **`_start_secondary`**: MIE=0、mscratch=0、gp=0 にし、Core1 用のスタック（`.stack1`。P4 は RAM_HI に 64 KiB、QEMU は 32 KiB）に切り替えて、`smp::kmain_secondary` を呼ぶ。
- **`kmain_secondary`**:
  - プラットフォームの初期化（P4 は Core1 の CLIC のしきい値）、トラップのベクタ、PMP の初期化をする。
  - `smp.core_up`（hart、mtvec、sp）を出し、`CORE_UP` を立てる。
  - その後は `park` で待つ。`park` は RAM（`.ram_text`）の WFI のループで、Core1 に割り込みは何も回していないので眠り続ける。RAM に置くのは、Core0 のフラッシュの操作でキャッシュが止まっても読めるようにするため（doc/14 §5）。
- **Core0**（`kmain` の PMP の初期化の直後、`smp::start`）: プラットフォームの `start_secondary` で Core1 を起動し、`CORE_UP` を最大 100 ms 待って `smp.started`（cores）を出す。S1 では、Core0 が待っている間に Core1 がログを書くので、両コアが同時にカーネルの状態に触ることはない。
- **P4 の起動**（`esp32p4v1.rs`）: SOC_CLK_CTRL0 の CORE1_CPU_CLK_EN を立て、HP_RST_EN0 の RST_EN_CORE1_GLOBAL を下ろし、ROM の `ets_set_appcpu_boot_addr(_start_secondary)` を呼ぶ（doc/32 §1）。
- **QEMU**: `start_secondary` は `SECONDARY_GO` を立てるだけ。`-smp 1` では待ちが 100 ms で切れ、`cores=1` で続く。`make test-virt` を `-smp 2` にした。

## 3. S1: t5emu（`emu/src/machine.rs`、`periph.rs`）
- `Machine` は、動いていないほうのコア（`Core`: `Cpu`、CLIC、待ちの印）を持つ。1 度に 1 コアを `QUANTUM`（1024 命令）ずつ進め、状態と CLIC を入れ替える。時刻（`bus.cycles`）は両コアで共有する。両方が忙しいと、エミュレータの時刻あたりの仕事は半分になる（判断。正しさの試験が目的）。
- **WFI**: 2 コアのときは、起こすものがなければ「待ち」の印を付けて、もう一方に番を渡す。待っているコアは、起こすものが来るまで飛ばす。両方とも待っていれば、どちらかが起きられる最初の時刻まで時計を飛ばす。
- **割り込み**: Core1 の割り込みマトリクス（0x500D_6800）を足した。振り分けは、動いているコアのマトリクスで、そのコアの CLIC に入れる。
- **`mhartid`**: コアの番号を返す。
- **LR/SC**: 書き込み（AMO を含む）は、もう一方のコアの同じワードの予約を消す。
- **Core1 の起動**: ROM の `ets_set_appcpu_boot_addr` は、Core1 のクロックが入り、リセットが解けていれば、a0 から Core1 を始める。
- まだ: コア間の割り込み（HP_SYSTEM の `CPU_INT_FROM_CPU_n`）、コアごとの命令キャッシュ（今は 1 つの模型を共有）。

## 4. S1 の結果（2026-11-01）
| 環境 | 結果 |
|---|---|
| QEMU `-smp 2` | `smp.core_up hart=1 sp=0x800f8880`、`smp.started cores=2`、自己試験が通る |
| QEMU `-smp 1` | 100 ms 待って `smp.started cores=1`、自己試験が通る |
| t5emu | `smp.core_up hart=1 mtvec=0x40003ac3 sp=0x4ff6c430`、`smp.started cores=2`、自己試験が通る |
| 実機（kupdate） | `smp.started cores=2`（起動から 0.16 秒）。新しいカーネルが確定し、自動起動の Wi-Fi なども今までどおり動く |
