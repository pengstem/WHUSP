ARCH ?= riscv64
MODE ?= release
PERF_COUNTERS ?= 0
override CARGO_HOME := $(abspath ../vendor)
export CARGO_HOME

ifeq ($(ARCH),riscv64)
TARGET := riscv64gc-unknown-none-elf
DEFAULT_DISK := ../sdcard-rv.img
else ifeq ($(ARCH),loongarch64)
TARGET := loongarch64-unknown-none
DEFAULT_DISK := ../sdcard-la.img
else
$(error unsupported ARCH: $(ARCH))
endif

MODE_ARG := $(if $(filter release,$(MODE)),--release)
PERF_FEATURE_ARG := $(if $(filter 1 yes true on,$(PERF_COUNTERS)),--features perf-counters)
# Per-arch defaults when this Makefile is invoked directly; root targets pass MEM explicitly.
ifeq ($(ARCH),loongarch64)
MEM ?= 16G
else
MEM ?= 12G
endif
SMP ?= 8
# Must stay in sync with os/src/config.rs and both entry.asm boot stacks.
MAX_CPUS := 12
TARGET_DIR := $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)
KERNEL_ELF := $(TARGET_DIR)/$(TARGET)/$(MODE)/os
PRIMARY_DISK ?= $(DEFAULT_DISK)
AUX_DISK ?=
COMMA := ,
ifeq ($(ARCH),riscv64)
QEMU_SYSTEM := qemu-system-riscv64
QEMU_MACHINE_ARGS := -machine virt -bios default
PRIMARY_DEVICE_ARGS := -device virtio-blk-device$(COMMA)drive=x0$(COMMA)bus=virtio-mmio-bus.0
AUX_DISK_ARGS := $(if $(AUX_DISK),-drive file="$$aux_overlay"$(COMMA)if=none$(COMMA)format=qcow2$(COMMA)id=x1 -device virtio-blk-device$(COMMA)drive=x1$(COMMA)bus=virtio-mmio-bus.1)
QEMU_NETWORK_ARGS := -device virtio-net-device$(COMMA)netdev=net -netdev user$(COMMA)id=net
else ifeq ($(ARCH),loongarch64)
QEMU_SYSTEM := qemu-system-loongarch64
QEMU_MACHINE_ARGS :=
PRIMARY_DEVICE_ARGS := -device virtio-blk-pci$(COMMA)drive=x0
AUX_DISK_ARGS := $(if $(AUX_DISK),-drive file="$$aux_overlay"$(COMMA)if=none$(COMMA)format=qcow2$(COMMA)id=x1 -device virtio-blk-pci$(COMMA)drive=x1)
QEMU_NETWORK_ARGS := -device virtio-net-pci$(COMMA)netdev=net0 -netdev user$(COMMA)id=net0
endif

prepare-cargo-config:
	@rm -rf .cargo
	@ln -sfn dotcargo .cargo

kernel:
	@$(MAKE) --no-print-directory prepare-cargo-config
	@cargo build $(MODE_ARG) --target $(TARGET) $(PERF_FEATURE_ARG)

run: kernel
	@$(MAKE) --no-print-directory run-inner

check-smp:
	@case "$(SMP)" in ''|*[!0-9]*) echo "SMP must be an integer in 1..$(MAX_CPUS): $(SMP)"; exit 1;; esac
	@if [ "$(SMP)" -lt 1 ] || [ "$(SMP)" -gt "$(MAX_CPUS)" ]; then \
		echo "SMP must be in 1..$(MAX_CPUS): $(SMP)"; \
		exit 1; \
	fi

run-inner: check-smp
	@set -eu; \
	if [ ! -f "$(PRIMARY_DISK)" ]; then \
		echo "PRIMARY_DISK does not exist: $(PRIMARY_DISK)"; \
		exit 1; \
	fi; \
	if [ -n "$(AUX_DISK)" ] && [ ! -f "$(AUX_DISK)" ]; then \
		echo "AUX_DISK does not exist: $(AUX_DISK)"; \
		exit 1; \
	fi; \
	if ! command -v qemu-img >/dev/null 2>&1; then \
		echo "qemu-img is required to create temporary disk overlays"; \
		exit 1; \
	fi; \
	if [ ! -d /tmp ] || [ ! -w /tmp ]; then \
		echo "/tmp must be a writable directory"; \
		exit 1; \
	fi; \
	overlay_dir=$$(mktemp -d "/tmp/whusp-qemu.XXXXXX"); \
	primary_overlay="$$overlay_dir/x0.qcow2"; \
	aux_overlay=""; \
	cleanup() { \
		status=$$?; \
		trap - 0 1 2 15; \
		rm -f -- "$$primary_overlay"; \
		if [ -n "$$aux_overlay" ]; then rm -f -- "$$aux_overlay"; fi; \
		rmdir -- "$$overlay_dir"; \
		exit $$status; \
	}; \
	trap cleanup 0 1 2 15; \
	primary_base=$$(realpath -- "$(PRIMARY_DISK)"); \
	qemu-img create -q -f qcow2 -o "backing_file=$$primary_base,backing_fmt=raw" "$$primary_overlay"; \
	if [ -n "$(AUX_DISK)" ]; then \
		aux_overlay="$$overlay_dir/x1.qcow2"; \
		aux_base=$$(realpath -- "$(AUX_DISK)"); \
		qemu-img create -q -f qcow2 -o "backing_file=$$aux_base,backing_fmt=raw" "$$aux_overlay"; \
	fi; \
	echo "using temporary disk overlays in $$overlay_dir"; \
	$(QEMU_SYSTEM) $(QEMU_MACHINE_ARGS) -kernel "$(KERNEL_ELF)" -m "$(MEM)" -nographic -smp "$(SMP)" \
		-drive file="$$primary_overlay",if=none,format=qcow2,id=x0 \
		$(PRIMARY_DEVICE_ARGS) \
		$(AUX_DISK_ARGS) \
		-no-reboot \
		$(QEMU_NETWORK_ARGS) \
		-rtc base=utc

fmt: prepare-cargo-config
	@cargo fmt

clean:
	@rm -rf .cargo
	@cargo clean
	@rm -f src/linker.ld

.PHONY: prepare-cargo-config kernel run check-smp run-inner fmt clean
