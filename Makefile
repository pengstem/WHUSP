MODE ?= release
PERF_COUNTERS ?= 0
MEM ?= 8G
SMP ?= 8
INTERACTIVE ?= 0
NO_BUILD ?= 0
MAX_CPUS := 8
CARGO_HOME ?= $(CURDIR)/vendor
export CARGO_HOME

# Keep local development usable when the contest Docker image is unavailable.
# The extracted GCC 13.2 LoongArch toolchain is intentionally ignored with the
# other large tools, and the official image still wins when this directory is
# absent.
LOONGARCH_TOOLCHAIN_BIN ?= $(CURDIR)/tools/loongarch64-linux-musl-cross/bin
ifneq ($(wildcard $(LOONGARCH_TOOLCHAIN_BIN)/loongarch64-linux-musl-gcc),)
export PATH := $(LOONGARCH_TOOLCHAIN_BIN):$(PATH)
endif

RISCV_TARGET := riscv64gc-unknown-none-elf
LOONGARCH_TARGET := loongarch64-unknown-none
KERNEL_RV_SRC := os/target/$(RISCV_TARGET)/$(MODE)/os
KERNEL_LA_SRC := os/target/$(LOONGARCH_TARGET)/$(MODE)/os

TEST_DISK ?= $(CURDIR)/sdcard-rv-pub.img
TEST_DISK_LA ?= $(CURDIR)/sdcard-la-pub.img
CONTEST_SCRIPT_DISK ?= $(CURDIR)/disk.img
CONTEST_SCRIPT_DISK_SIZE ?= 64M

ifneq ($(filter 1 yes true on,$(NO_BUILD)),)
RUN_RV_KERNEL_PREREQ := check-kernel-rv
RUN_LA_KERNEL_PREREQ := check-kernel-la
else
RUN_RV_KERNEL_PREREQ := kernel-rv
RUN_LA_KERNEL_PREREQ := kernel-la
endif

all: validation

validation:
	@$(MAKE) --no-print-directory fmt
	@$(MAKE) --no-print-directory contest-disk
	@$(MAKE) --no-print-directory kernel-rv
	@$(MAKE) --no-print-directory kernel-la

validate: validation

kernel-rv:
	@$(MAKE) --no-print-directory -C os ARCH=riscv64 MODE=$(MODE) PERF_COUNTERS=$(PERF_COUNTERS) kernel
	@cp -f $(KERNEL_RV_SRC) kernel-rv

kernel-la:
	@$(MAKE) --no-print-directory -C os ARCH=loongarch64 MODE=$(MODE) PERF_COUNTERS=$(PERF_COUNTERS) kernel
	@cp -f $(KERNEL_LA_SRC) kernel-la

contest-disk:
	@CONTEST_SCRIPT_DISK="$(CONTEST_SCRIPT_DISK)" CONTEST_SCRIPT_DISK_SIZE="$(CONTEST_SCRIPT_DISK_SIZE)" CONTEST_INTERACTIVE="$(INTERACTIVE)" ./scripts/build_contest_disk.sh

check-kernel-rv:
	@if [ ! -f "$(CURDIR)/kernel-rv" ]; then \
		echo "kernel-rv does not exist; run 'make kernel-rv' or omit NO_BUILD=1"; \
		exit 1; \
	fi
	@echo "using existing kernel-rv (NO_BUILD=$(NO_BUILD))"

check-kernel-la:
	@if [ ! -f "$(CURDIR)/kernel-la" ]; then \
		echo "kernel-la does not exist; run 'make kernel-la' or omit NO_BUILD=1"; \
		exit 1; \
	fi
	@echo "using existing kernel-la (NO_BUILD=$(NO_BUILD))"

check-smp:
	@case "$(SMP)" in ''|*[!0-9]*) echo "SMP must be an integer in 1..$(MAX_CPUS): $(SMP)"; exit 1;; esac
	@if [ "$(SMP)" -lt 1 ] || [ "$(SMP)" -gt "$(MAX_CPUS)" ]; then \
		echo "SMP must be in 1..$(MAX_CPUS): $(SMP)"; \
		exit 1; \
	fi

run-rv: check-smp $(RUN_RV_KERNEL_PREREQ) contest-disk
	@$(MAKE) --no-print-directory -C os ARCH=riscv64 MODE=$(MODE) PERF_COUNTERS=$(PERF_COUNTERS) MEM=$(MEM) SMP=$(SMP) run-inner KERNEL_ELF="$(CURDIR)/kernel-rv" PRIMARY_DISK="$(TEST_DISK)" AUX_DISK="$(CONTEST_SCRIPT_DISK)"

run-la: check-smp $(RUN_LA_KERNEL_PREREQ) contest-disk
	@$(MAKE) --no-print-directory -C os ARCH=loongarch64 MODE=$(MODE) PERF_COUNTERS=$(PERF_COUNTERS) MEM=$(MEM) SMP=$(SMP) run-inner KERNEL_ELF="$(CURDIR)/kernel-la" PRIMARY_DISK="$(TEST_DISK_LA)" AUX_DISK="$(CONTEST_SCRIPT_DISK)"

shell-rv: INTERACTIVE=1
shell-rv: run-rv

shell-la: INTERACTIVE=1
shell-la: run-la

fmt:
	@$(MAKE) --no-print-directory -C os fmt
	@cd vendor/lwext4_rust && cargo fmt

clean:
	@$(MAKE) --no-print-directory -C os clean
	@rm -f kernel-rv kernel-la disk.img disk-la.img

.PHONY: all validation validate kernel-rv kernel-la contest-disk check-kernel-rv check-kernel-la check-smp run-rv run-la shell-rv shell-la fmt clean
