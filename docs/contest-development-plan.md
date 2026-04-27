# OSKernel2026 开发任务清单

## P0 — 提交闭环（contest submission blockers）

- [x] 重写根目录 `Makefile`，让 `make all` 成为正式提交入口
- [x] 让根目录 `make all` 产出 `kernel-rv`
- [ ] 让根目录 `make all` 产出 `kernel-la`
- [x] 清理提交链路对隐藏目录 `.cargo` 的依赖（仓库已无 `.cargo/`）
- [x] 把远程 Cargo 依赖改成离线可构建方案（`vendor/crates` + `vendor/config.toml`）
- [x] 在本地验证无网络构建仍然可用（`CARGO_NET_OFFLINE=true make all`）
- [x] 修改 `initproc`，支持比赛模式启动（当前内核直接加载 `/musl/busybox sh`）
- [ ] 新增 submit runner 用户程序
- [ ] 让 submit runner 按固定顺序串行执行测试脚本
- [ ] 输出精确的 `#### OS COMP TEST GROUP START xxxxx ####`
- [ ] 输出精确的 `#### OS COMP TEST GROUP END xxxxx ####`
- [ ] 在所有测试组结束后主动关机

## P1 — 启动与设备基线（基本完成，保留为记录）

- [x] 修正 `os/src/boards/qemu.rs`，去掉对 GPU 的硬依赖（已改为 `Option<IrqDevice>`）
- [x] 修正 `os/src/boards/qemu.rs`，去掉对键盘的硬依赖
- [x] 修正 `os/src/boards/qemu.rs`，去掉对鼠标的硬依赖
- [x] 用官方评测风格的无头 QEMU 命令验证内核可以启动（`ci-riscv-smoke.yml` 已覆盖）
- [x] 升级块设备发现逻辑，支持识别多个 virtio block 设备（`BLOCK_DEVICE_CAPACITY=8`，按 base 排序）
- [x] 明确区分评测盘 `x0` 和自带盘 `x1`
- [x] 设计并实现测试盘挂载路径（`x0` → `/`，`x1` → `/x1`）
- [x] 接入评测 EXT4 测试盘的只读访问

## P2 — basic-musl syscall 补齐（跑通 `/musl/basic_testcode.sh` 前置）

- [ ] 补齐 `basic-musl` 需要的 syscall（父任务）
  - [x] 补齐目录遍历与 `getdents64`
  - [x] `openat(56)` 基础版本
  - [x] `mkdirat(34)` / `unlinkat(35)` / `chdir(49)` / `getcwd(17)`
  - [ ] 升级 `openat` 相关语义（flags / mode / O_CREAT / O_DIRECTORY 完整行为）
  - [x] 升级 `execve` 的 `argv/envp` 传递（当前 `sys_exec` 只接 2 参数，无 envp）
  - [ ] 升级 `wait4/waitpid` 相关语义（options / rusage）
  - [ ] 补齐 `stat/fstat/newfstatat` 相关语义
  - [ ] 补齐 `mmap/munmap/brk` 相关语义
- [ ] 根据 `/musl/basic/run-all.sh` 输出继续补齐（2026-04-27）
  - [x] 当前已跑通（至少 basic 用例已观察通过）：`brk` / `chdir` / `clone` / `close` / `dup2` / `execve` / `exit` / `fork` / `fstat` / `getcwd` / `getdents64` / `getpid` / `mkdirat` / `mmap` / `munmap` / `openat` / `pipe` / `read` / `unlinkat` / `wait4(wait/waitpid)` / `write` / `yield`
  - [ ] `dup` 语义仍异常（`test_dup` 触发 assert）
  - [ ] `getppid(173)` 仍异常（`test_getppid` 输出 error）
  - [ ] `gettimeofday(169)` 语义仍不兼容（`test_gettimeofday` 输出 error）
  - [ ] `sleep` 相关语义仍不兼容（`test_sleep` 触发 assert）
  - [ ] `times(153)` 未补齐（`test_times` 触发 assert）
  - [ ] `uname(160)` 仍不兼容（`test_uname` 触发 assert）
  - [ ] `mount(40)` / `umount2(39)` 竞赛测试语义仍需完善（`mount` 当前返回 `-19`）
- [ ] 让 `/musl/basic_testcode.sh` 可以完整跑通
  - [ ] 非 syscall 问题：`/musl/basic_testcode.sh` 无 shebang，需用 `./busybox sh ./basic_testcode.sh` 执行

### syscall ABI 合规性审计（参考 `reference-project/RocketOS`、`oskernel_neverdown`、`NighthawkOS`、`RustOsWhu`；每条独立一轮，动手前对照 `man 2` + 参考实现）

- [x] 统一 `SYSCALL_OPENAT = 56` 命名：user 侧 `user/src/syscall.rs:9` 写作 `SYSCALL_OPEN`，kernel 侧 `os/src/syscall/mod.rs:9` 写作 `SYSCALL_OPENAT`，同号异名；语义已经是 openat，只是命名需要对齐
- [x] `sys_waitpid`(260) 升级为 `sys_wait4(pid, wstatus, options, rusage)`（基础路径已接入，后续继续补 Linux 细节）
- [x] 实现 `sys_exit_group`(94)（当前为单线程兼容实现，后续补完整线程组语义）
- [ ] 修正 `sys_kill`(129) 信号参数类型：`os/src/syscall/process.rs:106` 用 `SignalFlags::from_bits(signal)` 把信号当 bitflags，但 Linux 信号号是整数（SIGKILL=9、SIGTERM=15 不是位标志），应直接按 signum 分发
- [ ] errno support?

## P2.5 — cwd in pcb 收尾

- [ ] cwd in pcb（父任务）
  - [x] widen syscall arg forwarding to 6 args for Linux pathname syscalls
  - [x] add pcb `cwd_path` string alongside `WorkingDir`
  - [x] allow directory fd open and dirfd base extraction
  - [x] implement `chdir(49)`
  - [x] implement `getcwd(17)`
  - [x] upgrade syscall 56 to real `openat`
  - [x] implement `mkdirat(34)`
  - [x] implement `unlinkat(35)` for file removal
  - [ ] implement `fchdir(50)`
  - [ ] implement `newfstatat` / `fstatat`（与 P2 的 stat 家族合并推进）
  - [ ] implement `readlinkat`
  - [ ] implement `faccessat`
  - [ ] implement `renameat2`
  - [ ] implement `chroot`
  - [ ] implement `openat2`
  - [ ] support `..` in relative path resolution（部分完成，`os/src/fs/path.rs:209` 仍有 TODO）
  - [ ] support symlink traversal / nofollow semantics
  - [ ] make mount/umount target path respect cwd-relative resolution

## P3 — 扩展 libc 与动态链接

- [ ] 推进 `busybox` 需要的 shell / pipe / 重定向语义
- [ ] 推进 `lua` 所需的文件与执行环境兼容性
- [ ] 推进 `libctest-musl` 所需的动态链接与共享库运行时
- [ ] 补齐 `/lib/ld-musl-riscv64.so.1` 路径支持
- [ ] 推进 glibc 变体运行
- [ ] 补齐 `/lib/ld-linux-riscv64-lp64d.so.1` 路径支持
- [ ] 让 `/glibc/basic_testcode.sh` 可以运行

## P4 — 性能与压力测试

- [ ] 记录并跟踪 EXT4 phase 1 的 `huge_write` 性能回退（当前约 256KiB/s，对比旧 `easy-fs` 约 549KiB/s）
- [ ] 分析 `huge_write` 在 EXT4 路径上的瓶颈（分配、flush、缓存、写入粒度）
- [ ] 优化 EXT4 顺序写路径，让 `huge_write` 不再明显慢于旧 `easy-fs`
- [ ] 推进 `iozone`
- [ ] 推进 `unixbench`
- [ ] 推进 `lmbench`
- [ ] 推进 `iperf`
- [ ] 推进 `netperf`
- [ ] 推进 `cyclictest`
- [ ] 推进 `ltp`

## P5 — LoongArch

- [ ] 给 LoongArch 提前保留根构建入口和最小验证脚本（可并行）
- [ ] 推进 LoongArch 最小可构建路径
- [ ] 推进 LoongArch 最小可启动路径
- [ ] 推进 LoongArch 的 submit runner 闭环

## 基础设施与并行研究

- [x] 建立官方 QEMU 启动命令的本地复现脚本（CI `ci-riscv-smoke.yml` 覆盖）
- [x] 建立官方容器里的 smoke test 脚本
- [x] 建立 `basic` 用例到 syscall 的逐项对照表（`develop-guide/linux-syscall-implementation-survey.md`）
- [x] 对比 `RustOsWhu` / `NighthawkOS` 的提交路径并提炼可复用做法（`develop-guide/reference-project-notes.md`）
- [x] 评估 EXT4 方案的许可证、维护成本和提交打包方式（`develop-guide/lwext4-rust-research.md` + `ext4-phase1-migration-and-validation.md`）
- [x] 更合适比赛开发的 github ci
- [x] 升级 dependencies 😄（commit `c001a72`）
