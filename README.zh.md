<div align="center">

<img src="assets/whu.png" alt="Wuhan University" width="420"/>

# WHUSP

一个用 Rust 编写的现代宏内核，双架构支持 **RISC-V 64** 和 **LoongArch 64**

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

- [快速开始](#-快速开始)
- [项目结构](#-项目结构)
- [团队](#-团队)

---

## 项目结构

```
oskernel2026-whusp/
├── Makefile                  # 大赛风格根入口：make all / run-rv / run-la
├── os/                       # 内核 crate
│   └── src/
│       ├── arch/             # 架构差异收敛点
│       │   ├── riscv64/      #   ↳ entry · trap · signal · switch · board
│       │   └── loongarch64/  #   ↳ entry · trap · signal · switch · board
│       ├── mm/               # 堆、帧、页表、MemorySet、ELF Loader
│       ├── task/             # 进程/线程/调度/信号/exec/clone/initproc
│       ├── fs/               # VFS、EXT4、devfs、procfs、tmpfs
│       ├── drivers/          # VirtIO、UART、PLIC / IOCSR
│       ├── sync/             # UPIntrFreeCell、Mutex、Condvar、SleepMutex
│       ├── syscall.rs        # 分发表（文件，非模块）
│       └── syscall/          # 各子系统 handler 实现
├── vendor/
│   ├── crates/               # 镜像的 crates.io / git 依赖（离线构建）
│   ├── config.toml           # 重定向 crates-io 与 riscv git 源
│   └── lwext4_rust/          # 路径依赖：lwext4 的 Rust FFI 封装
├── docs/                     # 团队文档
├── assets/                   # 图标与徽标
└── user/                     # 遗留用户态实验（不在默认构建路径）
```

---

## 🚀 快速开始

### 环境准备

- **Rust** `nightly-2025-05-20`（参见 [`rust-toolchain.toml`](rust-toolchain.toml)），需包含：
  - 组件：`rust-src`、`llvm-tools`、`rustfmt`、`clippy`
  - 目标平台：`riscv64gc-unknown-none-elf`、`loongarch64-unknown-none`
- **QEMU** ≥ 10.0.2，需含 `qemu-system-riscv64` 与 `qemu-system-loongarch64`
- **Python 3** 与 **`mkfs.ext4`**（用于构建测试脚本盘）
- **测试磁盘镜像** — 从 [oscomp/testsuits-for-oskernel releases](https://github.com/oscomp/testsuits-for-oskernel/releases) 下载：
  - `sdcard-rv.img`（RISC-V，约 4 GiB）
  - `sdcard-la.img`（LoongArch，约 4 GiB）
- *（可选）* **Docker** 镜像 [`zhouzhouyi/os-contest:20260104`](https://hub.docker.com/r/zhouzhouyi/os-contest)，用于官方比赛环境：
  ```bash
  docker run -it --rm -v $(pwd):/code zhouzhouyi/os-contest:20260104 bash
  ```

### 构建

```bash
make all          # 完整提交构建：格式化 → 测试脚本盘 → kernel-rv → kernel-la
make kernel-rv    # 仅构建 RISC-V 内核
make kernel-la    # 仅构建 LoongArch 内核
make clean        # 清理所有构建产物
```

离线 / 本地镜像构建（无需网络）：
```bash
CARGO_NET_OFFLINE=true make all
```

### 运行

在 QEMU 中启动内核并挂载测试磁盘：

```bash
make run-rv                          # 启动 RISC-V（默认磁盘：./sdcard-rv.img）
make run-la                          # 启动 LoongArch（默认磁盘：./sdcard-la.img）

# 覆盖测试磁盘或调整资源
make run-rv TEST_DISK=/path/to/sdcard-rv.img
make run-rv MEM=2G SMP=4
```

### 测试配置

测试用例**不**编译进内核。它们存放在一个自动生成的**脚本盘**（`disk.img`）中，
该盘作为第二块块设备（`x1`）挂载，由内核 init 进程在启动时执行。

脚本盘由 `scripts/` 下的两个文件构建：

| 脚本 | 作用 |
|------|------|
| `scripts/export_contest_case_scripts.py` | **核心配置文件** — 定义运行哪些测试组、使用哪个 libc、跑哪些 LTP 用例。在此处修改配置常量，然后重建。 |
| `scripts/build_contest_disk.sh` | 调用 Python 导出器并创建 ext4 磁盘镜像。 |

修改配置后重建脚本盘：

```bash
make contest-disk
```

#### 配置项一览

所有配置项位于 `scripts/export_contest_case_scripts.py`（及一个配套文件）：

| 配置项 | 默认值 | 说明 |
|--------|--------|------|
| `INTERACTIVE_SHELL` | `False` | `True` → 进入 BusyBox 交互 shell，不执行测试（调试模式） |
| `TEST_SCRIPTS` | 全部 11 个组 | 启用哪些测试组。删除条目即可跳过对应套件。 |
| `TEST_LIBCS` | `("/glibc", "/musl")` | 测试哪些 libc 根目录。 |
| `LTP_CASE_FILTER_OPTION` | `None` | 运行时过滤 LTP 用例。`None` = 跑完整白名单；`"prefix:ioctl"` = 仅 ioctl 相关用例；`"case:fork07"` = 单个用例；`"a"`–`"z"` = 按首字母筛选；`"range:start,end"` = 字典序区间。 |
| [`scripts/ltp_whitelist.txt`](scripts/ltp_whitelist.txt) | ~800 个用例 | 精选的 LTP 用例列表（每行一个）。当 `LTP_CASE_FILTER_OPTION` 为 `None` 时生效。 |

#### 常见工作流

**调试单个 LTP 用例：**

1. 编辑 `scripts/export_contest_case_scripts.py`：
   ```python
   INTERACTIVE_SHELL = True
   LTP_CASE_FILTER_OPTION = "case:fork07"
   ```
2. 重建并运行：
   ```bash
   make contest-disk && make run-rv
   ```

**快速迭代 — 只跑 basic 测试组：**

1. 编辑 `scripts/export_contest_case_scripts.py`：
   ```python
   TEST_SCRIPTS = ("basic_testcode.sh",)
   ```
2. 重建并运行：
   ```bash
   make contest-disk && make run-rv
   ```

**向白名单添加新的 LTP 用例：**

```bash
echo "new_case_name" >> scripts/ltp_whitelist.txt
make contest-disk
```

---

## 团队

<div align="center">

**武汉大学 · WHUSP 队**

| 成员 | Member |
|------|--------|
| 彭灵钰 | Peng Lingyu |
| 石瑞博 | Shi Ruibo |

</div>

---

<div align="center">

**Designed & Built at Wuhan University · 2026** ✨

</div>
