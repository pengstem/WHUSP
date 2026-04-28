ARCH ?= riscv64
MODE ?= release
TEST ?=
CARGO_HOME ?= $(CURDIR)/vendor
export CARGO_HOME

ifeq ($(ARCH),riscv64)
TARGET := riscv64gc-unknown-none-elf
KERNEL_SRC := os/target/$(TARGET)/$(MODE)/os
KERNEL_STAMP := os/target/$(TARGET)/$(MODE)/kernel-rv.stamp
DISK_SRC := user/target/$(TARGET)/$(MODE)/fs.img
DISK_STAMP := user/target/$(TARGET)/$(MODE)/fs-img.$(if $(strip $(TEST)),$(TEST),default).stamp
 KERNEL_INPUTS := Makefile os/Cargo.toml $(wildcard os/Cargo.lock) os/Makefile os/build.rs rust-toolchain.toml vendor/config.toml $(shell find os/src -type f ! -name linker.ld) $(shell find vendor/lwext4_rust -type f ! -path '*/target/*' ! -path '*/build_musl-generic/*')
 USER_INPUTS := user/Cargo.toml user/Makefile vendor/config.toml $(wildcard user/Cargo.lock) $(shell find user/src -type f)
else
$(error Unsupported ARCH '$(ARCH)' in root Makefile. Only ARCH=riscv64 is wired today.)
endif

PRIMARY_DISK ?=$(CURDIR)/disk.img
TEST_DISK ?=$(CURDIR)/sdcard-rv.img
CONTEST_AUX_DISK ?=$(CURDIR)/disk.img
AUX_DISK ?=

all: kernel-rv disk.img

$(KERNEL_SRC) $(KERNEL_STAMP) &: $(KERNEL_INPUTS)
	@$(MAKE) --no-print-directory -C os ARCH=$(ARCH) MODE=$(MODE) kernel
	@touch $(KERNEL_STAMP)

kernel-rv: $(KERNEL_SRC) $(KERNEL_STAMP)
	@cp $(KERNEL_SRC) kernel-rv

$(DISK_SRC) $(DISK_STAMP) &: $(USER_INPUTS) Makefile os/Makefile
	@$(MAKE) --no-print-directory -C os ARCH=$(ARCH) MODE=$(MODE) TEST=$(TEST) fs-img
	@touch $(DISK_STAMP)

disk.img: $(DISK_SRC) $(DISK_STAMP)
	@cp $(DISK_SRC) disk.img

run-rv: all
	@if [ -z "$(TEST_DISK)" ]; then \
		echo "TEST_DISK is required for contest-style boot. Example:"; \
		echo "  make run-rv TEST_DISK=$(CURDIR)/sdcard-rv.img"; \
		echo "For local development with generated disk.img on x0, use: make run-rv-dev"; \
		exit 1; \
	fi
	@$(MAKE) --no-print-directory -C os ARCH=$(ARCH) MODE=$(MODE) TEST=$(TEST) run-inner PRIMARY_DISK="$(TEST_DISK)" AUX_DISK="$(CONTEST_AUX_DISK)"

run-rv-contest: run-rv

fmt:
	@cd os && cargo fmt
	@cd user && cargo fmt
	@cd vendor/lwext4_rust && cargo fmt

clean:
	@$(MAKE) --no-print-directory -C os clean
	@$(MAKE) --no-print-directory -C user clean
	@rm -f kernel-rv disk.img

.PHONY: all run-rv run-rv-dev run-rv-contest fmt clean
