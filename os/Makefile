ARCH ?= riscv64
MODE ?= release
PERF_COUNTERS ?= 0
CARGO_HOME ?= $(abspath ../vendor)
export CARGO_HOME
CARGO_CONFIG_DIR ?= dotcargo

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
MEM ?= 8G
SMP ?= 8
MAX_CPUS := 8
TARGET_DIR := $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)
KERNEL_ELF := $(TARGET_DIR)/$(TARGET)/$(MODE)/os
PRIMARY_DISK ?= $(DEFAULT_DISK)
AUX_DISK ?=
COMMA := ,
ifeq ($(ARCH),riscv64)
AUX_DISK_ARGS := $(if $(AUX_DISK),-drive file=$(AUX_DISK)$(COMMA)if=none$(COMMA)format=raw$(COMMA)id=x1 -device virtio-blk-device$(COMMA)drive=x1$(COMMA)bus=virtio-mmio-bus.1)
else ifeq ($(ARCH),loongarch64)
AUX_DISK_ARGS := $(if $(AUX_DISK),-drive file=$(AUX_DISK)$(COMMA)if=none$(COMMA)format=raw$(COMMA)id=x1 -device virtio-blk-pci$(COMMA)drive=x1)
endif

prepare-cargo-config:
	@rm -rf .cargo
	@ln -sfn $(CARGO_CONFIG_DIR) .cargo

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
	@if [ ! -f "$(PRIMARY_DISK)" ]; then \
		echo "PRIMARY_DISK does not exist: $(PRIMARY_DISK)"; \
		exit 1; \
	fi
	@if [ -n "$(AUX_DISK)" ] && [ ! -f "$(AUX_DISK)" ]; then \
		echo "AUX_DISK does not exist: $(AUX_DISK)"; \
		exit 1; \
	fi
ifeq ($(ARCH),riscv64)
	@qemu-system-riscv64 -machine virt -kernel $(KERNEL_ELF) -m $(MEM) -nographic -smp $(SMP) -bios default \
		-drive file=$(PRIMARY_DISK),if=none,format=raw,id=x0 \
		-device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
		$(AUX_DISK_ARGS) \
		-no-reboot \
		-device virtio-net-device,netdev=net -netdev user,id=net \
		-rtc base=utc
else ifeq ($(ARCH),loongarch64)
	@qemu-system-loongarch64 -kernel $(KERNEL_ELF) -m $(MEM) -nographic -smp $(SMP) \
		-drive file=$(PRIMARY_DISK),if=none,format=raw,id=x0 \
		-device virtio-blk-pci,drive=x0 \
		$(AUX_DISK_ARGS) \
		-no-reboot \
		-device virtio-net-pci,netdev=net0 \
		-netdev user,id=net0 \
		-rtc base=utc 
endif

fmt: prepare-cargo-config
	@cargo fmt

clean:
	@rm -rf .cargo
	@cargo clean
	@rm -f src/linker.ld

.PHONY: prepare-cargo-config kernel run check-smp run-inner fmt clean
