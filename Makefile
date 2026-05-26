MODE ?= release
PERF_COUNTERS ?= 0
CARGO_HOME ?= $(CURDIR)/vendor
export CARGO_HOME

RISCV_TARGET := riscv64gc-unknown-none-elf
LOONGARCH_TARGET := loongarch64-unknown-none
KERNEL_RV_SRC := os/target/$(RISCV_TARGET)/$(MODE)/os
KERNEL_LA_SRC := os/target/$(LOONGARCH_TARGET)/$(MODE)/os

TEST_DISK ?= $(CURDIR)/sdcard-rv.img
TEST_DISK_LA ?= $(CURDIR)/sdcard-la.img
CONTEST_SCRIPT_DISK ?= $(CURDIR)/disk.img
CONTEST_SCRIPT_DISK_SIZE ?= 64M

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
	@CONTEST_SCRIPT_DISK="$(CONTEST_SCRIPT_DISK)" CONTEST_SCRIPT_DISK_SIZE="$(CONTEST_SCRIPT_DISK_SIZE)" ./scripts/build_contest_disk.sh

run-rv: kernel-rv contest-disk
	@$(MAKE) --no-print-directory -C os ARCH=riscv64 MODE=$(MODE) PERF_COUNTERS=$(PERF_COUNTERS) run-inner PRIMARY_DISK="$(TEST_DISK)" AUX_DISK="$(CONTEST_SCRIPT_DISK)"

run-la: kernel-la contest-disk
	@$(MAKE) --no-print-directory -C os ARCH=loongarch64 MODE=$(MODE) PERF_COUNTERS=$(PERF_COUNTERS) run-inner PRIMARY_DISK="$(TEST_DISK_LA)" AUX_DISK="$(CONTEST_SCRIPT_DISK)"
fmt:
	@$(MAKE) --no-print-directory -C os fmt
	@cd vendor/lwext4_rust && cargo fmt

clean:
	@$(MAKE) --no-print-directory -C os clean
	@rm -f kernel-rv kernel-la disk.img disk-la.img

.PHONY: all validation validate kernel-rv kernel-la contest-disk run-rv run-la fmt clean
