MODE ?= release
CARGO_HOME ?= $(CURDIR)/vendor
export CARGO_HOME

RISCV_TARGET := riscv64gc-unknown-none-elf
LOONGARCH_TARGET := loongarch64-unknown-none
KERNEL_RV_SRC := os/target/$(RISCV_TARGET)/$(MODE)/os
KERNEL_LA_SRC := os/target/$(LOONGARCH_TARGET)/$(MODE)/os

TEST_DISK ?= $(CURDIR)/sdcard-rv.img
TEST_DISK_LA ?= $(CURDIR)/sdcard-la.img
CONTEST_AUX_DISK ?= $(CURDIR)/disk.img
CONTEST_AUX_DISK_LA ?= $(CURDIR)/disk-la.img

all: kernel-rv kernel-la

kernel-rv:
	@$(MAKE) --no-print-directory -C os ARCH=riscv64 MODE=$(MODE) kernel
	@cp -f $(KERNEL_RV_SRC) kernel-rv

kernel-la:
	@$(MAKE) --no-print-directory -C os ARCH=loongarch64 MODE=$(MODE) kernel
	@cp -f $(KERNEL_LA_SRC) kernel-la

run-rv: kernel-rv
	@$(MAKE) --no-print-directory -C os ARCH=riscv64 MODE=$(MODE) run-inner PRIMARY_DISK="$(TEST_DISK)"

run-la: kernel-la
	@$(MAKE) --no-print-directory -C os ARCH=loongarch64 MODE=$(MODE) run-inner PRIMARY_DISK="$(TEST_DISK_LA)"
fmt:
	@$(MAKE) --no-print-directory -C os fmt
	@cd vendor/lwext4_rust && cargo fmt

clean:
	@$(MAKE) --no-print-directory -C os clean
	@rm -f kernel-rv kernel-la disk.img disk-la.img

.PHONY: all kernel-rv kernel-la run-rv run-la fmt clean
