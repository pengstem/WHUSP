<div align="center">

<img src="assets/whu.png" alt="Wuhan University" width="420"/>

# WHUSP

A modern monolithic kernel written in Rust, dual-targeting **RISC-V 64** and **LoongArch 64**

[![Rust](https://img.shields.io/badge/rust-nightly--2025--05--20-orange?logo=rust)](rust-toolchain.toml)
[![Arch](https://img.shields.io/badge/arch-RISC--V%2064%20%7C%20LoongArch%2064-blue)](#)
[![Kernel](https://img.shields.io/badge/kernel-monolithic-success)](#)
[![Status](https://img.shields.io/badge/status-WIP-yellow)](#-roadmap)
[![Contest](https://img.shields.io/badge/OS--Comp-2026-red)](#)

[中文](README.zh.md) ·
[Quick Start](#-quick-start) ·
[Materials](#-materials) ·
[Architecture](#-architecture) ·
[Roadmap](#-roadmap) ·
[Team](#-team)

</div>

---

## 📖 Table of Contents

- [Quick Start](#-quick-start)
- [Repository Layout](#-repository-layout)
- [Materials](#-materials)
- [Team](#-team)

---

## Repository Layout

```
oskernel2026-whusp/
├── Makefile                  # contest-style root entry: make all / run-rv / run-la
├── os/                       # kernel crate
│   └── src/
│       ├── arch/             # arch-specific code converges here
│       │   ├── riscv64/      #   ↳ entry · trap · signal · switch · board
│       │   └── loongarch64/  #   ↳ entry · trap · signal · switch · board
│       ├── mm/               # heap, frames, page tables, MemorySet, ELF loader
│       ├── task/             # processes / threads / scheduler / signals / exec / clone / initproc
│       ├── fs/               # VFS, EXT4, devfs, procfs, tmpfs
│       ├── drivers/          # VirtIO, UART, PLIC / IOCSR
│       ├── sync/             # UPIntrFreeCell, Mutex, Condvar, SleepMutex
│       ├── syscall.rs        # dispatch table (a flat file, not a module)
│       └── syscall/          # per-subsystem handler implementations
├── vendor/
│   ├── crates/               # vendored crates.io / git deps for offline builds
│   ├── config.toml           # redirects crates-io & the riscv git source
│   └── lwext4_rust/          # path dependency: Rust FFI wrapper around lwext4
├── docs/                     # team-facing documentation
├── assets/                   # logos & badges
└── user/                     # legacy userland experiments (not in the default build)
```

---

## Materials

| Type | Link |
|------|------|
| Video | [Preliminary round video](初赛视频.mkv) |
| PDF | [Preliminary round report](初赛文档.pdf) |
| PPT | [Preliminary round slides](WHUSP%20初赛汇报.pptx) |

---

## 🚀 Quick Start

### Prerequisites

- **Rust** `nightly-2025-05-20` (see [`rust-toolchain.toml`](rust-toolchain.toml)) with:
  - Components: `rust-src`, `llvm-tools`, `rustfmt`, `clippy`
  - Targets: `riscv64gc-unknown-none-elf`, `loongarch64-unknown-none`
- **QEMU** ≥ 10.0.2 with `qemu-system-riscv64` and `qemu-system-loongarch64`
- **Python 3** and **`mkfs.ext4`** (for building the test script disk)
- **Test disk images** — download from [oscomp/testsuits-for-oskernel releases](https://github.com/oscomp/testsuits-for-oskernel/releases):
  - `sdcard-rv.img` (RISC-V, ~4 GiB)
  - `sdcard-la.img` (LoongArch, ~4 GiB)
- *(Optional)* **Docker** image [`zhouzhouyi/os-contest:20260104`](https://hub.docker.com/r/zhouzhouyi/os-contest) for the official contest environment:
  ```bash
  docker run -it --rm -v $(pwd):/code zhouzhouyi/os-contest:20260104 bash
  ```

### Build

```bash
make all          # Full submission build: format → contest disk → kernel-rv → kernel-la
make kernel-rv    # RISC-V kernel only
make kernel-la    # LoongArch kernel only
make clean        # Remove all build artifacts
```

Offline / vendored build (no network access):
```bash
CARGO_NET_OFFLINE=true make all
```

### Run

Boot the kernel in QEMU with the test disk attached:

```bash
make run-rv                          # Boot RISC-V (default: ./sdcard-rv.img)
make run-la                          # Boot LoongArch (default: ./sdcard-la.img)

# Override test disk or tune resources
make run-rv TEST_DISK=/path/to/sdcard-rv.img
make run-rv MEM=2G SMP=4
```

### Test Configuration

Tests are **not** compiled into the kernel. They live on a generated **script disk**
(`disk.img`) that is attached as a second block device (`x1`) and executed by the
kernel's init process at boot.

The script disk is built by two files under `scripts/`:

| Script | Role |
|--------|------|
| `scripts/export_contest_case_scripts.py` | **Central configuration** — defines which tests run, under which libc, with which LTP cases. Edit constants here, then rebuild. |
| `scripts/build_contest_disk.sh` | Wraps the Python exporter and creates the ext4 disk image. |

Rebuild the script disk after changing any configuration:

```bash
make contest-disk
```

#### Configuration Knobs

All knobs live in `scripts/export_contest_case_scripts.py` (and one companion file):

| Knob | Default | What it controls |
|------|---------|-----------------|
| `INTERACTIVE_SHELL` | `False` | `True` → drop into a BusyBox shell instead of running tests (debug mode) |
| `TEST_SCRIPTS` | all 11 groups | Which test groups to enable. Remove entries to skip suites. |
| `TEST_LIBCS` | `("/glibc", "/musl")` | Which libc roots to test. |
| `LTP_CASE_FILTER_OPTION` | `None` | Filter LTP cases at runtime. `None` = full whitelist; `"prefix:ioctl"` = only ioctl tests; `"case:fork07"` = single case; `"a"`–`"z"` = first-letter filter; `"range:start,end"` = lexicographic range. |
| [`scripts/ltp_whitelist.txt`](scripts/ltp_whitelist.txt) | ~800 cases | Curated LTP case list (one per line). Used when `LTP_CASE_FILTER_OPTION` is `None`. |

#### Common Workflows

**Debug a specific LTP case:**

1. Edit `scripts/export_contest_case_scripts.py`:
   ```python
   INTERACTIVE_SHELL = True
   LTP_CASE_FILTER_OPTION = "case:fork07"
   ```
2. Rebuild and run:
   ```bash
   make contest-disk && make run-rv
   ```

**Fast iteration — run only the basic test group:**

1. Edit `scripts/export_contest_case_scripts.py`:
   ```python
   TEST_SCRIPTS = ("basic_testcode.sh",)
   ```
2. Rebuild and run:
   ```bash
   make contest-disk && make run-rv
   ```

**Add a new LTP case to the whitelist:**

```bash
echo "new_case_name" >> scripts/ltp_whitelist.txt
make contest-disk
```

---

## Team

<div align="center">

**Wuhan University · Team WHUSP**

| Member | 成员 |
|--------|------|
| Peng Lingyu | 彭灵钰 |
| Shi Ruibo | 石瑞博 |

</div>

---

<div align="center">

**Designed & Built at Wuhan University · 2026** ✨

</div>
