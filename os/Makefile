# Build configuration
ARCH ?= riscv64
MODE ?= release
BOARD ?= qemu
TEST ?=

ifeq ($(ARCH),riscv64)
TARGET := riscv64gc-unknown-none-elf
QEMU := qemu-system-riscv64
OBJDUMP_ARCH := riscv64
OBJCOPY_ARCH := riscv64
GDB := riscv64-unknown-elf-gdb
GDB_ARCH := riscv:rv64
else
$(error Unsupported ARCH '$(ARCH)' in os/Makefile. Current kernel Makefile only supports ARCH=riscv64)
endif

ifeq ($(MODE),release)
MODE_ARG := --release
endif

# Paths
USER_DIR := ../user
TARGET_DIR := target/$(TARGET)/$(MODE)
KERNEL_ELF := $(TARGET_DIR)/os
KERNEL_BIN := $(KERNEL_ELF).bin
DISASM_TMP := $(TARGET_DIR)/asm
FS_IMG := $(USER_DIR)/target/$(TARGET)/$(MODE)/fs.img
ROOTFS_DIR := $(USER_DIR)/target/$(TARGET)/$(MODE)/rootfs
APPS := $(USER_DIR)/src/bin/*

# Ext4 image layout for local development
FS_BLOCK_SIZE := 4096
FS_SIZE_MIB := 128
FS_BLOCK_COUNT := 32768

# QEMU defaults
MEM ?= 1G
SMP ?= 1
BIOS ?= default
RTC_BASE ?= utc
HOSTFWD_UDP ?= 6200
GUEST_UDP ?= 2000
HOSTFWD_TCP ?= 6201
GUEST_TCP ?= 80
GUI ?= off
TEST_DISK ?=
AUX_DISK ?=

ifeq ($(GUI),off)
QEMU_CONSOLE_ARGS := -nographic
else
QEMU_CONSOLE_ARGS := -serial stdio
endif

QEMU_PRIMARY_DISK_ARGS = \
	-drive file=$(TEST_DISK),if=none,format=raw,id=x0 \
	-device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0

ifneq ($(strip $(AUX_DISK)),)
QEMU_AUX_DISK_ARGS = \
	-drive file=$(AUX_DISK),if=none,format=raw,id=x1 \
	-device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1
endif

# The current kernel still initializes GPU/keyboard/mouse during boot,
# so local QEMU runs keep those devices attached even though the official
# contest command only requires block + net.
QEMU_ARGS = \
	-machine virt \
	-m $(MEM) \
	$(QEMU_CONSOLE_ARGS) \
	-smp $(SMP) \
	-bios $(BIOS) \
	-kernel $(KERNEL_ELF) \
	$(QEMU_PRIMARY_DISK_ARGS) \
	$(QEMU_AUX_DISK_ARGS) \
	-no-reboot \
	-device virtio-gpu-device \
	-device virtio-keyboard-device \
	-device virtio-mouse-device \
	-device virtio-net-device,netdev=net0 \
	-netdev user,id=net0,hostfwd=udp::$(HOSTFWD_UDP)-:$(GUEST_UDP),hostfwd=tcp::$(HOSTFWD_TCP)-:$(GUEST_TCP) \
	-rtc base=$(RTC_BASE)

# Tooling
OBJDUMP := rust-objdump --arch-name=$(OBJDUMP_ARCH)
OBJCOPY := rust-objcopy --binary-architecture=$(OBJCOPY_ARCH)
DISASM ?= -x

build: env $(KERNEL_BIN) fs-img

env:
	(rustup target list | grep "$(TARGET) (installed)") || rustup target add $(TARGET)
	command -v rust-objcopy >/dev/null 2>&1 || cargo install cargo-binutils
	(rustup component list --installed | grep '^rust-src') >/dev/null 2>&1 || rustup component add rust-src
	(rustup component list --installed | grep '^llvm-tools-preview') >/dev/null 2>&1 || rustup component add llvm-tools-preview

kernel: env $(KERNEL_ELF)

$(KERNEL_ELF):
	@echo Platform: $(BOARD)
	@cp src/linker-$(BOARD).ld src/linker.ld
	@cargo build $(MODE_ARG)
	@rm src/linker.ld

$(KERNEL_BIN): $(KERNEL_ELF)
	@$(OBJCOPY) $(KERNEL_ELF) --strip-all -O binary $@

fs-img: $(APPS)
	@cd $(USER_DIR) && make build TEST=$(TEST)
	@rm -rf $(ROOTFS_DIR)
	@mkdir -p $(ROOTFS_DIR)
	@for app in $(APPS); do \
		name=$$(basename $$app .rs); \
		cp $(USER_DIR)/target/$(TARGET)/$(MODE)/$$name $(ROOTFS_DIR)/$$name; \
	done
	@rm -f $(FS_IMG)
	@truncate -s $(FS_SIZE_MIB)M $(FS_IMG)
	@mke2fs -q -t ext4 -b $(FS_BLOCK_SIZE) -O ^orphan_file,^metadata_csum_seed -d $(ROOTFS_DIR) -F $(FS_IMG) $(FS_BLOCK_COUNT)

$(APPS):

run: qemu-version-check kernel
	@if [ -z "$(TEST_DISK)" ]; then \
		echo "TEST_DISK is required for contest-style run."; \
		echo "Example: make run TEST_DISK=/path/to/sdcard-rv.img AUX_DISK=../disk.img"; \
		echo "For local development with the generated fs.img, use: make run-dev"; \
		exit 1; \
	fi
	@if [ ! -f "$(TEST_DISK)" ]; then \
		echo "TEST_DISK does not exist: $(TEST_DISK)"; \
		exit 1; \
	fi
	@if [ -n "$(AUX_DISK)" ] && [ ! -f "$(AUX_DISK)" ]; then \
		echo "AUX_DISK does not exist: $(AUX_DISK)"; \
		exit 1; \
	fi
	@$(QEMU) $(QEMU_ARGS)

run-dev: qemu-version-check build
	@$(MAKE) --no-print-directory run TEST_DISK=$(FS_IMG) AUX_DISK="$(AUX_DISK)"

fdt:
	@$(QEMU) -machine virt,dumpdtb=virt.out -m 128m
	@fdtdump virt.out

qemu-version-check:
	@sh scripts/qemu-ver-check.sh $(QEMU)

debug: qemu-version-check kernel
	@if [ -z "$(TEST_DISK)" ]; then \
		echo "TEST_DISK is required. Use TEST_DISK=/path/to/sdcard-rv.img or run-dev for the local fs image."; \
		exit 1; \
	fi
	@if [ ! -f "$(TEST_DISK)" ]; then \
		echo "TEST_DISK does not exist: $(TEST_DISK)"; \
		exit 1; \
	fi
	@if [ -n "$(AUX_DISK)" ] && [ ! -f "$(AUX_DISK)" ]; then \
		echo "AUX_DISK does not exist: $(AUX_DISK)"; \
		exit 1; \
	fi
	@tmux new-session -d \
		"$(QEMU) $(QEMU_ARGS) -s -S" && \
		tmux split-window -h "$(GDB) -ex 'file $(KERNEL_ELF)' -ex 'set arch $(GDB_ARCH)' -ex 'target remote localhost:1234'" && \
		tmux -2 attach-session -d

gdbserver: qemu-version-check kernel
	@if [ -z "$(TEST_DISK)" ]; then \
		echo "TEST_DISK is required. Use TEST_DISK=/path/to/sdcard-rv.img or run-dev for the local fs image."; \
		exit 1; \
	fi
	@if [ ! -f "$(TEST_DISK)" ]; then \
		echo "TEST_DISK does not exist: $(TEST_DISK)"; \
		exit 1; \
	fi
	@if [ -n "$(AUX_DISK)" ] && [ ! -f "$(AUX_DISK)" ]; then \
		echo "AUX_DISK does not exist: $(AUX_DISK)"; \
		exit 1; \
	fi
	@$(QEMU) $(QEMU_ARGS) -s -S

gdbclient:
	@$(GDB) -ex 'file $(KERNEL_ELF)' -ex 'set arch $(GDB_ARCH)' -ex 'target remote localhost:1234'

clean:
	@cargo clean

disasm: kernel
	@$(OBJDUMP) $(DISASM) $(KERNEL_ELF) | less

disasm-vim: kernel
	@$(OBJDUMP) $(DISASM) $(KERNEL_ELF) > $(DISASM_TMP)
	@nvim $(DISASM_TMP)
	@rm $(DISASM_TMP)

.PHONY: build env kernel clean disasm disasm-vim fs-img run run-dev debug gdbserver gdbclient fdt qemu-version-check
