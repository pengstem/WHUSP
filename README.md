<div align="center">

<img src="assets/whu.png" alt="Wuhan University" width="420"/>

# WHUSP-OS

**For the 2026 National College Student Computer System Capability Contest · OS Track**

A modern monolithic kernel written in Rust, dual-targeting **RISC-V 64** and **LoongArch 64**

[![Rust](https://img.shields.io/badge/rust-nightly--2025--05--20-orange?logo=rust)](rust-toolchain.toml)
[![Arch](https://img.shields.io/badge/arch-RISC--V%2064%20%7C%20LoongArch%2064-blue)](#)
[![Kernel](https://img.shields.io/badge/kernel-monolithic-success)](#)
[![Status](https://img.shields.io/badge/status-WIP-yellow)](#-roadmap)
[![Contest](https://img.shields.io/badge/OS--Comp-2026-red)](#)

[中文](README.zh.md) ·
[Quick Start](#-quick-start) ·
[Architecture](#-architecture) ·
[Roadmap](#-roadmap) ·
[Team](#-team)

</div>

---

## 📖 Table of Contents

- [About](#-about)
- [Highlights](#-highlights)
- [Quick Start](#-quick-start)
- [Architecture](#-architecture)
- [Repository Layout](#-repository-layout)
- [Roadmap](#-roadmap)
- [Team](#-team)
- [Acknowledgements](#-acknowledgements)

---

## 🎯 About

**WHUSP-OS** is the kernel project from **Wuhan University**, team **WHU S**ystem **P**roject, for the 2026 National College Student Computer System Capability Contest — Operating System Design Track. Written in [Rust](https://www.rust-lang.org/), the goal is a POSIX-compatible monolithic kernel that boots on **RISC-V 64** and **LoongArch 64**, runs the official judge scripts, and exercises real userland (busybox, lua, libc-test, …).

> Status: **active development**. The kernel boots, mounts an EXT4 test image, and runs the basic statically-linked test suite under busybox. Bring-up of the dynamically-linked glibc path is in progress — see the [roadmap](#-roadmap).

---

## ✨ Highlights

| Subsystem | What's notable |
|---|---|
| 🏗️ **Dual-arch** | RISC-V 64 and LoongArch 64 share a common core; arch-specific code is confined to `os/src/arch/<arch>/` |
| 🧠 **Memory** | Buddy heap + frame allocator; per-process `MemorySet`; ELF loader handles `PT_LOAD` / `PT_INTERP` |
| 🗂️ **Filesystem** | Real EXT4 read/write via vendored `lwext4_rust`; unified VFS; `devfs` / `procfs` / `tmpfs` |
| ⚙️ **Tasks** | TCB / Thread split · `clone` / `execve` / signals · PID recycling · Mutex / Condvar / SleepMutex |
| 📡 **Drivers** | VirtIO Block / Input · UART · PLIC (RV) · IOCSR (LA) · DTB-driven device discovery |
| 🔌 **Syscalls** | Linux-generic ABI compatible dispatch table (`os/src/syscall.rs`), handlers split under `syscall/` |
| 🧰 **Build** | Fully offline & reproducible — every crates.io / git dependency mirrored in `vendor/crates/` |

---

## 🚀 Quick Start

### Prerequisites

- Rust toolchain `nightly-2025-05-20` (pinned via `rust-toolchain.toml`)
- `qemu-system-riscv64` ≥ 7.0.0; `qemu-system-loongarch64` ≥ 10.0.0
- The official contest container is recommended for parity:

```bash
docker run --rm -it \
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    -v "$PWD":/kernel -w /kernel --privileged \
    zhouzhouyi/os-contest:20260104 \
    bash -lc 'chmod o+x /root && exec setpriv --reuid "$HOST_UID" --regid "$HOST_GID" --clear-groups env HOME=/tmp RUSTUP_HOME=/root/.rustup PATH="$PATH" bash'
```

### Build

The repository root is the contest-style entry point. Both kernel ELFs land at the top level:

```bash
make all                  # build both kernel-rv and kernel-la (judge command)
make kernel-rv            # RISC-V 64 only
make kernel-la            # LoongArch 64 only
make fmt                  # format the workspace
make clean                # remove build artifacts
```

### Run in QEMU

You will need the official EXT4 test image distributed by the contest:

```bash
make run-rv  TEST_DISK=/path/to/contest-disk-rv.img
make run-la  TEST_DISK_LA=/path/to/contest-disk-la.img
```

On boot the kernel mounts the test image as `/`, launches `busybox sh` as init, and drives each `*_testcode.sh` group, emitting the contest-required markers:

```
#### OS COMP TEST GROUP START xxxxx ####
...
#### OS COMP TEST GROUP END   xxxxx ####
```

---

## 🧩 Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                        User Space                            │
│            busybox · lua · libc-test · *_testcode.sh         │
├──────────────────────────────────────────────────────────────┤
│              Syscall Dispatch (os/src/syscall.rs)            │
│       fs · process · signal · sync · memory · wait · …       │
├────────────┬───────────┬───────────┬───────────┬─────────────┤
│   task/    │   mm/     │   fs/     │  drivers/ │   sync/     │
│  TCB /     │ buddy +   │ VFS +     │ VirtIO·   │ UPIntr-     │
│  scheduler │ MemorySet │ EXT4 /    │ UART·     │ FreeCell ·  │
│  signals / │ ELF load  │ devfs /   │ PLIC /    │ SleepMutex  │
│  exec /    │           │ procfs /  │ IOCSR     │ · Condvar   │
│  clone     │           │ tmpfs     │           │             │
├────────────┴───────────┴───────────┴───────────┴─────────────┤
│      arch/{riscv64, loongarch64} —— entry · trap · MMU       │
│              · timer · context switch · board                │
├──────────────────────────────────────────────────────────────┤
│                RustSBI (RV) · direct entry (LA)              │
└──────────────────────────────────────────────────────────────┘
```

Boot order (`os/src/main.rs::rust_main`):

```
arch::init  →  mm::init  →  trap / timer  →  fs::mount(EXT4)  →  task::spawn(initproc)
   │              │              │                   │                      │
 board/UART/   buddy heap +    S-mode trap +       mount root            busybox sh
 SBI probe    frame allocator timer interrupts     filesystem (EXT4)    runs testcode
```

---

## 📁 Repository Layout

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

## 🗺️ Roadmap

### ✅ Done

- Dual-arch boot path: RISC-V and LoongArch each reach `rust_main` from their assembly entry
- Real EXT4 read/write via `lwext4_rust`, mounted as the root filesystem
- Virtual filesystems: `devfs` / `procfs` / `tmpfs`, with synthesized `/dev/null` `/dev/zero` `/dev/tty`
- Processes / threads / signals: `clone` · `execve` · signal delivery · PID recycling
- Memory: buddy heap, frame allocator, per-process `MemorySet`, ELF loader (with `PT_INTERP` redirection)
- Sync primitives: `UPIntrFreeCell` / `SleepMutex` / `Condvar`
- VirtIO Block / Input, UART, PLIC, IOCSR drivers
- Offline build: every crates.io / git dependency mirrored under `vendor/crates/`
- busybox + statically-linked test suite running end-to-end

### 🚧 In progress / Not yet

- **glibc dynamic-linker boot path**: implement `pread64` / `pwrite64` / `getrandom` / `madvise` / `mremap` / `getuid` / `getgid` / `rseq` and other early-startup syscalls
- `/dev/urandom` character device (so glibc's `getrandom` fallback works)
- Pass lua, libc-test and iozone in full
- Performance work: lmbench / unixbench / iperf paths
- Full validation on the LoongArch judging path
- Kernel backtrace / observability improvements

---

## 👥 Team

<div align="center">

**Wuhan University · Team WHUSP**

| Member | 成员 |
|--------|------|
| Peng Lingyu | 彭灵钰 |
| Shi Ruibo | 石瑞博 |

</div>

---

## 🙏 Acknowledgements

We only list projects whose **code we directly use** in this kernel:

- [**rCore-Tutorial-v3**](https://github.com/rcore-os/rCore-Tutorial-v3) — the original skeleton this kernel grew out of (boot flow, `UPIntrFreeCell`, address-space abstraction, …)
- [**lwext4**](https://github.com/gkostka/lwext4) — the C library that powers our EXT4 read/write path
- [**lwext4_rust**](https://github.com/elliott10/lwext4_rust) — Rust FFI wrapper around `lwext4`; `vendor/lwext4_rust/` carries a few local follow-up patches on top

---

<div align="center">

**Designed & Built at Wuhan University · 2026** ✨

</div>
