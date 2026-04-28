# Build configuration
ARCH ?= riscv64
MODE ?= release
BOARD ?= qemu
TEST ?=
CARGO_HOME ?= $(abspath ../vendor)
export CARGO_HOME

ifeq ($(ARCH),riscv64)
TARGET := riscv64gc-unknown-none-elf
QEMU := qemu-system-riscv64
OBJDUMP_ARCH := riscv64
OBJCOPY_ARCH := riscv64
GDB := riscv64-unknown-elf-gdb
GDB_ARCH := riscv:rv64
else ifeq ($(ARCH),loongarch64)
TARGET := loongarch64-unknown-none
QEMU := qemu-system-loongarch64
OBJDUMP_ARCH := loongarch64
OBJCOPY_ARCH := loongarch64
GDB := loongarch64-linux-gnu-gdb
GDB_ARCH := loongarch64
else
$(error Unsupported ARCH '$(ARCH)' in os/Makefile. Supported values are riscv64 and loongarch64)
endif

ifeq ($(MODE),release)
MODE_ARG := --release
endif

# Paths
TARGET_DIR := target/$(TARGET)/$(MODE)
KERNEL_ELF := $(TARGET_DIR)/os
KERNEL_BIN := $(KERNEL_ELF).bin
DISASM_TMP := $(TARGET_DIR)/asm
LINKER_SCRIPT := src/linker-$(BOARD).ld
ifeq ($(ARCH),loongarch64)
LINKER_SCRIPT := src/linker-loongarch64.ld
endif

# QEMU defaults
MEM ?= 1G
SMP ?= 1
BIOS ?= default
RTC_BASE ?= utc
GUI ?= off
TEST_DISK ?=../sdcard-rv.img
AUX_DISK ?=
PRIMARY_DISK ?=$(TEST_DISK)
EXTRA_QEMU_ARGS ?=

ifeq ($(GUI),off)
QEMU_CONSOLE_ARGS := -nographic
else
QEMU_CONSOLE_ARGS := -serial stdio
endif

ifeq ($(ARCH),loongarch64)
QEMU_PRIMARY_DISK_ARGS = \
	-drive file=$(PRIMARY_DISK),if=none,format=raw,id=x0 \
	-device virtio-blk-pci,drive=x0

ifneq ($(strip $(AUX_DISK)),)
QEMU_AUX_DISK_ARGS = \
	-drive file=$(AUX_DISK),if=none,format=raw,id=x1 \
	-device virtio-blk-pci,drive=x1
endif

QEMU_COMMON_ARGS = \
	-machine virt \
	-m $(MEM) \
	$(QEMU_CONSOLE_ARGS) \
	-smp $(SMP) \
	-kernel $(KERNEL_ELF) \
	$(QEMU_AUX_DISK_ARGS) \
	-no-reboot \
	-rtc base=$(RTC_BASE)

QEMU_NET_ARGS = \
	-device virtio-net-pci,netdev=net0 \
	-netdev user,id=net0,hostfwd=tcp::5555-:5555,hostfwd=udp::5555-:5555
else
QEMU_PRIMARY_DISK_ARGS = \
	-drive file=$(PRIMARY_DISK),if=none,format=raw,id=x0 \
	-device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0

ifneq ($(strip $(AUX_DISK)),)
QEMU_AUX_DISK_ARGS = \
	-drive file=$(AUX_DISK),if=none,format=raw,id=x1 \
	-device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1
endif

QEMU_COMMON_ARGS = \
	-machine virt \
	-m $(MEM) \
	$(QEMU_CONSOLE_ARGS) \
	-smp $(SMP) \
	-bios $(BIOS) \
	-kernel $(KERNEL_ELF) \
	$(QEMU_AUX_DISK_ARGS) \
	-no-reboot \
	-rtc base=$(RTC_BASE)

QEMU_NET_ARGS = \
	-device virtio-net-device,netdev=net \
	-netdev user,id=net
endif

QEMU_ARGS = \
	$(QEMU_PRIMARY_DISK_ARGS) \
	$(QEMU_NET_ARGS) \
	$(QEMU_COMMON_ARGS)

# Tooling
OBJDUMP := rust-objdump --arch-name=$(OBJDUMP_ARCH)
OBJCOPY := rust-objcopy --binary-architecture=$(OBJCOPY_ARCH)
DISASM ?= -x

build: env $(KERNEL_BIN)

env:
	(rustup target list | grep "$(TARGET) (installed)") || rustup target add $(TARGET)
	command -v rust-objcopy >/dev/null 2>&1 || cargo install cargo-binutils
	(rustup component list --installed | grep '^rust-src') >/dev/null 2>&1 || rustup component add rust-src
	(rustup component list --installed | grep '^llvm-tools-preview') >/dev/null 2>&1 || rustup component add llvm-tools-preview

kernel: env $(KERNEL_ELF)

$(KERNEL_ELF): FORCE
	@echo Platform: $(BOARD)
	@cp $(LINKER_SCRIPT) src/linker.ld
	@cargo build $(MODE_ARG) --target $(TARGET)
	@rm src/linker.ld

$(KERNEL_BIN): $(KERNEL_ELF)
	@$(OBJCOPY) $(KERNEL_ELF) --strip-all -O binary $@

run: kernel
	@$(MAKE) --no-print-directory run-inner PRIMARY_DISK="$(PRIMARY_DISK)" AUX_DISK="$(AUX_DISK)"

run-inner:
	@if [ -z "$(PRIMARY_DISK)" ]; then \
		echo "PRIMARY_DISK is required."; \
		exit 1; \
	fi
	@if [ ! -f "$(PRIMARY_DISK)" ]; then \
		echo "PRIMARY_DISK does not exist: $(PRIMARY_DISK)"; \
		exit 1; \
	fi
	@if [ -n "$(AUX_DISK)" ] && [ ! -f "$(AUX_DISK)" ]; then \
		echo "AUX_DISK does not exist: $(AUX_DISK)"; \
		exit 1; \
	fi
	@$(QEMU) $(QEMU_ARGS) $(EXTRA_QEMU_ARGS)

fdt:
	@$(QEMU) -machine virt,dumpdtb=virt.out -m 128m
	@fdtdump virt.out

debug: kernel
	@if [ ! -f "$(PRIMARY_DISK)" ]; then \
		echo "PRIMARY_DISK does not exist: $(PRIMARY_DISK)"; \
		exit 1; \
	fi
	@if [ -n "$(AUX_DISK)" ] && [ ! -f "$(AUX_DISK)" ]; then \
		echo "AUX_DISK does not exist: $(AUX_DISK)"; \
		exit 1; \
	fi
	@tmux new-session -d \
		"$(MAKE) --no-print-directory run-inner PRIMARY_DISK='$(PRIMARY_DISK)' AUX_DISK='$(AUX_DISK)' EXTRA_QEMU_ARGS='-s -S'" && \
		tmux split-window -h "$(GDB) -ex 'file $(KERNEL_ELF)' -ex 'set arch $(GDB_ARCH)' -ex 'target remote localhost:1234'" && \
		tmux -2 attach-session -d

gdbserver: kernel
	@if [ ! -f "$(PRIMARY_DISK)" ]; then \
		echo "PRIMARY_DISK does not exist: $(PRIMARY_DISK)"; \
		exit 1; \
	fi
	@if [ -n "$(AUX_DISK)" ] && [ ! -f "$(AUX_DISK)" ]; then \
		echo "AUX_DISK does not exist: $(AUX_DISK)"; \
		exit 1; \
	fi
	@$(MAKE) --no-print-directory run-inner PRIMARY_DISK="$(PRIMARY_DISK)" AUX_DISK="$(AUX_DISK)" EXTRA_QEMU_ARGS="-s -S"

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

FORCE:

.PHONY: build env kernel clean disasm disasm-vim run run-inner debug gdbserver gdbclient fdt FORCE
