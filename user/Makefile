ARCH ?= riscv64
MODE := release
CARGO_HOME ?= $(abspath ../vendor)
export CARGO_HOME

ifeq ($(ARCH),riscv64)
TARGET := riscv64gc-unknown-none-elf
OBJDUMP := rust-objdump --arch-name=riscv64
OBJCOPY := rust-objcopy --binary-architecture=riscv64
else ifeq ($(ARCH),loongarch64)
$(error LoongArch user programs are not implemented yet: user syscall wrappers and ABI setup belong to P5 stage 5)
else
$(error Unsupported ARCH '$(ARCH)' in user/Makefile. Supported values are riscv64 and loongarch64)
endif

APP_DIR := src/bin
TARGET_DIR := target/$(TARGET)/$(MODE)
APP_NAMES := \
	brk_smoke \
	cat \
	cmdline_args \
	count_lines \
	exit \
	fantastic_text \
	filetest_simple \
	forktest \
	forktest2 \
	forktest_simple \
	forktree \
	hello_world \
	huge_write \
	infloop \
	initproc \
	matrix \
	pipe_large_test \
	pipetest \
	priv_csr \
	priv_inst \
	random_num \
	run_pipe_test \
	sleep \
	sleep_simple \
	stack_overflow \
	stackful_coroutine \
	stackless_coroutine \
	store_fault \
	until_timeout \
	user_shell \
	usertests \
	waitid_smoke \
	yield
APPS := $(addprefix $(APP_DIR)/,$(addsuffix .rs,$(APP_NAMES)))
ELFS := $(addprefix $(TARGET_DIR)/,$(APP_NAMES))
BINS := $(addsuffix .bin,$(ELFS))

CP := cp 

TEST ?= 

elf: $(APPS)
	@cargo build --release
ifeq ($(TEST), 1)
	@$(CP) $(TARGET_DIR)/usertests $(TARGET_DIR)/initproc
endif

binary: elf
	@$(foreach elf, $(ELFS), $(OBJCOPY) $(elf) --strip-all -O binary $(patsubst $(TARGET_DIR)/%, $(TARGET_DIR)/%.bin, $(elf));)

build: binary

clean:
	@cargo clean

.PHONY: elf binary build clean
