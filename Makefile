MODE ?= release
PERF_COUNTERS ?= 0
BLOCK_IO_MODE ?= force-sync
CARGO_DEFAULT_FEATURES ?= 1
EXTRA_FEATURES ?=

ifeq ($(BLOCK_IO_MODE),auto)
else ifeq ($(BLOCK_IO_MODE),force-sync)
else
$(error BLOCK_IO_MODE must be auto or force-sync: $(BLOCK_IO_MODE))
endif

# The published final-round profile is identical on both architectures.
# `MEM=...` on the command line still overrides both for targeted debugging.
MEM_RV ?= 8G
MEM_LA ?= 8G
SMP ?= 8
INTERACTIVE ?= 0
RUN_CAGENT ?= 1
RUN_BUILDSTORM ?= 1
STARFIVE_SAFE_BUILDSTORM ?= 0
NO_BUILD ?= 0
# Must stay in sync with os/src/config.rs and both entry.asm boot stacks.
MAX_CPUS := 12
override CARGO_HOME := $(CURDIR)/vendor
export CARGO_HOME

# Optional unified override: `make run-rv MEM=2G` / `make run-la MEM=2G`.
ifdef MEM
EFFECTIVE_MEM_RV := $(MEM)
EFFECTIVE_MEM_LA := $(MEM)
else
EFFECTIVE_MEM_RV := $(MEM_RV)
EFFECTIVE_MEM_LA := $(MEM_LA)
endif

LOONGARCH_TOOLCHAIN_BIN ?= $(CURDIR)/tools/loongarch64-linux-musl-cross/bin
ifneq ($(wildcard $(LOONGARCH_TOOLCHAIN_BIN)/loongarch64-linux-musl-gcc),)
export PATH := $(LOONGARCH_TOOLCHAIN_BIN):$(PATH)
endif

RISCV_TARGET := riscv64gc-unknown-none-elf
LOONGARCH_TARGET := loongarch64-unknown-none-softfloat
KERNEL_RV_SRC := os/target/$(RISCV_TARGET)/$(MODE)/os
KERNEL_LA_SRC := os/target/$(LOONGARCH_TARGET)/$(MODE)/os

TEST_DISK ?= $(CURDIR)/sdcard-rv-pub.img
TEST_DISK_LA ?= $(CURDIR)/sdcard-la-pub.img
CONTEST_SCRIPT_DISK ?= $(CURDIR)/disk.img
CONTEST_SCRIPT_DISK_SIZE ?= 64M
STARFIVE_TFTP_DIR ?= /tmp/whusp-starfive-tftp
STARFIVE_FIT_OUTPUT ?= $(STARFIVE_TFTP_DIR)/whusp-cagent.itb

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

final-preflight:
	@python3 scripts/finals_guard.py preflight

final-preflight-built:
	@python3 scripts/finals_guard.py preflight --require-kernels

final-guard-test:
	@python3 -m unittest discover -s scripts/tests -p 'test_*.py'

final-config:
	@printf 'FINAL_PROFILE smp=%s mem_rv=%s mem_la=%s perf_counters=%s block_io=%s cagent=%s buildstorm=%s\n' \
		"$(SMP)" "$(EFFECTIVE_MEM_RV)" "$(EFFECTIVE_MEM_LA)" \
		"$(PERF_COUNTERS)" "$(BLOCK_IO_MODE)" "$(RUN_CAGENT)" "$(RUN_BUILDSTORM)"

kernel-rv:
	@$(MAKE) --no-print-directory -C os ARCH=riscv64 MODE=$(MODE) PERF_COUNTERS=$(PERF_COUNTERS) BLOCK_IO_MODE=$(BLOCK_IO_MODE) CARGO_DEFAULT_FEATURES=$(CARGO_DEFAULT_FEATURES) EXTRA_FEATURES="$(EXTRA_FEATURES)" kernel
	@cp -f $(KERNEL_RV_SRC) kernel-rv

kernel-la:
	@$(MAKE) --no-print-directory -C os ARCH=loongarch64 MODE=$(MODE) PERF_COUNTERS=$(PERF_COUNTERS) BLOCK_IO_MODE=$(BLOCK_IO_MODE) CARGO_DEFAULT_FEATURES=$(CARGO_DEFAULT_FEATURES) EXTRA_FEATURES="$(EXTRA_FEATURES)" kernel
	@cp -f $(KERNEL_LA_SRC) kernel-la

contest-disk:
	@CONTEST_SCRIPT_DISK="$(CONTEST_SCRIPT_DISK)" CONTEST_SCRIPT_DISK_SIZE="$(CONTEST_SCRIPT_DISK_SIZE)" CONTEST_INTERACTIVE="$(INTERACTIVE)" CONTEST_RUN_CAGENT="$(RUN_CAGENT)" CONTEST_RUN_BUILDSTORM="$(RUN_BUILDSTORM)" CONTEST_STARFIVE_SAFE_BUILDSTORM="$(STARFIVE_SAFE_BUILDSTORM)" ./scripts/build_contest_disk.sh

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
	@$(MAKE) --no-print-directory -C os ARCH=riscv64 MODE=$(MODE) PERF_COUNTERS=$(PERF_COUNTERS) MEM=$(EFFECTIVE_MEM_RV) SMP=$(SMP) run-inner KERNEL_ELF="$(CURDIR)/kernel-rv" PRIMARY_DISK="$(TEST_DISK)" AUX_DISK="$(CONTEST_SCRIPT_DISK)"

run-la: check-smp $(RUN_LA_KERNEL_PREREQ) contest-disk
	@$(MAKE) --no-print-directory -C os ARCH=loongarch64 MODE=$(MODE) PERF_COUNTERS=$(PERF_COUNTERS) MEM=$(EFFECTIVE_MEM_LA) SMP=$(SMP) run-inner KERNEL_ELF="$(CURDIR)/kernel-la" PRIMARY_DISK="$(TEST_DISK_LA)" AUX_DISK="$(CONTEST_SCRIPT_DISK)"

shell-rv: INTERACTIVE=1
shell-rv: run-rv

shell-la: INTERACTIVE=1
shell-la: run-la

starfive-cagent-image: RUN_CAGENT=1
starfive-cagent-image: RUN_BUILDSTORM=0
starfive-cagent-image: INTERACTIVE=0
starfive-cagent-image: kernel-rv contest-disk
	@STARFIVE_KERNEL_ELF="$(CURDIR)/kernel-rv" STARFIVE_RUNNER_DISK="$(CONTEST_SCRIPT_DISK)" STARFIVE_FIT_OUTPUT="$(STARFIVE_FIT_OUTPUT)" ./scripts/build_starfive_fit.sh

starfive-shell-image: RUN_CAGENT=0
starfive-shell-image: RUN_BUILDSTORM=0
starfive-shell-image: INTERACTIVE=1
starfive-shell-image: kernel-rv contest-disk
	@STARFIVE_KERNEL_ELF="$(CURDIR)/kernel-rv" STARFIVE_RUNNER_DISK="$(CONTEST_SCRIPT_DISK)" STARFIVE_FIT_OUTPUT="$(STARFIVE_FIT_OUTPUT)" ./scripts/build_starfive_fit.sh

starfive-buildstorm-image: RUN_CAGENT=0
starfive-buildstorm-image: RUN_BUILDSTORM=1
starfive-buildstorm-image: STARFIVE_SAFE_BUILDSTORM=1
starfive-buildstorm-image: STARFIVE_FIT_OUTPUT=$(STARFIVE_TFTP_DIR)/whusp-buildstorm.itb
starfive-buildstorm-image: INTERACTIVE=0
starfive-buildstorm-image: kernel-rv contest-disk
	@STARFIVE_KERNEL_ELF="$(CURDIR)/kernel-rv" STARFIVE_RUNNER_DISK="$(CONTEST_SCRIPT_DISK)" STARFIVE_FIT_OUTPUT="$(STARFIVE_FIT_OUTPUT)" ./scripts/build_starfive_fit.sh

starfive-rust-smoke-image: kernel-rv
	@python3 scripts/build_starfive_rust_smoke.py --kernel "$(CURDIR)/kernel-rv" --perf-counters "$(PERF_COUNTERS)"

fmt:
	@$(MAKE) --no-print-directory -C os fmt
	@cd vendor/lwext4_rust && cargo fmt

clean:
	@$(MAKE) --no-print-directory -C os clean
	@rm -f kernel-rv kernel-la disk.img disk-la.img

.PHONY: all validation validate final-preflight final-preflight-built final-guard-test final-config kernel-rv kernel-la contest-disk check-kernel-rv check-kernel-la check-smp run-rv run-la shell-rv shell-la starfive-cagent-image starfive-shell-image starfive-buildstorm-image starfive-rust-smoke-image fmt clean
