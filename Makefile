ARCH ?= riscv64
MODE ?= release
TEST ?=
CARGO_HOME ?= $(CURDIR)/vendor
export CARGO_HOME

RISCV_TARGET := riscv64gc-unknown-none-elf
LOONGARCH_TARGET := loongarch64-unknown-none
KERNEL_RV_SRC := os/target/$(RISCV_TARGET)/$(MODE)/os
KERNEL_RV_STAMP := os/target/$(RISCV_TARGET)/$(MODE)/kernel-rv.stamp
KERNEL_LA_SRC := os/target/$(LOONGARCH_TARGET)/$(MODE)/os
KERNEL_LA_STAMP := os/target/$(LOONGARCH_TARGET)/$(MODE)/kernel-la.stamp
KERNEL_INPUTS := Makefile os/Cargo.toml $(wildcard os/Cargo.lock) os/.cargo/config.toml os/Makefile os/build.rs rust-toolchain.toml vendor/config.toml $(shell find os/src -type f ! -name linker.ld) $(shell find vendor/lwext4_rust -type f ! -path '*/target/*' ! -path '*/build_musl-generic/*')

TEST_DISK ?=$(CURDIR)/sdcard-rv.img
TEST_DISK_LA ?=$(CURDIR)/sdcard-la.img
CONTEST_AUX_DISK ?=
CONTEST_AUX_DISK_LA ?=$(wildcard $(CURDIR)/disk-la.img)
RUN_RV_AUX_ARG :=
ifneq ($(strip $(CONTEST_AUX_DISK)),)
RUN_RV_AUX_ARG := AUX_DISK="$(CONTEST_AUX_DISK)"
endif

all: kernel-rv kernel-la

$(KERNEL_RV_SRC) $(KERNEL_RV_STAMP) &: $(KERNEL_INPUTS)
	@$(MAKE) --no-print-directory -C os ARCH=riscv64 MODE=$(MODE) kernel
	@touch $(KERNEL_RV_STAMP)

kernel-rv: $(KERNEL_RV_SRC) $(KERNEL_RV_STAMP)
	@cp $(KERNEL_RV_SRC) kernel-rv

$(KERNEL_LA_SRC) $(KERNEL_LA_STAMP) &: $(KERNEL_INPUTS) $(shell find vendor/crates/loongArch64 -type f 2>/dev/null)
	@$(MAKE) --no-print-directory -C os ARCH=loongarch64 MODE=$(MODE) kernel
	@touch $(KERNEL_LA_STAMP)

kernel-la: $(KERNEL_LA_SRC) $(KERNEL_LA_STAMP)
	@cp $(KERNEL_LA_SRC) kernel-la

run-rv: kernel-rv
	@if [ -z "$(TEST_DISK)" ]; then \
		echo "TEST_DISK is required for contest-style boot. Example:"; \
		echo "  make run-rv TEST_DISK=$(CURDIR)/sdcard-rv.img"; \
		exit 1; \
	fi
	@$(MAKE) --no-print-directory -C os ARCH=riscv64 MODE=$(MODE) TEST=$(TEST) run-inner PRIMARY_DISK="$(TEST_DISK)" $(RUN_RV_AUX_ARG)

run-rv-contest: run-rv

run-la: kernel-la
	@if [ -z "$(TEST_DISK_LA)" ]; then \
		echo "TEST_DISK_LA is required for LoongArch contest-style boot. Example:"; \
		echo "  make run-la TEST_DISK_LA=$(CURDIR)/sdcard-la.img"; \
		exit 1; \
	fi
	@$(MAKE) --no-print-directory -C os ARCH=loongarch64 MODE=$(MODE) TEST=$(TEST) run-inner PRIMARY_DISK="$(TEST_DISK_LA)" AUX_DISK="$(CONTEST_AUX_DISK_LA)"

fmt:
	@cd os && cargo fmt
	@cd vendor/lwext4_rust && cargo fmt

clean:
	@$(MAKE) --no-print-directory -C os clean
	@rm -f kernel-rv kernel-la disk.img disk-la.img

.PHONY: all kernel-rv kernel-la run-rv run-la run-rv-contest fmt clean
