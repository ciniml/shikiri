# tab5-os build entry points. See doc/07 §7 for the milestones.

BUILD      := build
APPS_DIR   := $(BUILD)/apps
APP_TARGET := user/target/riscv32imafc-unknown-none-elf/release
ELF2T5A    := tools/target/release/elf2t5a
T5LOG      := tools/target/release/t5log
APPS       := hello fault ticker spin
APP_ELFS   := $(addprefix $(APP_TARGET)/,$(APPS))
# All log schemas (doc/12): every event! call site in the kernel and the apps.
SCHEMAS    := $(BUILD)/t5log.schemas

QEMU       ?= qemu-system-riscv32
QEMU_FLAGS := -M virt -cpu rv32,s=off,h=off,mmu=off,d=off,pmp=on -bios none \
              -no-reboot

.PHONY: all apps tools kernel-virt kernel-p4 image-p4 boot run-virt test-virt \
        schemas-virt schemas-p4 emu test-emu test-p4emu backup-flash flash-p4 monitor \
        probe-rs rtt clean

all: kernel-virt image-p4 boot

IDF_PATH_6 ?= $(HOME)/esp-idf/6.0
IDF        := . $(IDF_PATH_6)/export.sh >/dev/null && idf.py
PORT       ?= /dev/ttyACM0
PRETTY     := $(T5LOG) decode --pretty --schemas $(SCHEMAS)

# probe-rs pinned to the commit reviewed in doc/13, installed into build/.
PROBE_RS_REV := dd4909231da2e17bea25c9277196d9fdf3ade53d
PROBE_RS     := $(BUILD)/probe-rs/bin/probe-rs
# The debug probe, as `probe-rs list` shows it (VID:PID:serial). The Tab5's
# built-in USB-JTAG uses its MAC address as the serial. Required: another
# ESP device may be connected.
PROBE        ?=

tools:
	cd tools && cargo build --release

apps: tools
	cd user && cargo build --release
	mkdir -p $(APPS_DIR)
	for a in $(APPS); do $(ELF2T5A) $(APP_TARGET)/$$a $(APPS_DIR)/$$a.t5a || exit 1; done

kernel-virt: apps
	cd kernel && T5_APPS_DIR=$(CURDIR)/$(APPS_DIR) cargo build --release \
	    --features platform-virt --target-dir target/virt
	mkdir -p $(BUILD) && cp kernel/target/virt/riscv32imac-unknown-none-elf/release/t5-kernel $(BUILD)/t5-kernel-virt.elf

# Collect and check the schemas (id collisions, naming rules).
schemas-virt: kernel-virt
	$(T5LOG) schemas -o $(SCHEMAS) $(BUILD)/t5-kernel-virt.elf $(APP_ELFS)

schemas-p4: kernel-p4
	$(T5LOG) schemas -o $(SCHEMAS) $(BUILD)/t5-kernel-esp32p4v1.elf $(APP_ELFS)

kernel-p4: apps
	cd kernel && T5_APPS_DIR=$(CURDIR)/$(APPS_DIR) cargo build --release \
	    --features platform-esp32p4v1 --target-dir target/esp32p4v1
	mkdir -p $(BUILD) && cp kernel/target/esp32p4v1/riscv32imac-unknown-none-elf/release/t5-kernel $(BUILD)/t5-kernel-esp32p4v1.elf

# ESP app image for the 2nd stage bootloader (rev v1.x only).
image-p4: kernel-p4
	espflash save-image --chip esp32p4 --min-chip-rev 1.0 --flash-size 16mb \
	    $(BUILD)/t5-kernel-esp32p4v1.elf $(BUILD)/t5-kernel-esp32p4v1.bin

# ESP-IDF bootloader + partition table (boot/sdkconfig.defaults, boot/partitions.csv).
boot:
	cd boot && bash -c '$(IDF) -B ../$(BUILD)/boot bootloader partition-table'

# --- Targets below touch the device. Run them only deliberately. ---

# Read the whole 16 MB flash into build/flash-backup.bin.
backup-flash:
	mkdir -p $(BUILD)
	espflash read-flash --port $(PORT) 0 0x1000000 $(BUILD)/flash-backup.bin
	sha256sum $(BUILD)/flash-backup.bin

# Writes bootloader @0x2000, partition table @0x8000 and the kernel to ota_0.
# otadata is erased so a selection left by the previous firmware cannot make
# the bootloader pick ota_1.
flash-p4: image-p4 boot
	espflash erase-region --port $(PORT) 0xd000 0x2000
	espflash write-bin --port $(PORT) 0x2000 $(BUILD)/boot/bootloader/bootloader.bin
	espflash write-bin --port $(PORT) 0x8000 $(BUILD)/boot/partition_table/partition-table.bin
	espflash write-bin --port $(PORT) 0x10000 $(BUILD)/t5-kernel-esp32p4v1.bin

# Decoded log of the device (the console carries log frames, doc/12).
# Collect the log over RTT with probe-rs (doc/12 L2). Does not flash or reset;
# attaching halts the core briefly and disables the watchdogs (doc/13).
rtt: schemas-p4
	@test -n "$(PROBE)" || { echo "set PROBE=VID:PID:SERIAL (see: $(PROBE_RS) list)"; exit 1; }
	tools/probe-rs/rtt.sh $(PROBE_RS) $(PROBE) $(BUILD)/t5-kernel-esp32p4v1.elf \
	    $(SCHEMAS) $(T5LOG) $(BUILD)/rtt.hex

probe-rs:
	cargo install --locked --git https://github.com/probe-rs/probe-rs --rev $(PROBE_RS_REV) \
	    probe-rs-tools --root $(BUILD)/probe-rs

monitor: schemas-p4
	stty -F $(PORT) raw -echo
	$(PRETTY) $(PORT)

# Run on QEMU virt with the decoded log on stdout.
run-virt: schemas-virt
	$(QEMU) $(QEMU_FLAGS) -display none -monitor none -serial stdio \
	    -kernel $(BUILD)/t5-kernel-virt.elf | $(PRETTY)

# CI-style run: the kernel exits QEMU with status 0 only if the self-test
# passes; the raw log is kept in build/virt-console.bin and shown decoded.
test-virt: schemas-virt
	timeout 20 $(QEMU) $(QEMU_FLAGS) -display none -monitor none \
	    -serial file:$(BUILD)/virt-console.bin -kernel $(BUILD)/t5-kernel-virt.elf; \
	    s=$$?; $(PRETTY) $(BUILD)/virt-console.bin; exit $$s

# ESP32-P4 rev v1.x emulator (doc/11). Runs the same kernel ELF as the
# hardware; exit status 0 only if the self-test passes.
T5EMU := emu/target/release/t5emu

emu:
	cd emu && cargo build --release

test-emu:
	cd emu && cargo test --release

test-p4emu: emu kernel-p4
	$(T5EMU) $(addprefix --elf ,$(APP_ELFS)) $(BUILD)/t5-kernel-esp32p4v1.elf

clean:
	rm -rf $(BUILD) kernel/target user/target tools/target emu/target
