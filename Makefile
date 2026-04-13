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

TEST_DISK ?=$(CURDIR)/disk.img
AUX_DISK ?=

all: kernel-rv disk.img

kernel-rv:
	@$(MAKE) --no-print-directory -C os ARCH=$(ARCH) MODE=$(MODE) kernel
	@cp $(KERNEL_SRC) kernel-rv

disk.img:
	@$(MAKE) --no-print-directory -C os ARCH=$(ARCH) MODE=$(MODE) TEST=$(TEST) fs-img
	@cp $(DISK_SRC) disk.img

run-rv: all
	@$(MAKE) --no-print-directory -C os ARCH=$(ARCH) MODE=$(MODE) TEST=$(TEST) run TEST_DISK="$(TEST_DISK)" AUX_DISK="$(AUX_DISK)"

run-rv-contest: run-rv

fmt:
	@cd os && cargo fmt
	@cd user && cargo fmt

clean:
	@$(MAKE) --no-print-directory -C os clean
	@$(MAKE) --no-print-directory -C user clean
	@rm -f kernel-rv disk.img

.PHONY: all kernel-rv disk.img run-rv run-rv-contest fmt clean
