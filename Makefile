# tab5-os build entry points. See doc/07 §7 for the milestones.

BUILD      := build
APPS_DIR   := $(BUILD)/apps
APP_TARGET := user/target/riscv32imafc-unknown-none-elf/release
ELF2T5A    := tools/target/release/elf2t5a
T5LOG      := tools/target/release/t5log
T5STORE    := tools/target/release/t5store
# App store images (doc/14 §2): appmeta.bin @0x410000, apps.bin @0x420000.
STORE_DIR  := $(BUILD)/store
APPS       := hello fault ticker spin timers echo paint fs fstest net nettest wifi netctl init keylog term shell hcitest ble blescan blegatt bleecho mtbench matter mtreq mtep mtpanel mtdash cfg cfgctl c6ota ota kgfx imu beep mic codecdiag spkcheck ts
# Permissions TLV per app (doc/14 §4): the objects it may open, and with
# `svc:NAME/GRANT` what it may do in a service (doc/52 §3.3: fs paths, net
# ports, cfg keys, wifi, ble roles, matter requests). The least each needs.
PERMIT_timers := --permit timer
PERMIT_fault  := $(foreach q,1 2 3 4 5 6 7 8 9,--permit chan:q$(q))
PERMIT_echo   := --permit chan:echo --permit listen:echo --permit svc:echo --permit shm --permit listen:shmt --permit svc:shmt
PERMIT_paint  := --permit display --permit touch
PERMIT_fs     := --permit sd --permit shm --permit listen:fs --stack 32768
PERMIT_fstest := --permit svc:fs --stack 16384
PERMIT_net    := --permit cpu:net --permit shm --permit netdev:eth0 --permit netdev:ap0 --permit listen:net --permit listen:netctl --permit netdev:host --permit netdev:wlan0 --permit random --stack 65536
PERMIT_nettest := --permit svc:net/udp:any --permit svc:net/tcp-listen:any --permit svc:net/tcp-connect \
                  --permit svc:net/ping --permit svc:net/resolve --permit svc:net/info --stack 32768
PERMIT_wifi   := --permit cpu:net --permit wlan:ctl --permit listen:wifi --permit svc:net/link --permit svc:cfg/r:wifi. \
                 --permit svc:cfg/w:wifi. --permit svc:cfg/secret:wifi. --permit ready:bt --stack 24576
PERMIT_netctl := --permit svc:netctl --permit svc:fs/rw:T5OS/NET.RUL --stack 32768
PERMIT_init   := --permit proc:spawn --permit svc:fs/rw:T5OS/START.CFG --stack 16384
PERMIT_keylog := --permit kbd
PERMIT_hcitest := --permit bt:hci
PERMIT_ble    := --permit bt:hci --permit listen:ble --stack 65536
PERMIT_blescan := --permit svc:ble/scan
PERMIT_blegatt := --permit svc:ble/central
PERMIT_bleecho := --permit svc:ble/peripheral
PERMIT_mtbench := --permit random --permit svc:fs/rw:T5OS/MATTER --stack 32768
PERMIT_mtreq  := --permit svc:matter/read --permit svc:matter/control --permit svc:matter/admin
PERMIT_mtep   := --permit svc:matter/endpoint
PERMIT_mtpanel := --permit display --permit touch --permit svc:matter/endpoint --permit svc:matter/read
PERMIT_mtdash  := --permit display --permit touch --permit svc:matter/read --permit svc:matter/control
PERMIT_cfg    := --permit listen:cfg --permit flash:cfg --permit random --permit svc:fs/rw:T5OS/CONFIG.A \
                 --permit svc:fs/rw:T5OS/CONFIG.B --stack 16384
PERMIT_cfgctl := --permit svc:cfg/r: --permit svc:cfg/w:test. --permit svc:cfg/w:wifi.
PERMIT_c6ota  := --permit svc:net/tcp-connect --permit svc:wifi/ota
PERMIT_kgfx   := --permit display
PERMIT_imu    := --permit i2c:int/0x68 --permit i2c:int/0x69
PERMIT_codecdiag := --permit i2c:int/0x10 --permit i2c:int/0x43 --permit audio:volume
PERMIT_beep   := --permit audio:out
PERMIT_mic    := --permit audio:in
PERMIT_spkcheck := --permit audio:out --permit audio:in
# Tailscale (doc/71): the control server over TLS; its keys and the auth
# key are secret settings (strong: the system key).
PERMIT_ts := --permit svc:net/tcp-connect --permit svc:net/resolve --permit random \
             --permit svc:cfg/r:ts. --permit svc:cfg/w:ts. --permit svc:cfg/secret:ts. --stack 262144
PERMIT_ota    := --permit listen:ota --permit update --permit svc:fs/r: --permit svc:net/tcp-connect --permit svc:net/resolve --stack 24576
PERMIT_matter := --permit svc:net/udp:5540 --permit svc:net/udp:5541 --permit svc:net/udp:5353 --permit svc:net/mcast \
                 --permit svc:net/info --permit svc:fs/rw:T5OS/MATTER --permit random --permit listen:matter \
                 --permit svc:ble/scan --permit svc:ble/central --permit svc:ble/peripheral \
                 --permit svc:wifi/status --permit svc:wifi/join --permit svc:cfg/r:wifi. --permit svc:cfg/secret:wifi. \
                 --stack 131072
PERMIT_font   := --permit listen:font --permit shm
PERMIT_term   := --permit display --permit kbd --permit listen:tty --permit proc:spawn/shell --permit svc:font --stack 16384
PERMIT_shell  := --permit svc:tty --permit svc:fs/rw: --permit svc:net/tcp-connect --permit svc:net/ping \
                 --permit svc:net/resolve --permit svc:net/info --permit proc:spawn --permit proc:list --permit proc:apps --permit proc:mem \
                 --permit proc:kill/own --permit display:focus --permit svc:ota --permit svc:matter/read --permit svc:matter/control --permit svc:matter/admin \
                 --permit svc:cfg/r: --permit svc:cfg/w: --permit audio:volume --stack 32768
# What an app needs before the kernel starts it (doc/51 §3): `svc:NAME` (a
# service listens) or `ready:NAME` (a process raised the mark). The kernel
# holds the start up to 10 s.
NEEDS_ble     := --needs ready:bt
NEEDS_matter  := --needs svc:net
NEEDS_cfgctl  := --needs svc:cfg
# Apps linked for XIP (doc/15): text runs from flash, data in PSRAM. Built
# with t5app-xip.ld into their own target directory; hello is built both ways.
XIP_APPS   := hello xipcheck spin font
XIP_TARGET := user/target/xip/riscv32imafc-unknown-none-elf/release
XIP_RUSTFLAGS := -C relocation-model=pie -C link-arg=-pie -C link-arg=--no-dynamic-linker \
                 -C link-arg=-z -C link-arg=text -C link-arg=-Tt5app-xip.ld
# Apps with a heap (doc/71): built on their own so that the features they
# turn on (`alloc` in zeroize and the like) do not reach the other apps,
# which have no allocator.
HEAP_APPS   := ts
HEAP_TARGET := user/target/heap/riscv32imafc-unknown-none-elf/release
ELF_OF      = $(if $(filter $(1),$(HEAP_APPS)),$(HEAP_TARGET),$(APP_TARGET))/$(1)
APP_ELFS   := $(foreach a,$(APPS),$(call ELF_OF,$(a))) $(XIP_TARGET)/xipcheck $(XIP_TARGET)/font
# All log schemas (doc/12): every event! call site in the kernel and the apps.
SCHEMAS    := $(BUILD)/t5log.schemas

QEMU       ?= qemu-system-riscv32
# Two harts, as the Tab5's two cores (doc/33).
QEMU_FLAGS := -M virt -smp 2 -cpu rv32,s=off,h=off,mmu=off,d=off,pmp=on -bios none \
              -no-reboot
# The app store in place of flash: RAM at 0x8400_0000 + flash offset
# (platform/virt.rs FLASH_IMAGE).
QEMU_STORE := -device loader,file=$(BUILD)/store/appmeta.bin,addr=0x84410000,force-raw=on \
              -device loader,file=$(BUILD)/store/apps.bin,addr=0x84420000,force-raw=on

.PHONY: all keys apps tools kernel-virt kernel-p4 image-p4 kernel-p4evb image-p4evb kernel-p4evb3 image-p4evb3 boot boot-v3 run-virt test-virt \
        schemas-virt schemas-p4 emu test-emu test-p4emu backup-flash flash-p4 flash-store monitor \
        probe-rs rtt clean kupdate-test-kernels flash-kernel-p4 kupdate install-apps

all: kernel-virt image-p4 boot

IDF_PATH_6 ?= $(HOME)/esp-idf/6.0
IDF        := . $(IDF_PATH_6)/export.sh >/dev/null && idf.py
# Chip v3.2 (ROM eco7) needs ESP-IDF 6.0.1 or later: the bootloader from the
# 6.0-beta tree above made USB-Serial/JTAG vanish before it loaded the app
# (doc/58 R1b).
IDF_PATH_V3 ?= $(HOME)/esp-idf/6.0.2
IDF_V3     := . $(IDF_PATH_V3)/export.sh >/dev/null && idf.py
PORT       ?= /dev/ttyACM0
PRETTY     := $(T5LOG) decode --pretty --schemas $(SCHEMAS)

# The probe-rs CLI pinned to the commit reviewed in doc/13, installed into
# build/ (optional: `make rtt` does not need it; t5log links the same commit).
PROBE_RS_REV := dd4909231da2e17bea25c9277196d9fdf3ade53d
PROBE_RS     := $(BUILD)/probe-rs/bin/probe-rs
# The debug probe, as `probe-rs list` shows it (VID:PID:serial). The Tab5's
# built-in USB-JTAG uses its MAC address as the serial. Required: another
# ESP device may be connected.
PROBE        ?=

tools:
	cd tools && cargo build --release

# App signing (doc/14 §1.3). The secret key stays outside the repository; the
# kernel embeds the public key in keys/. `make keys` creates a pair once.
DEV_KEY       ?= $(HOME)/.config/tab5-os/dev-ed25519.key
DEV_PUB       := keys/dev-ed25519.pub
# The app key (doc/52 §4): images signed with it may not hold strong
# permissions (`t5a::strong`: serving, the devices services own, any file
# outside the app's folder, secrets, starting or stopping any app). The
# kernel tells the keys apart by the file name (keys/app-*.pub).
APP_KEY       ?= $(HOME)/.config/tab5-os/app-ed25519.key
APP_PUB       := keys/app-ed25519.pub
# Signed with the system key (DEV_KEY): the services and the apps that need
# strong permissions. The rest get APP_KEY.
SYSTEM_APPS   := fs net wifi netctl init term shell ble matter cfg font echo keylog hcitest mtbench c6ota ota mic codecdiag spkcheck ts
KEY_OF         = $(if $(filter $(1),$(SYSTEM_APPS)),$(DEV_KEY),$(APP_KEY))
# A key the kernel does not trust, for the rejection test. Not a secret.
UNTRUSTED_KEY := $(BUILD)/keys/untrusted.key

keys: tools
	@if [ -f $(DEV_KEY) ]; then echo "$(DEV_KEY) exists; not overwriting"; \
	else $(ELF2T5A) keygen --secret $(DEV_KEY) --public $(DEV_PUB); fi
	@if [ -f $(APP_KEY) ]; then echo "$(APP_KEY) exists; not overwriting"; \
	else $(ELF2T5A) keygen --secret $(APP_KEY) --public $(APP_PUB); fi

$(UNTRUSTED_KEY): | tools
	$(ELF2T5A) keygen --secret $@ --public $(BUILD)/keys/untrusted.pub \
	    --seed 4242424242424242424242424242424242424242424242424242424242424242

apps: tools $(UNTRUSTED_KEY)
	@test -f $(DEV_KEY) -a -f $(APP_KEY) || { echo "no signing key $(DEV_KEY) or $(APP_KEY): run make keys"; exit 1; }
	cd user && cargo build --release --workspace $(addprefix --exclude ,$(HEAP_APPS) tsclient)
	cd user && RUSTFLAGS="$(XIP_RUSTFLAGS)" cargo build --release --target-dir target/xip $(addprefix -p ,$(XIP_APPS))
	cd user && cargo build --release --target-dir target/heap $(addprefix -p ,$(HEAP_APPS))
	mkdir -p $(APPS_DIR)
	$(foreach a,$(APPS),$(ELF2T5A) build $(call ELF_OF,$(a)) $(APPS_DIR)/$(a).t5a --key $(call KEY_OF,$(a)) $(PERMIT_$(a)) $(NEEDS_$(a)) &&) true
	@# Images the kernel must reject (self-test, doc/14 §1.4).
	$(ELF2T5A) build $(APP_TARGET)/hello $(APPS_DIR)/hello-untrusted.t5a --key $(UNTRUSTED_KEY) \
	    --name hello-untrusted
	$(ELF2T5A) build $(APP_TARGET)/hello $(APPS_DIR)/hello-badabi.t5a --key $(DEV_KEY) --abi 9.0 \
	    --name hello-badabi
	$(ELF2T5A) build $(APP_TARGET)/hello $(APPS_DIR)/hello-strong.t5a --key $(APP_KEY) --permit sd \
	    --name hello-strong
	$(ELF2T5A) build $(XIP_TARGET)/hello $(APPS_DIR)/hello-xip.t5a --key $(APP_KEY) --name hello-xip
	$(ELF2T5A) build $(XIP_TARGET)/xipcheck $(APPS_DIR)/xipcheck.t5a --key $(APP_KEY)
	$(ELF2T5A) build $(XIP_TARGET)/font $(APPS_DIR)/font.t5a --key $(DEV_KEY) $(PERMIT_font)
	$(ELF2T5A) build $(XIP_TARGET)/spin $(APPS_DIR)/spin-xip.t5a --key $(APP_KEY) --name spin-xip
	@# Not in the store image: installed over the control channel (doc/14 §3).
	$(ELF2T5A) build $(APP_TARGET)/hello $(APPS_DIR)/greeter.t5a --key $(APP_KEY) --name greeter
	@# App updates (M5b, doc/16 §4.3): a newer greeter, one that faults
	@# (the fault app under greeter's name) and a newer XIP spin.
	$(ELF2T5A) build $(APP_TARGET)/hello $(APPS_DIR)/greeter-0.2.t5a --key $(APP_KEY) --name greeter --version 0.2.0
	$(ELF2T5A) build $(APP_TARGET)/fault $(APPS_DIR)/greeter-bad.t5a --key $(APP_KEY) --name greeter --version 0.3.0
	$(ELF2T5A) build $(XIP_TARGET)/spin $(APPS_DIR)/spin-xip-0.2.t5a --key $(APP_KEY) --name spin-xip --version 0.2.0
	@# Not the heap apps: ts (doc/71, 0.5 MB) would leave too few free
	@# extents for p4emu-run-35's spread store.
	$(T5STORE) build -o $(STORE_DIR) $(addprefix $(APPS_DIR)/,$(addsuffix .t5a,$(filter-out $(HEAP_APPS),$(APPS)) hello-untrusted hello-badabi hello-strong hello-xip xipcheck font))

# The services started at boot on the Tab5 (doc/30 §5): the same apps with
# the autostart flag, installed over RTT by `make install-auto`. The store
# image (and so the tests) keeps them without it.
# `init` starts the rest from T5OS/START.CFG on the SD card (doc/38).
AUTO_APPS := fs init ota
AUTO_DIR  := $(APPS_DIR)/auto
apps-auto: apps
	mkdir -p $(AUTO_DIR)
	$(foreach a,$(AUTO_APPS),$(ELF2T5A) build $(APP_TARGET)/$(a) $(AUTO_DIR)/$(a).t5a --key $(DEV_KEY) $(PERMIT_$(a)) $(NEEDS_$(a)) --autostart &&) true

install-auto: apps-auto schemas-p4
	@test -n "$(PROBE)" || { echo "set PROBE=VID:PID:SERIAL (see: probe-rs list)"; exit 1; }
	$(T5LOG) rtt --probe $(PROBE) --chip-description tools/probe-rs/esp32p4-tab5.yaml \
	    $(foreach a,$(AUTO_APPS),--cmd "install $(AUTO_DIR)/$(a).t5a") --until-done \
	    --elf $(BUILD)/t5-kernel-esp32p4v1.elf $(addprefix --elf ,$(APP_ELFS)) 2>&1 | tee $(BUILD)/install-auto.log
	@# A refused install (`store full`) left the app's earlier copy: say which.
	@python3 tools/install-failures.py $(BUILD)/install-auto.log | sed 's/^/install-auto: NOT INSTALLED: /'

# Every app the Tab5's store holds, from the current sources (the store
# keeps what was installed last; the self-test runs it): the store apps
# without the autostart services, and those with the flag. Not `fault`,
# which always faults, so its update would be rolled back. Run on the next
# boot (`reboot`).
STORE_APPS := $(filter-out fault $(AUTO_APPS),$(APPS)) hello-strong hello-xip xipcheck font
install-apps: apps-auto schemas-p4
	@test -n "$(PROBE)" || { echo "set PROBE=VID:PID:SERIAL (see: probe-rs list)"; exit 1; }
	$(T5LOG) rtt --probe $(PROBE) --chip-description tools/probe-rs/esp32p4-tab5.yaml \
	    $(foreach a,$(STORE_APPS),--cmd "install $(APPS_DIR)/$(a).t5a") \
	    $(foreach a,$(AUTO_APPS),--cmd "install $(AUTO_DIR)/$(a).t5a") --until-done \
	    --elf $(BUILD)/t5-kernel-esp32p4v1.elf $(addprefix --elf ,$(APP_ELFS)) 2>&1 | tee $(BUILD)/install-apps.log
	@# A refused install (`store full`) left the app's earlier copy: say which.
	@python3 tools/install-failures.py $(BUILD)/install-apps.log | sed 's/^/install-apps: NOT INSTALLED: /'

kernel-virt: apps
	cd kernel && cargo build --release \
	    --features platform-virt --target-dir target/virt
	mkdir -p $(BUILD) && cp kernel/target/virt/riscv32imac-unknown-none-elf/release/t5-kernel $(BUILD)/t5-kernel-virt.elf

# Collect and check the schemas (id collisions, naming rules).
schemas-virt: kernel-virt
	$(T5LOG) schemas -o $(SCHEMAS) $(BUILD)/t5-kernel-virt.elf $(APP_ELFS)

schemas-p4: kernel-p4
	$(T5LOG) schemas -o $(SCHEMAS) $(BUILD)/t5-kernel-esp32p4v1.elf $(APP_ELFS)

kernel-p4: apps
	cd kernel && cargo build --release \
	    --features platform-esp32p4v1 --target-dir target/esp32p4v1
	mkdir -p $(BUILD) && cp kernel/target/esp32p4v1/riscv32imac-unknown-none-elf/release/t5-kernel $(BUILD)/t5-kernel-esp32p4v1.elf
	tools/check-ram-text.sh $(BUILD)/t5-kernel-esp32p4v1.elf

# The same kernel for the ESP32-P4-Function-EV-Board v1.5.x (doc/58 §3.1):
# chip v1.x, no display, touch or keyboard, the C6 on its own pins.
kernel-p4evb: apps
	cd kernel && cargo build --release \
	    --features "platform-esp32p4v1 board-p4evb" --target-dir target/p4evb
	mkdir -p $(BUILD) && cp kernel/target/p4evb/riscv32imac-unknown-none-elf/release/t5-kernel $(BUILD)/t5-kernel-p4evb.elf
	tools/check-ram-text.sh $(BUILD)/t5-kernel-p4evb.elf

image-p4evb: kernel-p4evb
	espflash save-image --chip esp32p4 --min-chip-rev 1.0 --flash-size 16mb \
	    $(BUILD)/t5-kernel-p4evb.elf $(BUILD)/t5-kernel-p4evb.bin
	$(ELF2T5A) kernel $(BUILD)/t5-kernel-p4evb.bin $(BUILD)/t5-kernel-p4evb.t5k --key $(DEV_KEY)

# The EV-Board v1.6.x: chip v3.x (doc/58 R1b), its own memory map and
# bootloader (boot-v3).
kernel-p4evb3: apps
	cd kernel && cargo build --release \
	    --features "platform-esp32p4v1 board-p4evb chip-v3" --target-dir target/p4evb3
	mkdir -p $(BUILD) && cp kernel/target/p4evb3/riscv32imac-unknown-none-elf/release/t5-kernel $(BUILD)/t5-kernel-p4evb3.elf
	tools/check-ram-text.sh $(BUILD)/t5-kernel-p4evb3.elf

image-p4evb3: kernel-p4evb3
	espflash save-image --chip esp32p4 --min-chip-rev 3.0 --flash-size 16mb \
	    $(BUILD)/t5-kernel-p4evb3.elf $(BUILD)/t5-kernel-p4evb3.bin
	$(ELF2T5A) kernel $(BUILD)/t5-kernel-p4evb3.bin $(BUILD)/t5-kernel-p4evb3.t5k --key $(DEV_KEY) --chip-rev 300

# ESP app image for the 2nd stage bootloader (rev v1.x only), and the same
# image signed as a kernel update package (T5K, doc/16 §2).
KERNEL_T5K := $(BUILD)/t5-kernel-esp32p4v1.t5k
image-p4: kernel-p4
	espflash save-image --chip esp32p4 --min-chip-rev 1.0 --flash-size 16mb \
	    $(BUILD)/t5-kernel-esp32p4v1.elf $(BUILD)/t5-kernel-esp32p4v1.bin
	$(ELF2T5A) kernel $(BUILD)/t5-kernel-esp32p4v1.bin $(KERNEL_T5K) --key $(DEV_KEY)

# Kernels for the update tests (doc/16 §3.6), as T5K packages in
# build/kupdate/: good (0.1.1, secure_version 1), bad-selftest (its self-test
# always fails), bad-panic (panics before it can confirm itself), and
# tampered (the dev kernel with one byte changed after signing).
KUPDATE_DIR := $(BUILD)/kupdate
define test_kernel
	cd kernel && T5_KERNEL_VERSION=0.1.1-$(1) T5_KERNEL_SECURE_VERSION=1 cargo build --release \
	    --features "platform-esp32p4v1 $(2)" --target-dir target/kupdate-$(1)
	espflash save-image --chip esp32p4 --min-chip-rev 1.0 --flash-size 16mb \
	    kernel/target/kupdate-$(1)/riscv32imac-unknown-none-elf/release/t5-kernel $(KUPDATE_DIR)/$(1).bin
	$(ELF2T5A) kernel $(KUPDATE_DIR)/$(1).bin $(KUPDATE_DIR)/$(1).t5k --key $(DEV_KEY)
	cp kernel/target/kupdate-$(1)/riscv32imac-unknown-none-elf/release/t5-kernel $(KUPDATE_DIR)/$(1).elf
endef
kupdate-test-kernels: image-p4
	mkdir -p $(KUPDATE_DIR)
	$(call test_kernel,good,)
	$(call test_kernel,bad-selftest,test-bad-selftest)
	$(call test_kernel,bad-panic,test-bad-panic)
	$(ELF2T5A) otadata $(KUPDATE_DIR)/otadata-pending.bin 1 1
	$(ELF2T5A) otadata $(KUPDATE_DIR)/otadata-netconfirm.bin 1 1 10.55.0.1:8080
	@# The development kernel (secure_version 0): refused once a test kernel
	@# (secure_version 1) runs.
	cp $(KERNEL_T5K) $(KUPDATE_DIR)/downgrade.t5k
	cp $(KERNEL_T5K) $(KUPDATE_DIR)/tampered.t5k
	printf 'X' | dd of=$(KUPDATE_DIR)/tampered.t5k bs=1 seek=100 conv=notrunc status=none

# ESP-IDF bootloader + partition table (boot/sdkconfig.defaults, boot/partitions.csv).
boot:
	cd boot && bash -c '$(IDF) -B ../$(BUILD)/boot bootloader partition-table'

# The same for chip v3.x (boot/sdkconfig.v3.defaults), in build/boot-v3, with
# ESP-IDF 6.0.2.
boot-v3:
	cd boot && bash -c '$(IDF_V3) -B ../$(BUILD)/boot-v3 -D SDKCONFIG=../$(BUILD)/boot-v3/sdkconfig \
	    -D SDKCONFIG_DEFAULTS=sdkconfig.v3.defaults bootloader partition-table'

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

# Kernel update over RTT (doc/16 §3): the running kernel writes the T5K
# package (default: this build; T5K=FILE for another) into the other slot,
# selects it in otadata, and restarts. The new kernel confirms itself after
# its self-test; `make rtt PROBE=... CMD=kinfo` shows the slots afterwards.
kupdate: image-p4 schemas-p4
	@test -n "$(PROBE)" || { echo "set PROBE=VID:PID:SERIAL (see: probe-rs list)"; exit 1; }
	$(T5LOG) rtt --probe $(PROBE) --chip-description tools/probe-rs/esp32p4-tab5.yaml \
	    --cmd "kupdate $(or $(T5K),$(KERNEL_T5K))" --cmd reboot --until-done \
	    --elf $(BUILD)/t5-kernel-esp32p4v1.elf $(addprefix --elf ,$(APP_ELFS))

# Kernel to ota_0 with otadata erased, so the bootloader starts ota_0
# whichever slot a kernel update left selected (doc/16 §3.7).
flash-kernel-p4: image-p4
	espflash erase-region --port $(PORT) 0xd000 0x2000
	espflash write-bin --port $(PORT) 0x10000 $(BUILD)/t5-kernel-esp32p4v1.bin

# Decoded log of the device (the console carries log frames, doc/12).
# Read the log over RTT and decode it as it arrives: t5log uses probe-rs as a
# library (doc/12 §8). Does not flash. Attaching halts the core briefly and
# disables the watchdogs (doc/13); RESET=1 resets the chip first to show a
# whole boot (RTT data is consumed when read). Commands typed on stdin go to
# the kernel (list, ps, run NAME [A0 [A1]], kill PID, install FILE.t5a,
# remove NAME, flashtest; doc/14 §3); CMD="..."
# sends one and exits when it is answered. Ctrl-C to stop.
rtt: schemas-p4
	@test -n "$(PROBE)" || { echo "set PROBE=VID:PID:SERIAL (see: probe-rs list)"; exit 1; }
	$(T5LOG) rtt --probe $(PROBE) --chip-description tools/probe-rs/esp32p4-tab5.yaml \
	    $(if $(RESET),--reset) $(if $(CMD),--cmd "$(CMD)" --until-done) \
	    --elf $(BUILD)/t5-kernel-esp32p4v1.elf $(addprefix --elf ,$(APP_ELFS))

probe-rs:
	cargo install --locked --git https://github.com/probe-rs/probe-rs --rev $(PROBE_RS_REV) \
	    probe-rs-tools --root $(BUILD)/probe-rs

# Writes the app store (appmeta @0x410000, apps @0x420000; doc/14 §2). This
# overwrites whatever is in those partitions.
flash-store: apps
	espflash write-bin --port $(PORT) 0x410000 $(STORE_DIR)/appmeta.bin
	espflash write-bin --port $(PORT) 0x420000 $(STORE_DIR)/apps.bin

monitor: schemas-p4
	stty -F $(PORT) raw -echo
	$(PRETTY) $(PORT)

# Run on QEMU virt with the decoded log on stdout.
run-virt: schemas-virt
	$(QEMU) $(QEMU_FLAGS) $(QEMU_STORE) -display none -monitor none -serial stdio \
	    -kernel $(BUILD)/t5-kernel-virt.elf | $(PRETTY)

# CI-style run: the kernel exits QEMU with status 0 only if the self-test
# passes; the raw log is kept in build/virt-console.bin and shown decoded.
test-virt: schemas-virt
	timeout 20 $(QEMU) $(QEMU_FLAGS) $(QEMU_STORE) -display none -monitor none \
	    -serial file:$(BUILD)/virt-console.bin -kernel $(BUILD)/t5-kernel-virt.elf; \
	    s=$$?; $(PRETTY) $(BUILD)/virt-console.bin; exit $$s

# ESP32-P4 rev v1.x emulator (doc/11). Runs the same kernel ELF as the
# hardware; exit status 0 only if the self-test passes.
T5EMU := emu/target/release/t5emu

emu:
	cd emu && cargo build --release

test-emu:
	cd emu && cargo test --release

# The app store goes into the emulated flash at its partition offsets.
EMU_FLASH := --flash $(STORE_DIR)/appmeta.bin@410000 --flash $(STORE_DIR)/apps.bin@420000

# The p4 emulator runs (doc/59): each its own target, so `make -j` runs
# them side by side; `test-p4emu` runs them all, P4EMU_JOBS at once,
# each run's output kept together.
P4EMU_JOBS ?= 12
P4EMU_RUNS := $(foreach n,01 02 03 04 05 06 07 08 09 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46,p4emu-run-$(n))
.PHONY: $(P4EMU_RUNS) p4emu-prep
p4emu-prep: emu schemas-p4 kupdate-test-kernels kernel-p4evb $(BUILD)/sd-test.img $(BUILD)/store-frag/apps.bin $(BUILD)/sd-ota.img

test-p4emu: p4emu-prep
	$(MAKE) --no-print-directory --output-sync=target -j$(P4EMU_JOBS) $(P4EMU_RUNS)

p4emu-run-01:
	$(T5EMU) $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-02:
	@# The EVB's kernel (doc/58 §3.1) passes the same self-test; it touches
	@# no I2C (0 transfers: the emulated Tab5's devices stay unused).
	$(T5EMU) --quiet $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --expect-event board.name --expect-event selftest.result \
	    $(BUILD)/t5-kernel-p4evb.elf

p4emu-run-03:
	@# Control commands after the self-test (doc/14 §3): start two apps, stop
	@# one, and check the events.
	$(T5EMU) --quiet $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "list" --ctl "run ticker 7 20" --ctl "run spin 9 3000" --ctl "kill 102" \
	    --ctl "run nosuchapp" --ctl "ps" \
	    --expect-event ticker.tick --expect-event proc.killed --expect-event ctl.failed \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-04:
	@# The RNG's health tests (doc/47 §6): a stuck register fails the
	@# start-up test, and the kernel does not start the generator.
	$(T5EMU) --quiet --rng-stuck $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --expect-event rng.health_fail --expect-event rng.no_entropy \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-05:
	@# Starting apps from a process (doc/38): init's test list starts hello
	@# and asks for an app not in the store; the kernel refuses a long name.
	@# `hello 3` fails each time: started again 4 times, then given up
	@# (doc/56 §3: after 0.5, 1, 2 and 4 s).
	$(T5EMU) --quiet --max-ms 40000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run init 2" --ctl "wait-ms 10000" \
	    --expect-event proc.spawned --expect-event hello.greet --expect-event proc.spawn_failed \
	    --expect-event init.done --expect-event proc.restart_planned --expect-event proc.restarted \
	    --expect-event proc.restart_gave_up \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-06:
	@# The settings (doc/50, doc/51): cfgctl asked for before cfg waits for
	@# svc:cfg (Needs); cfg makes its key and keeps the table in the
	@# emulated flash; cfgctl's self-test (values, a sealed secret not given
	@# back, limits, list); after cfg starts again the values are there.
	$(T5EMU) --quiet --max-ms 40000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run cfgctl 0" --ctl "wait-ms 500" --ctl "run cfg" --ctl "wait-ms 6000" \
	    --ctl "kill 101" --ctl "run cfg" --ctl "wait-ms 4000" --ctl "run cfgctl 1" \
	    --expect-event proc.waiting --expect-event cfg.key_created --expect-event cfgctl.ok \
	    --expect-event cfg.loaded --expect-event cfgctl.entry \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-07:
	@# The keyboard (doc/39): keys typed on the emulated keyboard on ExtPort1
	@# reach keylog (5 keys: press and release each, "aB\n" has Shift).
	$(T5EMU) --quiet $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run keylog 10 6" --ctl "wait-ms 500" --ctl "keys aB\n" \
	    --expect-event kbd.found --expect-event keylog.key --expect-event keylog.done \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-08:
	@# The terminal and the shell (doc/39): term starts shell, and a line
	@# typed on the keyboard runs; ps, apps and kill (doc/40).
	$(T5EMU) --quiet --max-ms 40000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run term" --ctl "wait-ms 3000" --ctl "keys echo hi\n" --ctl "wait-ms 1000" \
	    --ctl "keys ps\napps\nkill 99\n" --ctl "wait-ms 2000" \
	    --expect-event term.ready --expect-event term.session --expect-event shell.exec \
	    --expect-event shell.listed --expect-event proc.kill_denied \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-09:
	@# The terminal's drawing (doc/42): 100 lines that scroll, then every
	@# pixel checked against the cells.
	$(T5EMU) --quiet --max-ms 120000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run font" --ctl "wait-ms 300" --ctl "run term 1" --ctl "wait-ms 15000" \
	    --expect-event font.ready --expect-event term.font --expect-event term.bench_ok \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-10:
	@# Installing (M3d): the flash write path on a spare sector, an install,
	@# running it, a rejected image, removal. Stops on any flash access the
	@# chip would not survive (doc/14 §5.3).
	$(T5EMU) --quiet $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "flashtest" --ctl "install $(APPS_DIR)/greeter.t5a" --ctl "run greeter" \
	    --ctl "install $(APPS_DIR)/hello-untrusted.t5a" --ctl "remove greeter" --ctl "run greeter" \
	    --ctl "list" \
	    --expect-event flash.test --expect-event store.installed --expect-event hello.greet \
	    --expect-event store.rejected --expect-event store.removed \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-11:
	@# XIP (doc/15): an XIP app installed over the control channel runs from
	@# flash; removing it while it runs and installing another must not
	@# overwrite its extents (it would fault, and spin.done would be missing).
	$(T5EMU) --quiet $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "install $(APPS_DIR)/spin-xip.t5a" --ctl "run spin-xip 4 300" --ctl "remove spin-xip" \
	    --ctl "install $(APPS_DIR)/greeter.t5a" --ctl "run greeter" \
	    --expect-event spin.done --expect-event hello.greet \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-12:
	@# Flash work while both cores run processes (SMP S3, doc/33 §10): two
	@# spins without system calls (id 0x101, 0x102) keep both cores in
	@# U-mode while an app is installed, an XIP app is mapped and run, and
	@# both are removed. The other core is stopped by an interrupt before
	@# each cache stop; t5emu stops on any flash or PSRAM access while L2 is
	@# suspended.
	$(T5EMU) --quiet --max-ms 30000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run spin 257 80" --ctl "run spin 258 80" \
	    --ctl "install $(APPS_DIR)/greeter.t5a" --ctl "run greeter" \
	    --ctl "install $(APPS_DIR)/spin-xip.t5a" --ctl "run spin-xip 4 300" \
	    --ctl "remove spin-xip" --ctl "remove greeter" --ctl "wait-ms 3000" --ctl "ps" \
	    --expect-event store.installed --expect-event hello.greet --expect-event store.removed \
	    --expect-event spin.done --expect-event kernel.cpu \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-13:
	@# App update (M5b, doc/16 §4.3): over a valid greeter, 0.2 goes in
	@# pending and its normal end confirms it; a version that faults is rolled
	@# back and the next run is 0.2 again; an XIP update is confirmed after
	@# 2 s of running; `rollback` by command; `remove` drops both versions.
	$(T5EMU) --quiet --max-ms 40000 --linger-ms 3500 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "install $(APPS_DIR)/greeter.t5a" --ctl "install $(APPS_DIR)/greeter-0.2.t5a" --ctl "list" \
	    --ctl "run greeter" --ctl "wait-ms 300" --ctl "list" \
	    --ctl "install $(APPS_DIR)/greeter-bad.t5a" --ctl "run greeter 1" --ctl "wait-ms 300" --ctl "list" \
	    --ctl "run greeter" --ctl "wait-ms 300" \
	    --ctl "install $(APPS_DIR)/spin-xip.t5a" --ctl "install $(APPS_DIR)/spin-xip-0.2.t5a" --ctl "run spin-xip 4 3000" \
	    --ctl "wait-ms 2500" --ctl "list" \
	    --ctl "install $(APPS_DIR)/greeter-bad.t5a" --ctl "rollback greeter" --ctl "remove greeter" --ctl "list" \
	    --expect-event store.confirmed --expect-event store.rolled_back --expect-event proc.fault \
	    --expect-event hello.greet --expect-event spin.done \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-14:
	@# Board and display (M6a/M6b, doc/17): the modelled GT911 (ILI9881C lot)
	@# is identified over I2C and its touch point logged; t5emu fails on any
	@# write to IO expander #2. The display comes up and keeps refreshing
	@# (frames counted by `display`).
	$(T5EMU) --quiet --linger-ms 1500 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "i2cscan" --ctl "touch 1" --ctl "display" --ctl "rotate 180" --ctl "run paint 0 1" --ctl "wait-ms 1500" \
	    --expect-event display.panel --expect-event i2c.found --expect-event touch.point \
	    --expect-event display.ready --expect-event display.stats --expect-event paint.done \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-15:
	@# Debugging over the console line (doc/54): a tap and a screen capture
	@# sent as commands on USB Serial/JTAG. paint draws a square where the
	@# tap was (as well as at the modelled GT911's fixed point); the
	@# capture, turned to the user's view, has it there, the white corner
	@# top left and the background elsewhere.
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run paint 0 8" --ctl "wait-ms 1500" --ctl "serial tap 600 400" --ctl "wait-ms 500" --ctl "serial capture" \
	    --capture-out $(BUILD)/emu-capture.png --expect-pixel 600,400,fa8a --expect-pixel 20,20,ffff --expect-pixel 300,600,18c4 \
	    --expect-event paint.ready --expect-event touch.injected --expect-event capture.done \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-16:
	@# Network service (N1, doc/research/net-stack.md §4): over 127.0.0.1,
	@# 100 UDP round trips and 1 MiB of TCP with matching hashes.
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run net" --ctl "run nettest" --ctl "wait-ms 30000" \
	    --expect-event net.ready --expect-event nettest.done \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-17:
	@# Host link (N2a, doc/21): t5emu plays the PC (10.55.0.1) on the
	@# console line: 3 pings, then 16 KiB through the TCP echo server.
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run net" --ctl "run nettest 1" --ctl "wait-ms 40000" --host-peer 16384 \
	    --expect-event net.ready --expect-event hostlink.open \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-18:
	@# Forwarding (N3, doc/23): t5emu's PC reaches the test LAN's peer
	@# (192.168.50.2, inside `net`) through the Tab5: pings, 16 KiB of TCP
	@# echo, then Time Exceeded (TTL 1) and Destination Unreachable.
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run net 1" --ctl "wait-ms 40000" --host-peer 16384 \
	    --host-peer-target 192.168.50.2 --host-peer-start net.link \
	    --expect-event net.ready --expect-event hostlink.open \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-19:
	@# Filter (N4, doc/24): with netctl's test set, the test LAN's peer
	@# starts pings to the Tab5 (in) and the PC (forward), all dropped
	@# (net.drop); then the PC's traffic through the Tab5 still passes.
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run net 1" --ctl "run netctl 1" --ctl "wait-ms 40000" --host-peer 16384 \
	    --host-peer-target 192.168.50.2 --host-peer-start net.peer_probes \
	    --expect-event net.rules --expect-event net.drop --expect-event net.peer_probes \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-20:
	@# NAT and DHCP server (N5, doc/25): the test LAN's peer gets its
	@# address from net's DHCP server and pings the PC, which sees the
	@# pings come from 10.55.0.2 (masquerade on the host link).
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run net 3" --ctl "run netctl 5" --ctl "wait-ms 30000" --host-peer 16384 \
	    --host-peer-start none --host-peer-pinged-by 10.55.0.2 \
	    --expect-event net.dhcp_lease --expect-event net.peer_dhcp --expect-event net.peer_answered \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-21:
	@# IPv6 (doc/26): the PC (fd55::1) pings fd55::2 and sends 16 KiB
	@# through the TCP echo over IPv6; then ::1 (UDP and TCP) and a ping to
	@# the test LAN's peer (fd50::2).
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run net" --ctl "run nettest 1" --ctl "wait-ms 40000" --host-peer 16384 \
	    --host-peer-target fd55::2 \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-22:
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run net 1" --ctl "run nettest 6" --ctl "wait-ms 8000" --ctl "run nettest 7" --ctl "wait-ms 8000" \
	    --expect-event nettest.loop6 --expect-event nettest.ping6 \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-23:
	@# IPv6 forwarding (doc/27): the PC (fd55::1) reaches the test LAN's
	@# peer (fd50::2) through the Tab5: pings and 16 KiB of TCP echo.
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run net 1" --ctl "wait-ms 40000" --host-peer 16384 \
	    --host-peer-target fd50::2 --host-peer-start net.link \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-24:
	@# Kernel update (doc/16 §3.6): with the running kernel in ota_0, a
	@# tampered package is refused, the signed one goes into ota_1 and is
	@# selected in otadata, and reboot ends the run (no bootloader here).
	$(T5EMU) --quiet --max-ms 30000 $(EMU_FLASH) --flash $(BUILD)/t5-kernel-esp32p4v1.bin@10000 $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "kinfo" --ctl "kupdate $(KUPDATE_DIR)/tampered.t5k" --ctl "kupdate $(KERNEL_T5K)" --ctl "kinfo" \
	    --ctl "reboot" \
	    --expect-event kupdate.slot --expect-event ctl.failed --expect-event kupdate.slot_erased --expect-event kupdate.ready \
	    --expect-event kernel.reboot \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-25:
	@# An update that panics before confirming itself (otadata: seq 1 =
	@# ota_0, pending_verify) resets the chip so the bootloader rolls back,
	@# instead of stopping (doc/16 §6.10).
	$(T5EMU) --quiet --expect-reset $(EMU_FLASH) --flash $(KUPDATE_DIR)/otadata-pending.bin@d000 \
	    --flash $(KUPDATE_DIR)/bad-panic.bin@10000 \
	    --expect-event kupdate.pending --expect-event kernel.panic --expect-event kupdate.panic_reset \
	    $(KUPDATE_DIR)/bad-panic.elf

p4emu-run-26:
	@# Matter (doc/49 §16): `matter 1` (any link, the store in memory) and
	@# its own controller over 127.0.0.1: commissioning, a subscription to
	@# OnOff and to its StateChange event (doc/49 §22), a toggle (the
	@# device's event, the report and the event report), a read of a number
	@# and of a string. The apps' endpoints (doc/53): `mtep` brings a
	@# temperature sensor (endpoint 2) and a plug (3); the controller reads
	@# PartsList and the temperature and toggles the plug (`mtep.state`).
	@# `matter` takes ~3.2 s to be ready (its CA's key): 5 s before the
	@# first request (3 s raced it, doc/58 R5).
	$(T5EMU) --quiet --max-ms 180000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run net" --ctl "wait-ms 3000" --ctl "run matter 1" --ctl "wait-ms 5000" \
	    --ctl "run mtreq 14 1" --ctl "wait-ms 25000" --ctl "run mtreq 13 1" --ctl "wait-ms 5000" \
	    --ctl "run mtreq 17 1" --ctl "wait-ms 5000" \
	    --ctl "run mtreq 5 1" --ctl "wait-ms 5000" --ctl "run mtreq 6 1" --ctl "wait-ms 3000" \
	    --ctl "run mtreq 18 1" --ctl "wait-ms 3000" \
	    --ctl "run mtep 3 30" --ctl "wait-ms 1000" --ctl "run mtep 1 30" --ctl "wait-ms 4000" \
	    --ctl "run mtreq 19 1" --ctl "wait-ms 2000" --ctl "run mtreq 22 1" --ctl "wait-ms 3000" \
	    --ctl "run mtreq 20 1" --ctl "wait-ms 3000" --ctl "run mtreq 21 1" --ctl "wait-ms 4000" \
	    --expect-event matter.ready --expect-event matter.commissioned --expect-event matter.onoff \
	    --expect-event matter.report --expect-event matter.event \
	    --expect-event matter.ep_attached --expect-event mtep.set --expect-event matter.ep_onoff \
	    --expect-event mtep.state \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-27:
	@# Seats (doc/60 F1): two paints from the host. The first is in front;
	@# the second, not its child, goes behind. The tap reaches the front one
	@# only; after `focus` the capture shows the second's screen: its white
	@# corner, but the background where the tap was (it never saw it).
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run paint 0 20" --ctl "wait-ms 1500" --ctl "run paint 0 20" --ctl "wait-ms 1500" \
	    --ctl "serial tap 600 400" --ctl "wait-ms 500" --ctl "focus 102" --ctl "wait-ms 200" --ctl "serial capture" \
	    --capture-out $(BUILD)/emu-capture-seats.png --expect-pixel 600,400,18c4 --expect-pixel 20,20,ffff \
	    --expect-event paint.ready --expect-event screen.seat_behind --expect-event screen.focus \
	    --expect-event touch.injected --expect-event capture.done \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-28:
	@# The switch key and gesture (doc/60 F2): two paints, a tap to the
	@# front one, left Ctrl+Alt+Tab puts the second in front (the first is
	@# told HIDDEN, the second SHOWN), a swipe in from the left edge puts
	@# the first back. The capture is the first's screen with the tap's
	@# square but not the swipe's path (the gesture reaches no one).
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run paint 0 20" --ctl "wait-ms 1500" --ctl "run paint 0 20" --ctl "wait-ms 1500" \
	    --ctl "serial tap 600 400" --ctl "wait-ms 500" --ctl "keys <switch>" --ctl "wait-ms 500" \
	    --ctl "serial swipe 5 400 400 400 300" --ctl "wait-ms 800" --ctl "serial capture" \
	    --capture-out $(BUILD)/emu-capture-switch.png --expect-pixel 600,400,fa8a --expect-pixel 200,400,18c4 \
	    --expect-event paint.hidden --expect-event paint.shown --expect-event screen.focus \
	    --expect-event touch.injected --expect-event capture.done \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-29:
	@# fg and the child rule (doc/60 F3): paint started from term's shell
	@# comes in front by itself (term is its ancestor); the switch key
	@# puts term back, and `fg paint` typed in term puts paint in front
	@# again through display:focus. The capture is paint's screen.
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run term" --ctl "wait-ms 3000" --ctl "keys run paint 0 30\n" --ctl "wait-ms 2000" \
	    --ctl "keys <switch>" --ctl "wait-ms 500" --ctl "keys fg paint\n" --ctl "wait-ms 1000" --ctl "serial capture" \
	    --capture-out $(BUILD)/emu-capture-fg.png --expect-pixel 20,20,ffff \
	    --expect-event paint.ready --expect-event paint.hidden --expect-event paint.shown \
	    --expect-event shell.fg --expect-event screen.focus_by --expect-event capture.done \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-30:
	@# Framebuffers taken away (doc/60 §10): paint 2 (101, keeps drawing,
	@# knows nothing of seats) in front on framebuffer 0; two paints behind
	@# share framebuffer 1, the second takes it from the first (HIDDEN 1);
	@# with 102 in front, a fourth paint behind takes framebuffer 0 from 101,
	@# which stops at its next write (no_screen); `focus 101` gives it back
	@# and 101 goes on: the capture has its yellow square.
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run paint 2 30" --ctl "wait-ms 1000" --ctl "run paint 0 30" --ctl "wait-ms 1000" \
	    --ctl "run paint 0 30" --ctl "wait-ms 1000" --ctl "focus 102" --ctl "wait-ms 500" \
	    --ctl "run paint 0 30" --ctl "wait-ms 1000" --ctl "focus 101" --ctl "wait-ms 500" --ctl "serial capture" \
	    --capture-out $(BUILD)/emu-capture-takeaway.png --expect-pixel 600,400,fee7 \
	    --expect-event screen.taken_away --expect-event screen.no_screen --expect-event paint.hidden \
	    --expect-event paint.shown --expect-event capture.done \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-31:
	@# Memory in top (doc/61): proc:mem and the text size in proc:list;
	@# shell.top logs what the first screen showed.
	$(T5EMU) --quiet --max-ms 40000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run term" --ctl "wait-ms 3000" --ctl "keys top\n" --ctl "wait-ms 2500" --ctl "keys q" --ctl "wait-ms 500" \
	    --expect-event term.ready --expect-event shell.top \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-32:
	@# Tab completion and the history (doc/62): `upt<Tab>` runs uptime,
	@# `fg ter<Tab>` puts term in front by name; after help's lines,
	@# Alt+Up (and Shift+PgUp) move the view back, Alt+Down forward
	@# (term.view), and the capture
	@# shows the label at the top right (reversed: not the background).
	$(T5EMU) --quiet --max-ms 50000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run term" --ctl "wait-ms 3000" --ctl "keys upt\t\n" --ctl "wait-ms 800" --ctl "keys fg ter\t\n" --ctl "wait-ms 800" \
	    --ctl "keys help\n" --ctl "wait-ms 1500" --ctl "keys <alt-up><pgup><alt-down><alt-up>" --ctl "wait-ms 800" --ctl "serial capture" \
	    --capture-out $(BUILD)/emu-capture-history.png --expect-pixel 1270,4,d69a \
	    --expect-event shell.fg --expect-event term.view --expect-event capture.done \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

$(BUILD)/sd-test.img: tools/mksdimg.sh
	tools/mksdimg.sh $@

p4emu-run-33:
	@# The microSD card (doc/64) and long names (doc/63): fs on an image
	@# made by mtools; a long-named folder and file, with an escaped space
	@# and in quotes in another case, are read (shell.cat). Files and a
	@# folder are made with long names and an 8.3 one, a long-named file is
	@# deleted; mtools reads the card the run left, and fatcheck.py finds
	@# every LFN entry in its place (no orphans).
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --sd $(BUILD)/sd-test.img --sd-out $(BUILD)/sd-test-after.img \
	    --ctl "run fs" --ctl "wait-ms 2000" --ctl "run term" --ctl "wait-ms 3000" \
	    --ctl "keys cat My\\ Documents/notes-about-tab5.txt\n" --ctl "wait-ms 1500" \
	    --ctl "keys cat \"long file name.txt\"\n" --ctl "wait-ms 1500" \
	    --ctl "keys write new.txt made in emu\n" --ctl "wait-ms 1500" \
	    --ctl "keys write \"Notes From Emu.txt\" hello long\n" --ctl "wait-ms 1500" \
	    --ctl "keys write \"Notes From Emulator.txt\" second\n" --ctl "wait-ms 1500" \
	    --ctl "keys mkdir \"New Folder\"\n" --ctl "wait-ms 1500" \
	    --ctl "keys write New\\ Folder/inside.txt deep\n" --ctl "wait-ms 1500" \
	    --ctl "keys write NOTES.TXT plain\n" --ctl "wait-ms 1500" \
	    --ctl "keys rm \"long file name.txt\"\n" --ctl "wait-ms 1500" \
	    --expect-event fs.ready --expect-event shell.cat \
	    $(BUILD)/t5-kernel-esp32p4v1.elf
	MTOOLS_SKIP_CHECK=1 mtype -i $(BUILD)/sd-test-after.img@@1M ::/new.txt | grep -qx "made in emu"
	MTOOLS_SKIP_CHECK=1 mtype -i $(BUILD)/sd-test-after.img@@1M "::/Notes From Emu.txt" | grep -qx "hello long"
	MTOOLS_SKIP_CHECK=1 mtype -i $(BUILD)/sd-test-after.img@@1M "::/New Folder/inside.txt" | grep -qx "deep"
	MTOOLS_SKIP_CHECK=1 mtype -i $(BUILD)/sd-test-after.img@@1M ::/NOTES.TXT | grep -qx "plain"
	MTOOLS_SKIP_CHECK=1 mdir -i $(BUILD)/sd-test-after.img@@1M :: | grep -q "NOTESF~2 TXT .*Notes From Emulator.txt"
	! MTOOLS_SKIP_CHECK=1 mtype -i $(BUILD)/sd-test-after.img@@1M "::/Long File Name.txt" >/dev/null 2>&1
	python3 tools/fatcheck.py $(BUILD)/sd-test-after.img

# Apps installed over their valid versions in p4emu-run-34: each keeps its
# old version, more than the catalog (63 entries) holds.
CATALOG_FILL := hello keylog ticker spin blescan timers hcitest bleecho mtreq paint mtep blegatt cfgctl echo fstest netctl c6ota mtpanel nettest hello-strong hello-xip term shell cfg wifi mtdash init

p4emu-run-34:
	@# A full catalog (doc/65): 27 apps installed over their valid
	@# versions keep 27 old versions beside 37 entries; the old versions of
	@# other apps go (store.previous_dropped) instead of `catalog full`.
	$(T5EMU) --max-ms 120000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    $(foreach a,$(CATALOG_FILL),--ctl "install $(if $(filter init,$(a)),$(AUTO_DIR),$(APPS_DIR))/$(a).t5a") --ctl "run hello" \
	    --expect-event store.installed --expect-event store.previous_dropped \
	    $(BUILD)/t5-kernel-esp32p4v1.elf > $(BUILD)/emu-catalog.txt 2>&1 || { tail -20 $(BUILD)/emu-catalog.txt; false; }
	@# Every install went in, the last too.
	! grep -q "cmd=install_end ok=false" $(BUILD)/emu-catalog.txt
	grep -q "store.installed name=init " $(BUILD)/emu-catalog.txt

$(BUILD)/store-frag/apps.bin: $(STORE_DIR)/apps.bin tools/fragstore.py
	python3 tools/fragstore.py $(STORE_DIR) $(BUILD)/store-frag

p4emu-run-35:
	@# Moving apps to make room (doc/65): the store with one or two free
	@# extents before each app (tools/fragstore.py: 59 free, no run longer
	@# than 2); matter (8 extents) goes in after apps are moved out of a
	@# run (store.moved), and a moved app still loads and runs.
	$(T5EMU) --max-ms 90000 --flash $(BUILD)/store-frag/appmeta.bin@410000 --flash $(BUILD)/store-frag/apps.bin@420000 \
	    $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "install $(APPS_DIR)/matter.t5a" --ctl "run hello" \
	    --expect-event store.moved --expect-event store.installed --expect-event hello.greet \
	    $(BUILD)/t5-kernel-esp32p4v1.elf > $(BUILD)/emu-frag.txt 2>&1 || { tail -20 $(BUILD)/emu-frag.txt; false; }
	grep -q "store.moved name=hello " $(BUILD)/emu-frag.txt
	grep -q "store.installed name=matter " $(BUILD)/emu-frag.txt

# The card for p4emu-run-36: the test card and, in UPDATE, images to install.
$(BUILD)/sd-ota.img: $(BUILD)/sd-test.img $(APPS_DIR)/greeter.t5a $(APPS_DIR)/hello-untrusted.t5a $(KERNEL_T5K)
	cp $(BUILD)/sd-test.img $@
	MTOOLS_SKIP_CHECK=1 mmd -i $@@@1M ::/UPDATE
	MTOOLS_SKIP_CHECK=1 mcopy -i $@@@1M $(APPS_DIR)/greeter.t5a ::/UPDATE/greeter.t5a
	MTOOLS_SKIP_CHECK=1 mcopy -i $@@@1M $(APPS_DIR)/hello-untrusted.t5a ::/UPDATE/untrusted.t5a
	MTOOLS_SKIP_CHECK=1 mcopy -i $@@@1M $(KERNEL_T5K) ::/UPDATE/kernel.t5k

p4emu-run-36:
	@# Updates from an app (doc/67): the shell's `ota` starts the update
	@# service, which installs an app from the SD card through the kernel's
	@# `update` (store.installed; the new app runs), is refused an image
	@# signed with an unknown key, writes a kernel package into the other
	@# slot (kupdate.ready) and restarts (update.reboot ends the run).
	$(T5EMU) --quiet --max-ms 120000 $(EMU_FLASH) --flash $(BUILD)/t5-kernel-esp32p4v1.bin@10000 $(addprefix --elf ,$(APP_ELFS)) \
	    --sd $(BUILD)/sd-ota.img \
	    --ctl "run fs" --ctl "wait-ms 2000" --ctl "run term" --ctl "wait-ms 3000" \
	    --ctl "keys ota app UPDATE/greeter.t5a\n" --ctl "wait-ms 10000" \
	    --ctl "keys ota app UPDATE/untrusted.t5a\n" --ctl "wait-ms 6000" \
	    --ctl "keys run greeter\n" --ctl "wait-ms 2000" \
	    --ctl "keys ota kernel UPDATE/kernel.t5k\n" --ctl "wait-ms 30000" \
	    --ctl "keys ota reboot\n" --ctl "wait-ms 3000" \
	    --expect-event ota.ready --expect-event ota.installed --expect-event store.installed \
	    --expect-event store.rejected --expect-event hello.greet --expect-event kupdate.ready \
	    --expect-event update.reboot \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-37:
	@# Updates over the network (doc/67 stage 2): the host peer serves
	@# build/ over HTTP (10.55.0.1:8080, on the host link); the shell's
	@# `ota app http://...` installs greeter (it runs), a missing file is
	@# a 404 (ota.http_status), and the kernel package goes into the other
	@# slot (kupdate.ready). ota stays up for all of them (no app faults;
	@# its stack once overflowed in the HTTP path).
	$(T5EMU) --max-ms 150000 $(EMU_FLASH) --flash $(BUILD)/t5-kernel-esp32p4v1.bin@10000 $(addprefix --elf ,$(APP_ELFS)) \
	    --host-peer 0 --host-peer-start none --http-dir $(BUILD) \
	    --ctl "run net" --ctl "wait-ms 3000" --ctl "run term" --ctl "wait-ms 3000" \
	    --ctl "keys ota app http://10.55.0.1:8080/apps/greeter.t5a\n" --ctl "wait-ms 10000" \
	    --ctl "keys run greeter\n" --ctl "wait-ms 2000" \
	    --ctl "keys ota app http://10.55.0.1:8080/apps/missing.t5a\n" --ctl "wait-ms 5000" \
	    --ctl "keys ota kernel http://10.55.0.1:8080/t5-kernel-esp32p4v1.t5k\n" --ctl "wait-ms 45000" \
	    --expect-event ota.installed --expect-event store.installed --expect-event hello.greet \
	    --expect-event ota.http_status --expect-event kupdate.ready \
	    $(BUILD)/t5-kernel-esp32p4v1.elf > $(BUILD)/emu-ota-net.txt 2>&1 || { tail -20 $(BUILD)/emu-ota-net.txt; false; }
	! grep -aqE "proc.fault: 1[0-9][0-9] killed" $(BUILD)/emu-ota-net.txt
	test "$$(grep -ac ' ota.ready' $(BUILD)/emu-ota-net.txt)" = 1

p4emu-run-38:
	@# A kernel installed over the network confirms itself once the network
	@# works (doc/67 §5): started from otadata `pending_verify` whose label
	@# names 10.55.0.1:8080, it passes its self-test and waits
	@# (kupdate.awaiting_net); ota connects to that server over the host
	@# link and confirms it (kupdate.confirmed).
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) --flash $(KUPDATE_DIR)/otadata-netconfirm.bin@d000 \
	    $(addprefix --elf ,$(APP_ELFS)) \
	    --host-peer 0 --host-peer-start none --http-dir $(BUILD) \
	    --ctl "run net" --ctl "wait-ms 3000" --ctl "run ota" --ctl "wait-ms 8000" \
	    --expect-event kupdate.pending --expect-event kupdate.awaiting_net --expect-event ota.confirming \
	    --expect-event kupdate.net_reached --expect-event kupdate.confirmed --expect-event ota.confirmed \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-39:
	@# The same kernel when the server cannot be reached (no HTTP on the
	@# host peer): after update::NET_CONFIRM_MS (300 s) it marks itself
	@# invalid and restarts, so the bootloader would start the old one.
	$(T5EMU) --quiet --max-ms 340000 --expect-reset $(EMU_FLASH) --flash $(KUPDATE_DIR)/otadata-netconfirm.bin@d000 \
	    $(addprefix --elf ,$(APP_ELFS)) \
	    --host-peer 0 --host-peer-start none \
	    --ctl "run net" --ctl "wait-ms 3000" --ctl "run ota" --ctl "wait-ms 310000" \
	    --expect-event kupdate.awaiting_net --expect-event ota.confirming \
	    --expect-event kupdate.no_network --expect-event kupdate.rejected \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-40:
	@# kumiki-gfx on tab5-os (doc/68): kgfx draws with kumiki's FbDisplay
	@# through kumiki_t5os::FbPanel, turned as the user sees the screen
	@# (270°): the white square at the user's top left, the background and
	@# the red bar at the bottom left in the capture (the user's view).
	$(T5EMU) --quiet --max-ms 40000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run kgfx 3" --ctl "wait-ms 2000" --ctl "serial capture" \
	    --capture-out $(BUILD)/emu-capture-kgfx.png \
	    --expect-pixel 20,20,ffff --expect-pixel 600,300,18c4 --expect-pixel 50,690,f800 \
	    --expect-event kgfx.ready --expect-event kgfx.done --expect-event capture.done \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-41:
	@# kgfx when its framebuffer is taken away (doc/60 §10, doc/68): kgfx in
	@# front on framebuffer 0; with a paint (102) in front on framebuffer 1,
	@# a second paint behind takes framebuffer 0 from kgfx (HIDDEN 1);
	@# `focus 101` gives it back black (SHOWN 1) and kgfx draws everything
	@# again (kgfx.redraw): its scene is in the capture.
	$(T5EMU) --quiet --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run kgfx 12" --ctl "wait-ms 1500" --ctl "run paint 0 12" --ctl "wait-ms 1000" \
	    --ctl "focus 102" --ctl "wait-ms 500" --ctl "run paint 0 12" --ctl "wait-ms 1000" \
	    --ctl "focus 101" --ctl "wait-ms 1500" --ctl "serial capture" \
	    --capture-out $(BUILD)/emu-capture-kgfx-redraw.png \
	    --expect-pixel 20,20,ffff --expect-pixel 600,300,18c4 --expect-pixel 50,690,f800 \
	    --expect-event screen.taken_away --expect-event kgfx.redraw --expect-event capture.done \
	    $(BUILD)/t5-kernel-esp32p4v1.elf

p4emu-run-42:
	@# The IMU from an app (doc/69): kumiki's kumiki-bmi270 on
	@# kumiki_t5os::I2cDevice (the kernel's i2c:int/0x68) finds the BMI270,
	@# uploads its configuration (33-byte writes, over the FIFO: in two
	@# pieces with END) and reads samples: 1 g on Z (az_mg=1000).
	$(T5EMU) --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run imu 3" --ctl "wait-ms 4000" \
	    --expect-event imu.found --expect-event imu.ready --expect-event imu.sample \
	    $(BUILD)/t5-kernel-esp32p4v1.elf > $(BUILD)/emu-imu.txt 2>&1 || { tail -20 $(BUILD)/emu-imu.txt; false; }
	grep -q "imu.sample n=0 ax_mg=0 ay_mg=0 az_mg=1000 gx=10 gy=-20 gz=30" $(BUILD)/emu-imu.txt

p4emu-run-43:
	@# Audio stage A1 (doc/70): the kernel starts I2S0 on the AHB-GDMA
	@# (kumiki-p4v1-hal) before the display, at 48 kHz (375 TX and 750 RX
	@# descriptors a second); with nothing pushed it plays silence. The
	@# DMA's interrupt stays off until something uses the stream (A2).
	$(T5EMU) --max-ms 30000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --audio-out $(BUILD)/emu-audio.wav --ctl "wait-ms 1000" --ctl audio \
	    --expect-event audio.started --expect-event audio.stats \
	    $(BUILD)/t5-kernel-esp32p4v1.elf > $(BUILD)/emu-audio.txt 2>&1 || { tail -20 $(BUILD)/emu-audio.txt; false; }
	grep -Eq "audio.started rate_millihz=4[78][0-9]{6} " $(BUILD)/emu-audio.txt
	grep -Eq "audio.stats irqs=0 tx_descs=[0-9]+ tx_underruns=0 " $(BUILD)/emu-audio.txt
	grep -Eq "t5emu: audio: [0-9]{4,} descriptor\(s\) sent" $(BUILD)/emu-audio.txt
	@# The codecs (kumiki's drivers) before the display, the speaker amp
	@# (expander #1 bit1) off during their set-up and on after, kept by the
	@# display's writes (doc/70 §9).
	grep -Eq "board.codecs es8388=ok es7210=ok" $(BUILD)/emu-audio.txt
	grep -Eq "t5emu: codecs: 32 ES8388 and 30 ES7210 register write\(s\); expander #1 OUT=0x.[014589cd] before" $(BUILD)/emu-audio.txt
	grep -Eq "expander #1 DIR=0x6f OUT=0x66; 0 write" $(BUILD)/emu-audio.txt
	grep -B100 "board.codecs" $(BUILD)/emu-audio.txt | grep -q "display.panel" && { echo "codecs after the display"; false; } || true
	grep -B100 "audio.started" $(BUILD)/emu-audio.txt | grep -q "board.ready" && { echo "audio after the display"; false; } || true
	python3 -c "import sys; d = open(sys.argv[1], 'rb').read()[44:]; sys.exit(0 if len(d) > 100000 and not any(d) else 1)" $(BUILD)/emu-audio.wav

p4emu-run-44:
	@# Audio stage A2 (doc/70): `beep` plays 1 kHz for 500 ms through
	@# audio:out (the kernel's queue, mixed into the stream; the WAV holds
	@# it at a quarter of full scale) while `mic` reads audio:in (the
	@# emulator's 440 Hz tone at 4096: peak about 4096, RMS about 2896).
	@# `mic` starts once `beep` plays: loading it holds the kernel ~170 ms
	@# (the signature at 40 MHz), longer than the TX ring lasts: what was
	@# pushed plays again until the next poll silences the ring (laps_lost,
	@# kumiki 64b1f43), so the 1.5 s tone stays under 1700 ms (a ring left
	@# unpolled for good would repeat forever).
	$(T5EMU) --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --audio-out $(BUILD)/emu-beep.wav --ctl "run beep 1000 1500" --ctl "wait-ms 300" --ctl "run mic 500" --ctl "wait-ms 3000" --ctl audio \
	    --expect-event beep.start --expect-event beep.done --expect-event mic.level --expect-event mic.done \
	    $(BUILD)/t5-kernel-esp32p4v1.elf > $(BUILD)/emu-beep.txt 2>&1 || { tail -20 $(BUILD)/emu-beep.txt; false; }
	grep -q "beep.done frames=72000" $(BUILD)/emu-beep.txt
	grep -Eq "mic.level n=2 peak=40[0-9][0-9] rms=28[0-9][0-9]" $(BUILD)/emu-beep.txt
	grep -Eq "mic.done frames=24[0-9]{3} lost=0" $(BUILD)/emu-beep.txt
	grep -Eq "audio.stats .* laps_lost=[1-9]$$" $(BUILD)/emu-beep.txt
	tools/wavcheck.py $(BUILD)/emu-beep.wav 1000 8000 1700

p4emu-run-45:
	@# Audio stage A4 (doc/70 §10): two `beep`s at once, 1 kHz for 2 s and,
	@# once it plays, 1.5 kHz for 1 s. The kernel adds the queues: the WAV
	@# holds both tones (8192 each) for most of a second, summed to a peak
	@# about 16384. Loading the second holds the kernel (laps_lost: the
	@# first tone's last turn plays again), which wavmix does not mind.
	$(T5EMU) --max-ms 60000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --audio-out $(BUILD)/emu-mix.wav --ctl "run beep 1000 2000" --ctl "wait-ms 300" --ctl "run beep 1500 1000" --ctl "wait-ms 3000" --ctl audio \
	    --expect-event beep.start --expect-event beep.done --expect-event audio.stats \
	    $(BUILD)/t5-kernel-esp32p4v1.elf > $(BUILD)/emu-mix.txt 2>&1 || { tail -20 $(BUILD)/emu-mix.txt; false; }
	test $$(grep -c "beep.done frames=" $(BUILD)/emu-mix.txt) -eq 2
	grep -q "beep.done frames=96000" $(BUILD)/emu-mix.txt
	grep -q "beep.done frames=48000" $(BUILD)/emu-mix.txt
	tools/wavmix.py $(BUILD)/emu-mix.wav 1000 1500 6000 700

p4emu-run-46:
	@# The speaker's level (doc/70 §11): the shell's `volume 30` sets the
	@# ES8388's DAC to 35 dB down (attenuation 70 half-dB steps, 0x46, both
	@# channels), then `volume 0` mutes it (DACCONTROL3 0x24) and leaves the
	@# attenuation.
	$(T5EMU) --max-ms 40000 $(EMU_FLASH) $(addprefix --elf ,$(APP_ELFS)) \
	    --ctl "run term" --ctl "wait-ms 3000" --ctl "keys volume 30\n" --ctl "wait-ms 800" --ctl "keys volume 0\n" --ctl "wait-ms 800" \
	    --expect-event term.ready --expect-event shell.volume \
	    $(BUILD)/t5-kernel-esp32p4v1.elf > $(BUILD)/emu-volume.txt 2>&1 || { tail -20 $(BUILD)/emu-volume.txt; false; }
	grep -q "audio.volume volume=30 ok=true" $(BUILD)/emu-volume.txt
	grep -q "shell.volume volume=30" $(BUILD)/emu-volume.txt
	grep -q "shell.volume volume=0" $(BUILD)/emu-volume.txt
	grep -q "t5emu: ES8388 DAC: control3=0x24 volume=0x46,0x46" $(BUILD)/emu-volume.txt

clean:
	rm -rf $(BUILD) kernel/target user/target tools/target emu/target
