<div align="center">

<img src="assets/whu.png" alt="Wuhan University" width="420"/>

# WHUSP-OS

**面向 2026 全国大学生计算机系统能力大赛 · 操作系统设计赛**

一个用 Rust 编写的、双架构（RISC-V 64 / LoongArch 64）的现代宏内核

[![Rust](https://img.shields.io/badge/rust-nightly--2025--05--20-orange?logo=rust)](rust-toolchain.toml)
[![Arch](https://img.shields.io/badge/arch-RISC--V%2064%20%7C%20LoongArch%2064-blue)](#)
[![Kernel](https://img.shields.io/badge/kernel-monolithic-success)](#)
[![Status](https://img.shields.io/badge/status-WIP-yellow)](#-路线图)
[![Contest](https://img.shields.io/badge/OS--Comp-2026-red)](#)

[English](README.md) ·
[快速开始](#-快速开始) ·
[内核架构](#-内核架构) ·
[路线图](#-路线图) ·
[团队](#-团队)

</div>

---

## 📖 目录

- [项目简介](#-项目简介)
- [核心特性](#-核心特性)
- [快速开始](#-快速开始)
- [内核架构](#-内核架构)
- [项目结构](#-项目结构)
- [路线图](#-路线图)
- [团队](#-团队)
- [致谢](#-致谢)

---

## 🎯 项目简介

**WHUSP-OS** 是武汉大学参赛队伍（**WHU S**ystem **P**roject）为 2026 年全国大学生计算机系统能力大赛 ——「操作系统设计赛」打造的内核作品。项目使用 [Rust](https://www.rust-lang.org/) 语言构建，目标是在 **RISC-V 64** 与 **LoongArch 64** 两套指令集上提供一个兼容 POSIX、可运行评测脚本与真实用户态程序（busybox / lua / libc-test 等）的宏内核。

> 当前阶段：**积极开发中**。已能在 QEMU 中启动 busybox、挂载 EXT4 测试盘、跑通基础静态测试集；动态链接的 glibc 启动路径仍在补齐中。详见 [路线图](#-路线图)。

---

## ✨ 核心特性

| 模块 | 实现亮点 |
|---|---|
| 🏗️ **双架构** | RISC-V 64 与 LoongArch 64 共享主干内核，差异收敛于 `os/src/arch/<arch>/` |
| 🧠 **内存管理** | 伙伴堆 + 物理帧分配器；按进程 `MemorySet`；ELF Loader 支持 `PT_LOAD` / `PT_INTERP` |
| 🗂️ **文件系统** | EXT4 真磁盘读写（基于 vendored `lwext4_rust`） · 统一 VFS 抽象 · `devfs` / `procfs` / `tmpfs` |
| ⚙️ **进程线程** | TCB / Thread 分离 · `clone` / `execve` / 信号 · PID 回收 · 同步原语（Mutex / Condvar / SleepMutex）|
| 📡 **驱动** | VirtIO Block / Input · UART · PLIC（RV）· IOCSR（LA）· 设备发现基于 DTB |
| 🔌 **系统调用** | Linux Generic ABI 兼容的分发表（`os/src/syscall.rs`），按子系统拆分到 `syscall/` |
| 🧰 **构建链** | 离线、可复现 —— `vendor/crates/` 镜像所有 crates.io 与 git 依赖，零网络构建 |

---

## 🚀 快速开始

### 环境要求

- Rust 工具链：`nightly-2025-05-20`（仓库已通过 `rust-toolchain.toml` 钉版本）
- `qemu-system-riscv64` ≥ 7.0.0；`qemu-system-loongarch64` ≥ 10.0.0
- 推荐使用大赛官方容器，确保环境一致：

```bash
docker run --rm -it \
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    -v "$PWD":/kernel -w /kernel --privileged \
    zhouzhouyi/os-contest:20260104 \
    bash -lc 'chmod o+x /root && exec setpriv --reuid "$HOST_UID" --regid "$HOST_GID" --clear-groups env HOME=/tmp RUSTUP_HOME=/root/.rustup PATH="$PATH" bash'
```

### 构建

仓库根目录是大赛风格入口，直接产出 `kernel-rv` / `kernel-la` 两个内核 ELF：

```bash
make all                  # 同时构建 RISC-V 与 LoongArch（评测命令）
make kernel-rv            # 仅 RISC-V 64
make kernel-la            # 仅 LoongArch 64
make fmt                  # 一键格式化
make clean                # 清理产物
```

### 在 QEMU 中运行

需要先准备好大赛官方提供的 EXT4 测试盘镜像：

```bash
make run-rv  TEST_DISK=/path/to/contest-disk-rv.img
make run-la  TEST_DISK_LA=/path/to/contest-disk-la.img
```

启动后内核会挂载测试盘到根目录，以 `busybox sh` 为 init，执行 `*_testcode.sh` 测试组，按规约打印：

```
#### OS COMP TEST GROUP START xxxxx ####
...
#### OS COMP TEST GROUP END   xxxxx ####
```

---

## 🧩 内核架构

```
┌──────────────────────────────────────────────────────────────┐
│                        User Space                            │
│            busybox · lua · libc-test · *_testcode.sh         │
├──────────────────────────────────────────────────────────────┤
│              Syscall Dispatch (os/src/syscall.rs)            │
│       fs · process · signal · sync · memory · wait · …       │
├────────────┬───────────┬───────────┬───────────┬─────────────┤
│   task/    │   mm/     │   fs/     │  drivers/ │   sync/     │
│ TCB / 调度 │ 伙伴堆 +  │ VFS +     │ VirtIO·   │ UPIntr-     │
│ signal /   │ MemorySet │ EXT4 /    │ UART·     │ FreeCell ·  │
│ exec /     │ ELF Load  │ devfs /   │ PLIC /    │ SleepMutex  │
│ clone      │           │ procfs /  │ IOCSR     │ · Condvar   │
│            │           │ tmpfs     │           │             │
├────────────┴───────────┴───────────┴───────────┴─────────────┤
│      arch/{riscv64, loongarch64} —— entry · trap · MMU       │
│              · timer · context switch · board                │
├──────────────────────────────────────────────────────────────┤
│                  RustSBI (RV) · 直跳入口 (LA)                │
└──────────────────────────────────────────────────────────────┘
```

启动序列（`os/src/main.rs::rust_main`）：

```
arch::init  →  mm::init  →  trap / timer  →  fs::mount(EXT4)  →  task::spawn(initproc)
   │              │              │                  │                       │
 板载/UART/    伙伴堆 +        S 态陷阱             挂载根文件系统          busybox sh
 SBI 探测     物理帧分配      时钟中断接管           （EXT4，只读+写）     执行 testcode
```

---

## 📁 项目结构

```
oskernel2026-whusp/
├── Makefile                  # 大赛风格根入口：make all / run-rv / run-la
├── os/                       # 内核 crate
│   └── src/
│       ├── arch/             # 架构差异收敛点
│       │   ├── riscv64/      #   ↳ entry · trap · signal · switch · board
│       │   └── loongarch64/  #   ↳ entry · trap · signal · switch · board
│       ├── mm/               # 内存：堆、帧、页表、MemorySet、ELF Loader
│       ├── task/             # 进程/线程、调度、信号、clone、exec、initproc
│       ├── fs/               # VFS、EXT4、devfs、procfs、tmpfs
│       ├── drivers/          # VirtIO、UART、PLIC / IOCSR
│       ├── sync/             # UPIntrFreeCell、Mutex、Condvar、SleepMutex
│       ├── syscall.rs        # 分发表（注意：是文件，不是模块）
│       └── syscall/          # 各子系统的 handler 实现
├── vendor/
│   ├── crates/               # 镜像的 crates.io / git 依赖（离线构建）
│   ├── config.toml           # 重定向 crates-io 与 riscv git 源
│   └── lwext4_rust/          # 路径依赖：lwext4 的 Rust 封装
├── docs/                     # 团队面向文档
├── assets/                   # 图标、徽标
└── user/                     # 保留的用户态实验代码（不在默认构建路径）
```

---

## 🗺️ 路线图

### ✅ 已完成

- 双架构启动骨架：RISC-V / LoongArch 各自从汇编入口到 `rust_main`
- 基于 `lwext4_rust` 的 EXT4 真磁盘读写，挂载为根文件系统
- 虚拟文件系统层：`devfs` / `procfs` / `tmpfs`，以及合成的 `/dev/null` `/dev/zero` `/dev/tty`
- 进程 / 线程 / 信号：`clone` · `execve` · 信号投递 · PID 回收
- 内存管理：伙伴堆、物理帧分配、按进程 `MemorySet`、ELF Loader（含 `PT_INTERP` 重定向）
- 同步原语：`UPIntrFreeCell` / `SleepMutex` / `Condvar`
- VirtIO Block / Input、UART、PLIC、IOCSR 驱动
- 离线构建：所有 crates.io / git 依赖镜像至 `vendor/crates/`，零网络下可复现
- busybox 静态测试集主流程贯通

### 🚧 进行中 / 待办

- **glibc 动态链接启动路径**：补齐 `pread64` / `pwrite64` / `getrandom` / `madvise` / `mremap` / `getuid` / `getgid` / `rseq` 等启动期 syscall
- `/dev/urandom` 字符设备（让 glibc 的 `getrandom` 回退路径可用）
- 通过 lua、libc-test、iozone 全部测例
- 性能优化：lmbench / unixbench / iperf 路径
- LoongArch 评测路径完整验证
- 内核 backtrace / 可观测性增强

---

## 👥 团队

<div align="center">

**武汉大学 · WHUSP 队**

| 成员 | Member |
|------|--------|
| 彭灵钰 | Peng Lingyu |
| 石瑞博 | Shi Ruibo |

</div>

---

## 🙏 致谢

仅列出项目中**直接采用其代码**的开源工作：

- [**rCore-Tutorial-v3**](https://github.com/rcore-os/rCore-Tutorial-v3) —— 内核早期骨架的起点（启动流程、`UPIntrFreeCell`、地址空间抽象等借鉴自该教程）
- [**lwext4**](https://github.com/gkostka/lwext4) —— 内核中 EXT4 文件系统读写直接基于该 C 实现
- [**lwext4_rust**](https://github.com/elliott10/lwext4_rust) —— `lwext4` 的 Rust FFI 封装，本仓 `vendor/lwext4_rust/` 在其上做了少量本地补丁

---

<div align="center">

**Designed & Built at Wuhan University · 2026** ✨

</div>
