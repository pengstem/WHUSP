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
make run-rv                          # Build and boot RISC-V
make run-la                          # Build and boot LoongArch

# Override test disk or tune resources
make run-rv TEST_DISK=/path/to/sdcard-rv.img
make run-rv MEM=2G SMP=4

# Reuse an existing root-level kernel artifact
make run-rv NO_BUILD=1

# Enter an interactive BusyBox shell instead of running tests
make shell-rv
make shell-rv NO_BUILD=1             # Also skip the kernel build
```

### Test Configuration

Tests are **not** compiled into the kernel. They live on a generated **script disk**
(`disk.img`) that is attached as a second block device (`x1`) and executed by the
kernel's init process at boot.

The script disk is built by two files under `scripts/`:

| Script | Role |
|--------|------|
| `scripts/export_contest_case_scripts.py` | Generates the final-round CAgent/BuildStorm runner or an interactive shell entry point. |
| `scripts/build_contest_disk.sh` | Wraps the Python exporter and creates the ext4 disk image. |

The normal run targets rebuild only this small script disk on every invocation.
The kernel is rebuilt unless `NO_BUILD=1` is supplied.

```bash
make contest-disk                       # Normal final-round runner
make contest-disk INTERACTIVE=1         # Interactive shell runner
```

#### Configuration Knobs

| Knob | Default | What it controls |
|------|---------|-----------------|
| `INTERACTIVE` | `0` | `1` creates a script disk that enters a BusyBox shell instead of running tests. |
| `NO_BUILD` | `0` | `1` reuses `./kernel-rv` or `./kernel-la`; the command fails clearly if the requested artifact is absent. |
| `RUN_CAGENT` | `True` | Enables the final-round CAgent test in the exporter. |
| `RUN_BUILDSTORM` | `False` | Enables the final-round BuildStorm test in the exporter. |

#### Common Workflows

```bash
# Change only the guest-side task and reuse the last compiled kernel
make run-rv NO_BUILD=1

# Open a shell with that same kernel
make shell-rv NO_BUILD=1

# The scorer supports the same fast path
python3 tools/score_autotest.py --arch rv --no-build
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
