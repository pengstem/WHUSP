ARCH ?= riscv64
MODE ?= release
TEST ?=

ifeq ($(ARCH),riscv64)
TARGET := riscv64gc-unknown-none-elf
KERNEL_SRC := os/target/$(TARGET)/$(MODE)/os
DISK_SRC := user/target/$(TARGET)/$(MODE)/fs.img
else
$(error Unsupported ARCH '$(ARCH)' in root Makefile. Only ARCH=riscv64 is wired today.)
endif

PRIMARY_DISK ?=$(CURDIR)/disk.img
TEST_DISK ?=$(CURDIR)/sdcard-rv.img
CONTEST_AUX_DISK ?=$(CURDIR)/disk.img
AUX_DISK ?=

all: kernel-rv disk.img

kernel-rv:
	@$(MAKE) --no-print-directory -C os ARCH=$(ARCH) MODE=$(MODE) kernel
	@cp $(KERNEL_SRC) kernel-rv

disk.img:
	@$(MAKE) --no-print-directory -C os ARCH=$(ARCH) MODE=$(MODE) TEST=$(TEST) fs-img
	@cp $(DISK_SRC) disk.img

run-rv: all
	@if [ -z "$(TEST_DISK)" ]; then \
		echo "TEST_DISK is required for contest-style boot. Example:"; \
		echo "  make run-rv TEST_DISK=$(CURDIR)/sdcard-rv.img"; \
		echo "For local development with generated disk.img on x0, use: make run-rv-dev"; \
		exit 1; \
	fi
	@$(MAKE) --no-print-directory -C os ARCH=$(ARCH) MODE=$(MODE) TEST=$(TEST) run PRIMARY_DISK="$(TEST_DISK)" AUX_DISK="$(CONTEST_AUX_DISK)"

run-rv-dev: all
	@$(MAKE) --no-print-directory -C os ARCH=$(ARCH) MODE=$(MODE) TEST=$(TEST) run PRIMARY_DISK="$(PRIMARY_DISK)" AUX_DISK="$(AUX_DISK)"

run-rv-contest: run-rv

fmt:
	@cd os && cargo fmt
	@cd user && cargo fmt
	@cd vendor/lwext4_rust && cargo fmt

clean:
	@$(MAKE) --no-print-directory -C os clean
	@$(MAKE) --no-print-directory -C user clean
	@rm -f kernel-rv disk.img

.PHONY: all kernel-rv disk.img run-rv run-rv-dev run-rv-contest fmt clean
