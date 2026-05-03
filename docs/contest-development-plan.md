# OSKernel2026 开发任务清单

更新时间：2026-05-02

## 快照

- [x] 根目录 `make all` 已作为提交构建入口，当前目标是同时产出根目录 `kernel-rv` 和 `kernel-la`。
- [x] 根 `Makefile` 的 `kernel-rv` 通过 `os/ ARCH=riscv64` 构建，`kernel-la` 通过 `os/ ARCH=loongarch64` 构建。
- [x] 当前仓库已 vendoring Cargo 依赖到 `vendor/crates`，并通过 `vendor/config.toml` 支持离线构建。
- [x] 2026-04-28 移除默认 `user/` / `disk.img` 链路后，本地重新验证 `CARGO_NET_OFFLINE=true make all` 成功。
- [x] `make run-rv` 是默认 RISC-V 比赛形态启动：`x0 = sdcard-rv.img`，当前不传 `CONTEST_AUX_DISK` / `AUX_DISK`，也不挂载 `x1`。
- [x] `make run-la` 已有入口：`x0 = sdcard-la.img`。这只表示有构建/启动入口，不代表 LoongArch 比赛运行已经完整可用。
- [x] 内核当前直接从评测盘加载 `/musl/busybox sh` 作为 initproc。
- [x] 已有一次 RISC-V 全量手工运行记录：`develop-guide/contest-full-test-run-2026-04-27.md`。这次是主机注入命令，不是最终 submit runner。
- [x] `basic-musl` 在该记录中能跑到 END marker，2026-04-30 重跑 judge 结果是 `56 / 102`。
- [x] 在 2026-04-27 全量手工运行记录之后，源码又接入了 `times(153)`、`mprotect(226)`、`nanosleep(101)`、`clock_nanosleep(115)`、`gettimeofday(169)`、`uname(160)` 等修复；2026-04-30 重跑已验证提升。
- [ ] LoongArch 运行验证、submit runner、全组串行执行、自动 marker 管理、结束后主动关机。

## 判断

1. 最高优先级仍是 P0 submit runner 闭环。没有它，每次测试都依赖主机注入命令，不能稳定复现比赛提交形态。
2. P2.6 的 VFS/EXT4 长临界区和伪文件系统前置工作已完成，当前第二优先级转为补齐文件语义细节：`lseek` 回归、`fstat/stat` 稳健性、时间戳和目录/挂载边界。
3. `basic-musl` 还能短线补分，但要把 `dup2`、`wait/waitpid`、`pipe`、`mount/umount` 等剩余扣分点分开处理。
4. `mprotect` 已经接入源码，glibc 变体可以进入真实测试主体；后续按具体失败日志继续收窄。
5. LoongArch 有 `kernel-la` / `run-la` 入口和产物目标，但比赛运行仍要按具体验证记录描述；当前 RISC-V 提交闭环没稳定前，不让 LoongArch 抢主线。

## P0 - 提交

### 已完成

- [x] 重写根目录 `Makefile`，让 `make all` 成为正式提交入口。
- [x] 根目录 `make all` 产出 `kernel-rv` 和 `kernel-la`。
- [x] 根目录 `make kernel-rv` / `make kernel-la` 分别调用 `os/` 的 `ARCH=riscv64` / `ARCH=loongarch64`。
- [x] 根目录 `make all` 不再依赖仓库内 `user/`、rootfs 镜像打包或 `disk.img`。
- [x] 清理提交链路对隐藏目录 `.cargo` 的依赖，仓库根目录当前没有 `.cargo/`。
- [x] 把远程 Cargo 依赖改成离线可构建方案：`vendor/crates` + `vendor/config.toml`。
- [x] 本地重新验证离线构建入口仍可用：`CARGO_NET_OFFLINE=true make all`。
- [x] 接入比赛模式 initproc：内核直接加载 `/musl/busybox sh`。
- [x] `make run-rv` 默认使用 `sdcard-rv.img` 作为评测盘；当前根目标只传 `PRIMARY_DISK=$(TEST_DISK)`，不再把 `CONTEST_AUX_DISK` 传给 `os/Makefile`。
- [x] `make run-la` 默认使用 `sdcard-la.img` 作为评测盘；当前根目标只传 `PRIMARY_DISK=$(TEST_DISK_LA)`。
- [x] `os/Makefile` 的 `run-inner` 会检查 `PRIMARY_DISK`；`AUX_DISK` 变量存在且会被检查，但当前 QEMU 命令没有挂载 `x1`。

### 未完成

- [ ] 新增 submit runner 用户程序或等价启动入口。
- [ ] submit runner 按固定顺序串行执行测试脚本。
- [ ] runner 输出精确的 `#### OS COMP TEST GROUP START xxxxx ####`。
- [ ] runner 输出精确的 `#### OS COMP TEST GROUP END xxxxx ####`。
- [ ] 所有测试组结束后主动关机，而不是依赖超时或主机杀 QEMU。
- [ ] 在官方 contest Docker 中重新跑 `make all`、默认单盘 `make run-rv`，并按需要单独验证 `make run-la`。

### 顺序

1. 先做一个最小 submit runner：只跑 `basic-musl`，能输出 START/END marker，最后主动关机。
2. 再扩展到 musl 全组，保持串行执行。
3. 刷新 `basic-musl` judge 分数，确认 2026-04-27 全量记录之后的 syscall 修复是否提升旧的 `55 / 102`。
4. 最后再把 glibc 组纳入 runner，避免 glibc 动态链接问题阻塞 RISC-V 主闭环。

## P1 - 启动、设备、文件系统

- [x] 去掉 QEMU GPU 硬依赖，缺失时打印 `KERN: gpu device unavailable`。
- [x] 去掉键盘硬依赖，缺失时打印 `KERN: keyboard device unavailable`。
- [x] 去掉鼠标硬依赖，缺失时打印 `KERN: mouse device unavailable`。
- [x] 用 CI smoke 覆盖官方评测风格的无头 QEMU 启动。
- [x] 块设备发现支持多个 virtio block 设备，`BLOCK_DEVICE_CAPACITY = 8`，并按 MMIO base 排序。
- [x] 明确区分评测盘 `x0` 和可选辅助块设备 `x1`。
- [x] 当前根 `run-rv` / `run-la` 只挂载 `x0`；`x1` 只能通过后续显式 QEMU/Makefile 接线验证。
- [x] EXT4 测试盘作为主根文件系统挂载到 `/`。
- [x] 额外块设备 lazy-open 后可动态覆盖真实目录 `/x1`、`/x2`。
- [x] 接入评测 EXT4 测试盘的只读访问和普通路径读取。
- [x] 内核已有 SBI shutdown primitive，panic 或无任务时可关机。
- [ ] submit runner 主动调用关机。

## P2 - `basic-musl` 与 syscall 兼容

### 已接入的 syscall 和 ABI 能力

- [x] `getcwd(17)`。
- [x] `dup(23)`。
- [x] `dup3(24)`，包括基础 `O_CLOEXEC`。
- [x] `fcntl(25)` 基础实现：`F_DUPFD`、`F_DUPFD_CLOEXEC`、`F_GETFD`、`F_SETFD`、`F_GETFL`、`F_SETFL`、基础 lock no-op。
- [x] `ioctl(29)` 最小 tty 兼容。
- [x] `mkdirat(34)`。
- [x] `unlinkat(35)`。
- [x] `umount2(39)`，支持严格的动态 EXT4 mount 范围和 busy-mount 检查。
- [x] `mount(40)`，支持 whole-disk ext4、`/dev/vdXN` 分区解析、FAT/VFAT adapter 和 tmpfs fallback。
- [x] `statfs(43)`，支持当前 VFS mount 的最小 filesystem 统计，解锁 BusyBox `df`。
- [x] `chdir(49)`。
- [x] `openat(56)` 基础路径、dirfd、目录 fd 能力。
- [x] `pipe2(59)`，使用 Linux `int[2]` ABI，支持 `O_NONBLOCK` / `O_CLOEXEC`。
- [x] `getdents64(61)`。
- [x] `lseek(62)`，支持普通文件 `SEEK_SET` / `SEEK_CUR` / `SEEK_END`，pipe/stdio 返回 `ESPIPE`。
- [x] `readv(65)` 和 `writev(66)`。
- [x] `ppoll(73)` 最小 BusyBox 兼容。
- [x] `newfstatat(79)` 和 `fstat(80)` 基础实现。
- [x] `utimensat(88)` / `futimens` 基础实现，支持 fd、`AT_FDCWD`、`AT_EMPTY_PATH`、`UTIME_NOW`、`UTIME_OMIT`。
- [x] `exit_group(94)` 单线程兼容实现。
- [x] `waitid(95)` 基础实现。
- [x] `nanosleep(101)` 和 `clock_nanosleep(115)` 基础实现。
- [x] `syslog(116)` 最小兼容，解锁 BusyBox `dmesg`。
- [x] `kill(129)` 按 Linux signum 解析，支持 signal 0 检查和基础信号投递。
- [x] `rt_sigtimedwait(137)` 基础实现，可读取 pending signal，超时返回 `EAGAIN`。
- [x] `times(153)` 源码已接入，使用当前进程 CPU time 快照。
- [x] `uname(160)` 源码已接入，返回 Linux 风格 `utsname`。
- [x] `getrlimit(163)` / `setrlimit(164)` / `prlimit64(261)` 基础实现，当前以 `RLIMIT_NOFILE` 等进程资源表为核心。
- [x] `gettimeofday(169)` 源码已接入，当前基于 monotonic timer。
- [x] `getppid(173)`。
- [x] `brk(214)`。
- [x] `mmap(222)`、`munmap(215)`、`mprotect(226)`。
- [x] `clone(220)`、`execve(221)`、`wait4(260)` 基础路径。
- [x] `execve` 支持 `argv/envp`，并支持 shebang 脚本解释器重写。
- [x] 官方评测盘无 `/bin/sh` 时，脚本解释器可 fallback 到 `/musl/busybox` 或 `/glibc/busybox`。
- [x] fd table 已有 `FdTableEntry`，区分 fd flags 和 file status flags，并支持 close-on-exec。
- [x] `symlinkat(36)`。
- [x] `linkat(37)`。
- [x] `renameat2(276)` 基础实现：支持 `RENAME_NOREPLACE`。
- [x] `faccessat(48)` 基础实现：F_OK 存在性检查和 X_OK 执行位检查。
- [x] `fchdir(50)`。
- [x] `readlinkat(78)`。
- [x] `statx(291)` 基础实现。

### 已验证的 `basic-musl` 结果

- [x] 可通过 `cd /musl && ./busybox sh ./basic_testcode.sh` 手工跑到 END marker。
- [x] 上次全量记录中 `basic-musl` 官方 judge 为 `56 / 102`（2026-04-30 phase4-basic）。
- [x] 上次记录中已经拿满的 basic 子项：`brk`、`chdir`、`clone`、`close`、`dup`、`fork`、`fstat`、`getcwd`、`getppid`、`mkdir`、`openat`、`uname`、`yield`。
- [x] 2026-04-27 全量记录之后新增的 `times`、`mprotect`、`nanosleep`、`utimensat` 等实现已在后续重跑中验证；最新 basic 记录为 `60 / 102`。

### 当前 `basic-musl` 剩余缺口

- [ ] `dup2` / `dup3` 兼容细节：最新 basic 记录中 `test_dup2` 仍为 `0 / 2`。
- [ ] `times(153)` 2026-04-30 judge 为 `1 / 6`，需重跑确认。
- [ ] `gettimeofday(169)` 2026-04-30 judge 为 `1 / 3`，需重跑确认。
- [ ] `mount(40)` / `umount2(39)` 的比赛测试语义未完成：已有 `/dev/vdXN` 分区解析和 FAT/VFAT adapter，但 basic mount/umount 仍只有 `2 / 5`。
- [ ] `wait4/waitpid` 的 status、options、rusage 细节仍不足。
- [ ] `openat` 的 `flags/mode/O_CREAT/O_DIRECTORY/O_APPEND/O_TRUNC/O_EXCL/O_NOFOLLOW` 仍需补齐。
- [ ] `newfstatat/fstat/lstat` 的目录、pipe、stdio、设备号、时间戳、nlink 等细节仍需审计。
- [ ] `getdents64` 的 offset 稳定性、跨 mount readdir、buffer 边界还需补齐。
- [ ] `pipe` 的阻塞、非阻塞、关闭端、错误码等 Linux 细节还不完整。
- [x] 最小 `/dev/null` / devfs 已实现，支持 `/dev/null`、`/dev/zero`、`/dev/random`、`/dev/urandom`。
- [x] `sys_kill(129)` 已按 Linux signum 解析；`busybox sh -c 'sleep 5' & ./busybox kill $!` 已通过。

### 2026-05-02 当前新增缺口

已完成或不再适用的 2026-05-01 项已从 TODO 表移除：shebang fallback、`renameat2`、`unlinkat(AT_REMOVEDIR)`、`linkat`、RISC-V FPU/用户栈、`kill(0)`、`kill` 投递、procfs、tmpfs `/tmp`、`lseek`、`prlimit64`、`rt_sigtimedwait`、`utimensat`。

| 优先级 | 功能 | 关联测试 | 当前症状 | 修复方向 |
|--------|------|----------|----------|----------|
| 1 | `dup2` / `dup3` 细节 | `basic-musl` | `test_dup2` 仍为 `0 / 2` | 按 Linux 语义复查 oldfd==newfd、close-on-exec、fd 覆盖和错误码 |
| 2 | `mount(40)` / `umount2(39)` | `basic-musl` | `test_mount`、`test_umount` 仍各只有 `2 / 5` | 继续修 FAT/VFAT、分区挂载、busy target、umount 后路径状态和错误码 |
| 3 | `wait4/waitpid` / exit status | `basic-musl`、pthread 相关 | `test_wait`、`test_waitpid` 均为 `1 / 4` | 补 status 编码、options、rusage、线程组和被 signal 终止后的回收 |
| 4 | pipe / fd I/O 语义 | `basic-musl`、BusyBox pipeline | `test_pipe` 仍为 `1 / 4` | 补阻塞/非阻塞、关闭端、EOF、`EPIPE`、SIGPIPE、poll readiness |
| 5 | pthread/libctest 线程支线 | `libctest pthread_*` | `pthread_cancel(td)` 仍失败或超时；cond/TSD/robust 用例也会卡住 | 按 P2.7 的顺序拆解：TID/退出清理、classic futex、定向 signal/cancel、PI futex、robust futex |
| 6 | socket 最小 IPv4 UDP/TCP | `libctest socket` | socket 系列 syscall 仍 ENOSYS | 决定是否实现 loopback 最小语义，或明确作为网络扩展项延后 |

### 2026-05-02 测试结果快照

| 测试组 | 通过 | 失败 | 通过率 |
|--------|------|------|--------|
| basic-musl | 60/102 | 20 个扣分子项 | 59% |
| busybox-musl | 52/55 | 3 (`which ls`, `hwclock`, `kill 10`) | 95% |
| lua-musl | 9/9 | 0 | 100% (FPU fix + 128KiB stack) |
| libctest-musl | 未重新 judge | 手工运行仍未到 END | 主要卡在 fscanf/ungetc/stat/pthread/socket/utime 等语义 |

### 2026-05-02 `libctest-musl` 手工运行新增 TODO

这次在 `/musl $ ./libctest_testcode.sh` 中已经有大量 libc 纯计算/字符串类用例通过，但整组仍未到 END；当前失败集中在下面几类内核语义缺口。

| 优先级 | Syscall / 功能 | 关联用例 | 当前症状 | TODO |
|--------|---------------|----------|----------|------|
| 1 | fscanf/ungetc/stat 的卡死点 | `fscanf`、`stat`、`ungetc` | 仍被 runner SIGKILL 超时 | 在 `lseek` 已接入后重新跑单项，区分 fd offset、stdio 缓冲、`read` EOF、`stat` 死锁和资源回收问题 |
| 2 | tmpfile / 匿名临时文件写入链路 | `fwscanf`、`utime`、后续 `tmpfile` 相关用例 | 旧日志显示 `write: No error information`；需在 `lseek/utimensat` 后复测 | 审计 `/tmp` 上 `O_TMPFILE`/匿名临时文件兼容、`open(O_CREAT|O_EXCL)` fallback、tmpfs `write_at` 返回值和 errno |
| 3 | `fstat/stat` 稳健性 | `stat`、`utime` | 旧日志里 `utime` 在 `fstat` 路径触发 VFS panic；需确认是否已修复 | 复查 tmpfs、匿名临时文件、已 unlink 但仍打开文件、目录和设备文件的 inode 生命周期处理 |
| 4 | pthread/libctest 线程支线 | `pthread_cancel*`、`pthread_cond`、`pthread_tsd`、`pthread_mutex*`、`pthread_robust` | 取消、条件变量、TSD、PI/robust mutex 用例仍失败或超时 | 详见 P2.7，先补线程退出和 classic futex，再补 signal/cancel、PI futex、robust owner-death |
| 5 | signal mask 保存恢复 | `setjmp`、pthread cancel | `siglongjmp incorrectly restored mask`；取消信号也依赖每线程 mask | 补齐 `rt_sigprocmask(135)`、`rt_sigaction(134)`、`rt_sigreturn(139)` 与每线程 signal mask 保存恢复 |
| 6 | socket 最小 IPv4 UDP/TCP 语义 | `socket` | `socket/bind/getsockname/setsockopt/sendto/recvfrom/listen/connect/accept` 全部 ENOSYS | 至少实现 `AF_INET` + UDP loopback/本机收发、TCP listen/connect/accept 的最小兼容；或明确先作为非主线网络得分项延后 |
| 7 | 超时/杀进程后的资源回收 | `fscanf`、`pthread_*`、`stat`、`ungetc` | 多个用例由 runner SIGKILL 后结束 | 确认 SIGKILL 能终止所有线程、释放 fd/锁/futex waiter，避免后续用例继承坏状态或持锁死锁 |

## P2.7 - pthread/libctest 线程支线

**目标**：先打通 `rv-musl entry-static.exe pthread_*` 静态用例。动态 pthread 用例仍受 PT_INTERP / 动态链接路线影响，不作为本支线第一验收目标。

### 用例拆分

| 测试文件 | 最可能卡点 | 推荐阶段 | 验收信号 |
|----------|------------|----------|----------|
| `pthread_mutex.c` | `futex` WAIT/WAKE、`clock_gettime`、`clone`、`pthread_join` | 阶段 1-2 | 普通、errorcheck、recursive mutex 的 relock/unlock 语义不超时，返回值符合预期 |
| `pthread_cond.c` | `futex` WAIT/WAKE，可能用到 REQUEUE/CMP_REQUEUE；`nanosleep`；多 waiter 唤醒 | 阶段 2 | signal/broadcast 都能唤醒等待线程，三个 waiter 全部 join 成功 |
| `pthread_tsd.c` | `clone` TLS、线程退出、`CLONE_CHILD_CLEARTID`、join futex wake、TSD destructor | 阶段 1-2 | 子线程退出后 TSD destructor 执行，主线程 TSD 不被破坏 |
| `pthread_cancel.c` | `tgkill`/`tkill`、`rt_sigaction`、`rt_sigreturn`、cancel signal、cleanup handler、join | 阶段 3-4 | async cancel 和 sleep cancel 都返回 `PTHREAD_CANCELED`，cleanup handler 全部执行 |
| `pthread_cancel-points.c` | cancel point、sem/futex wait、`clock_gettime`、`openat`/`close`/`unlinkat`、`/dev/shm` | 阶段 4 | sem_wait、sem_timedwait、pthread_join 作为取消点生效；`shm_open` 场景不被误取消 |
| `pthread_mutex_pi.c` | `FUTEX_LOCK_PI`、`FUTEX_UNLOCK_PI`、`FUTEX_TRYLOCK_PI`，至少不能直接 ENOSYS | 阶段 5 | PI mutex 的 relock/unlock 错误码与非 PI 版本一致；真实优先级继承可暂标 `UNFINISHED` |
| `pthread_robust.c` | `set_robust_list`、`get_robust_list`、owner-death、robust futex、PI futex | 阶段 6 | owner 线程退出后 lock 返回 `EOWNERDEAD`，consistent 后可恢复；未恢复时返回 `ENOTRECOVERABLE` |

### 推荐推进顺序

1. [ ] **线程 ID 地基**：区分内核 task slot tid 和 Linux-visible TID；补 `gettid(178)`、`set_tid_address(96)`，并让 `tkill/tgkill`、robust list、clear-child-tid 都使用 Linux-visible TID。
2. [ ] **线程退出清理**：消费 `CLONE_CHILD_CLEARTID` / `set_tid_address` 保存的 `clear_child_tid`，非主线程退出时写 0、`futex_wake(1)`、从 task table 移除并回收线程资源。
3. [ ] **classic futex 校准**：复查现有 `futex(98)` 的 WAIT、WAKE、WAIT_BITSET、WAKE_BITSET、REQUEUE、CMP_REQUEUE 的 errno、timeout、返回计数和 waiter 清理，先满足 musl pthread mutex/cond/join。
4. [ ] **先过非取消 pthread 用例**：按 `pthread_mutex.c` -> `pthread_cond.c` -> `pthread_tsd.c` 顺序验收，确认基础线程创建、TLS、join、futex wake 不再超时。
5. [ ] **signal/cancel 地基**：补 `tkill(130)`、`tgkill(131)`、`rt_sigprocmask(135)`、`rt_sigaction(134)`、`rt_sigreturn(139)`；把 signal pending/mask 从 process-wide 位图推进到每线程语义，并能唤醒可中断等待。
6. [ ] **pthread cancel**：按 `pthread_cancel.c` -> `pthread_cancel-points.c` 验收，确保 cancel signal 不误杀整个进程，cleanup handler 执行，取消点和非取消点行为分开。
7. [ ] **PI futex 最小兼容**：补 `FUTEX_LOCK_PI`、`FUTEX_UNLOCK_PI`、`FUTEX_TRYLOCK_PI` 的 owner/waiter 语义；真实 priority inheritance 可先保留 `// UNFINISHED:`。
8. [ ] **robust futex**：补 `set_robust_list(99)` / `get_robust_list(100)`，线程退出时遍历 robust list，设置 `FUTEX_OWNER_DIED` 并唤醒 waiter。

### 验收命令

- [ ] `make kernel-rv`。
- [ ] item 级运行：`cd /musl && ./runtest.exe -w entry-static.exe pthread_mutex`。
- [ ] item 级运行：`cd /musl && ./runtest.exe -w entry-static.exe pthread_cond`。
- [ ] item 级运行：`cd /musl && ./runtest.exe -w entry-static.exe pthread_tsd`。
- [ ] item 级运行：`cd /musl && ./runtest.exe -w entry-static.exe pthread_cancel`。
- [ ] item 级运行：`cd /musl && ./runtest.exe -w entry-static.exe pthread_cancel_points`。
- [ ] item 级运行：`cd /musl && ./runtest.exe -w entry-static.exe pthread_mutex_pi`。
- [ ] item 级运行：`cd /musl && ./runtest.exe -w entry-static.exe pthread_robust`。
- [ ] 回归：`tools/contest_runner/run_groups.py --arch rv --libcs musl --groups basic,busybox,lua` 不退化。
- [ ] 完整组：`tools/contest_runner/run_groups.py --arch rv --libcs musl --groups libctest` 能跑过 pthread 段，不再因 pthread 用例 SIGKILL 超时。

## P2.5 - cwd in PCB 收尾

### 已完成

- [x] syscall 参数转发扩大到 6 个参数，满足 Linux pathname syscalls。
- [x] PCB 中加入 `cwd_path` 字符串，与 `WorkingDir` 一起维护。
- [x] 允许目录 fd open，并支持从 dirfd 提取路径基准目录。
- [x] `chdir(49)`。
- [x] `getcwd(17)`。
- [x] `openat(56)` 使用真实 dirfd/cwd 语义。
- [x] `mkdirat(34)`。
- [x] `unlinkat(35)`。
- [x] `mount(40)` / `umount2(39)` 的目标路径使用当前进程 cwd 做相对路径解析。
- [x] 基础 `..` 解析已接入。

### 已完成（新增）

- [x] `fchdir(50)`。
- [x] `readlinkat(78)`。
- [x] `faccessat(48)`。
- [x] `renameat2(276)`。

### 未完成

- [ ] `chroot`。
- [ ] `openat2`。
- [ ] symlink traversal / nofollow semantics。
- [ ] mounted root 的 `..` 语义仍是临时行为：当前没有记录 covered directory 的父目录。

## P2.6 - VFS 稳健化路线图

### 阶段 0：冻结事实与回归用例

- [x] 文件系统调用链已记录在 `develop-guide/current-filesystem-reading-tutorial.md`。
- [x] BusyBox pipeline / benchmark 并发触发 `UPIntrFreeCell` borrow panic 的现象已记录在 `develop-guide/contest-full-test-run-2026-04-27.md`。
- [x] 已确认 `UPIntrFreeCell` 不适合包住可能阻塞、可能 schedule 的长临界区对象。
- [ ] 固化一个最小手工验收命令：`/musl/busybox ls /musl/basic | /musl/busybox grep gettimeofday` 不应 panic。
- [ ] 固化一个基础正例验收命令：`/musl/basic/pipe` 仍输出 `Write to pipe successfully.`。
- [ ] 给上述验收加 timeout，区分死锁、panic 和正常退出。

### 阶段 1：修掉 mount/EXT4 跨调度借用 panic

- [x] 新增可睡眠的内核互斥原语，例如 `SleepMutex<T>` / `BlockingMutex<T>`。
- [x] 将可能等待 I/O 的 mount slot 从 `UPIntrFreeCell` 迁出。
- [x] 保持 `DYNAMIC_MOUNTS` 只做短临界区元数据操作，不进入块设备 I/O。
- [x] 改造 `with_mount()`：同一 mount 被其他任务使用时应等待，而不是 `borrow_mut()` panic。
- [x] 验证 BusyBox pipeline、`/musl/basic/pipe`、`/musl/basic/gettimeofday`、默认单盘 `make run-rv`。

### 阶段 1.5：块 I/O 策略与文件对象锁边界

- [x] 官方评测只要求 EXT4 测试盘、串行执行测试点、完整 marker 输出和结束后主动关机；不要求块设备 I/O 必须异步。
- [x] 官方 QEMU 形态中 RV 使用 `virtio-blk-device`，LA 使用 `virtio-blk-pci`，因此设备后端和中断成熟度可以按架构分别验收。
- [x] NighthawkOS / RocketOS 的 submit runner 都采用 `fork` / `execve` / `waitpid` 串行运行测试脚本，RocketOS 最后主动 `shutdown()`。
- [x] RocketOS / RustOsWhu 的 RV 与 LA virtio block 路径均以同步 `read_blocks` / `write_blocks` 作为稳定基线。
- [x] 短期将 `DEV_NON_BLOCKING_ACCESS` 默认保持为 `false`，RV/LA 都先走同步块 I/O，确保 `busybox-musl` pipeline 不再卡在 START marker 后。
- [x] 保留架构差异说明：LA 在 virtio-pci 外部 IRQ 路径未验收前保持 polling/sync；RV 只有通过 BusyBox/basic 回归后才允许重新打开 nonblocking。
- [x] 梳理所有 `OSInode` / fd 文件对象持有 `UPIntrFreeCell` guard 后进入 `with_mount()` / EXT4 / block I/O 的路径。
- [x] 将文件 offset / status 更新拆成短临界区；实际 `read_at` / `write_at` / `stat` / `read_dirent64` I/O 不持有 `UPIntrFreeCell` guard 跨 `schedule()`。
- [x] 对共享 offset 的 `read` / `write` / `write_append` / `read_dirent64` 增加可睡眠文件对象锁或等价序列化方案。
- [x] 保持 `DYNAMIC_MOUNTS` 只覆盖短元数据临界区；长 I/O 状态继续放在可睡眠锁保护下。
- [x] 重新验证 `./busybox cat ./busybox_cmd.txt | while read line`、完整 `busybox_testcode.sh`、`/musl/basic/pipe`、`/musl/basic/gettimeofday`。
- [x] 只有 RV 通过上述回归后，才允许把 RV 的 nonblocking block I/O 重新打开；LA 等 virtio-pci IRQ / 外部中断路径单独通过验收后再考虑。

2026-04-30 阶段 1.5 验证记录：

- `tools/contest_runner/run_groups.py --arch rv --libcs musl --groups busybox --out develop-guide/test-run-logs/2026-04-30-phase1_5/busybox-rv-musl --no-build`：`busybox-musl` end-seen，`42 / 55`。
- `tools/contest_runner/run_groups.py --arch rv --libcs musl --groups basic --out develop-guide/test-run-logs/2026-04-30-phase1_5/basic-rv-musl --no-build`：`basic-musl` end-seen，`56 / 102`，`test_gettimeofday` 与 `test_pipe` 都打印 END。
- `timeout 25s make run-rv`：默认单盘启动到 `/musl/busybox sh`，timeout 只用于截断交互式 shell。
- 当前默认策略仍是同步块 I/O；RV nonblocking 不是默认值，后续若重新打开必须重新跑同一组回归。

### 阶段 2：泛化 mount 系统以支持伪文件系统

**目标**：让 mount 表不再绑定块设备索引，使 procfs/tmpfs/devfs 可以作为普通 mount 实例注册。

**优先级**：HIGH — procfs/tmpfs 的前置条件。

- [x] 将 `MountId` 从块设备索引改为全局递增的 mount 实例 ID。
  - 修改 `os/src/fs/mount.rs`：新增 `NEXT_MOUNT_ID: AtomicUsize`。
  - 块设备 mount 仍占据前 N 个 slot，pseudo-fs mount 从 N 开始分配。
- [x] 将 `MOUNTS` 从固定长度 `Vec<SleepMutex<Option<Arc<MountedFs>>>>` 改为可动态增长的结构。
- [x] 在 `MountedFs` 中新增 `fs_type: &'static str` 字段（"ext4" / "proc" / "tmpfs" / "devtmpfs"）。
- [x] 新增 `register_pseudo_mount(backend: Box<dyn FileSystemBackend>, fs_type: &'static str) -> MountId`。
- [x] 在 `FileSystemBackend` trait 中新增 `fn root_ino(&self) -> u32 { 2 }` 默认方法。
- [x] 修改 `os/src/fs/vfs/path.rs`：移除对 `lwext4_rust::ffi::EXT4_ROOT_INO` 的直接依赖。
  - `VfsCursor::root()` 使用 `root_ino()` 查询。
  - `is_mount_root()` 查询对应 mount 的 `root_ino()`。
  - `follow_mounted_root()` 使用 `root_ino_for(mount_id)` 而非硬编码 2。
- [x] 新增 `mount_pseudo_fs_at(target: WorkingDir, backend: Box<dyn FileSystemBackend>, fs_type: &'static str) -> Result<MountId, MountError>`。
- [x] 新增 `list_mounts() -> Vec<MountInfo>` 供 procfs `/proc/mounts` 使用。
- [x] 验证：`make all`、`make run-rv`、`basic-musl` 文件系统用例不回退。

2026-05-01 阶段 2 验证记录：

- `make all`：通过，产出 `kernel-rv` 和 `kernel-la`。
- `tools/contest_runner/run_groups.py --arch rv --libcs musl --groups basic --out develop-guide/test-run-logs/2026-05-01-phase2/basic-rv-musl --no-build`：`basic-musl` end-seen，`56 / 102`，未退化；该阶段是 procfs/tmpfs 前置结构，未预期直接涨分。

### 阶段 3：procfs 实现（HIGH — 解锁 ~12 busybox 命令）

**目标**：实现最小 procfs 并挂载到 `/proc`，解锁 `df`、`free`、`ps`、`uptime`。

**新建文件**：`os/src/fs/procfs.rs`

- [x] 新建 `os/src/fs/procfs.rs`，实现 `FileSystemBackend` trait。
- [x] 定义 procfs inode 编号方案（root=2, mounts=3, meminfo=4, uptime=5, PID 目录=100+PID, PID 子文件=10000+PID*10+offset）。
- [x] 实现 `/proc/mounts`：调用 `mount::list_mounts()`，格式 `<device> <mountpoint> <fstype> <options> 0 0`。
- [x] 实现 `/proc/meminfo`：从 frame allocator 获取 total/free 帧数。
  - 已在 `os/src/mm/frame_allocator.rs` 新增 `pub fn frame_stats() -> (usize, usize)`。
  - 输出 `MemTotal`、`MemFree`、`MemAvailable`、`Buffers`(0)、`Cached`(0)、`SwapTotal`(0)、`SwapFree`(0)。
- [x] 实现 `/proc/uptime`：格式 `SECONDS.NN IDLE.NN\n`，从内核 timer 计算。
- [x] 实现 `/proc` 目录 readdir：列出固定条目 + 当前存活 PID（从 `PID2PCB` 迭代）。
- [x] 实现 `/proc/<PID>/stat`：Linux 格式 `pid (comm) state ppid ...`。
- [x] 实现 `/proc/<PID>/status`：`Name:\tcomm\nState:\tS\nPid:\tN\nPPid:\tN\nVmRSS:\tN kB\n`。
- [x] 实现 `/proc/<PID>/cmdline`：NUL-separated argv。
  - 已在 `ProcessControlBlockInner` 新增 `cmdline: Vec<String>`，在 `execve` 时保存。
- [x] 所有写操作返回 `FsError::ReadOnly`。
- [x] 在 `init_mounts()` 末尾挂载 procfs 到 `/proc`。
- [x] 补 `statfs(43)`，让 BusyBox `df` 能查询 `/`、`/proc`、`/tmp` 的 filesystem 统计。
- [x] 验证：`/musl/busybox df`、`/musl/busybox free`、`/musl/busybox ps`、`/musl/busybox uptime`。

2026-05-01 阶段 3 验证记录：

- `make fmt`：通过。
- `make all`：通过，产出 `kernel-rv` 和 `kernel-la`。
- `tools/contest_runner/run_groups.py --arch rv --libcs musl --groups busybox --out develop-guide/test-run-logs/2026-05-01-phase3-statfs/busybox-rv-musl --no-build`：`busybox-musl` end-seen，`49 / 55`；`df`、`free`、`ps`、`uptime` 均 success。

### 阶段 4：tmpfs 实现（MEDIUM — 解锁 libctest mkstemp/mkdtemp）

**目标**：实现内存文件系统并挂载到 `/tmp`。

**新建文件**：`os/src/fs/tmpfs.rs`

- [x] 新建 `os/src/fs/tmpfs.rs`，实现 `FileSystemBackend` trait。
- [x] 内部数据结构：`TmpfsInode { kind, mode, nlink, data: Vec<u8>, children: BTreeMap<String, u32>, parent_ino, ctime_us, mtime_us }` + `TmpFs { inodes: BTreeMap<u32, TmpfsInode>, next_ino }`。
- [x] 实现 VFS 需要的文件操作：create_file、create_dir、unlink、rename、link、symlink、read_at、write_at、set_len、readlink。
- [x] `read_at` / `write_at` 直接操作 `data: Vec<u8>`，write 时自动扩展。
- [x] 在 `init_mounts()` 中挂载 tmpfs 到 `/tmp`。
- [x] 验证：`/musl/busybox sh -c 'echo hello > /tmp/test'`、`cat /tmp/test`、`mkdir /tmp/dir`、`ls /tmp`、`rm /tmp/test`、`rmdir /tmp/dir`。
- [x] 验证：`/musl/busybox mktemp /tmp/tmp.XXXXXX` 和 `/musl/busybox mktemp -d /tmp/dir.XXXXXX` 均成功，覆盖 mkstemp/mkdtemp 的核心路径。
- [x] `touch /tmp/test` 已通过 `utimensat(88)` 基础实现解锁；`busybox touch test.txt` 在最新 busybox 记录中 success。
- [ ] 完整 `libctest-musl` 仍需重跑确认；旧阻塞点 `rt_sigtimedwait(137)`、`prlimit64(261)`、`lseek(62)` 已有源码实现，不再作为 ENOSYS TODO。

2026-05-01 阶段 4 验证记录：

- `make fmt`：通过。
- `make all`：通过，产出 `kernel-rv` 和 `kernel-la`。
- `CARGO_NET_OFFLINE=true make all`：通过。
- `make run-rv` 手工验收：`/tmp` 上 create/write/read/mkdir/readdir/unlink/rmdir 均成功；`busybox mktemp` / `mktemp -d` 均成功。
- `tools/contest_runner/run_groups.py --arch rv --libcs musl --groups busybox --out develop-guide/test-run-logs/2026-05-01-phase4/busybox-rv-musl --no-build`：`busybox-musl` end-seen，保持 `49 / 55`，未退化。
- `tools/contest_runner/run_groups.py --arch rv --libcs musl --groups libctest --out develop-guide/test-run-logs/2026-05-01-phase4/libctest-rv-musl --no-build`：未到 END，`0 / 220`；这是旧记录，里面的 `rt_sigtimedwait` / `getrlimit` / `lseek` ENOSYS 结论已过期。

### 阶段 5：devfs 迁移为 VFS backend（LOW）

**目标**：将 devfs 从 VFS 前拦截改为标准 mount，统一路径解析语义。

- [ ] 重构 `os/src/fs/devfs.rs`，实现 `FileSystemBackend` trait。
  - `lookup_component_from`：root 下匹配 "null"/"zero"/"tty"/"ttyS0"/"random"/"urandom"。
  - `read_at` / `write_at`：根据 ino 分发到 UART / null / zero 逻辑。
  - `stat`：返回 `S_IFCHR` + 正确的 rdev。
- [ ] 在 `init_mounts()` 中挂载 devfs 到 `/dev`。
- [ ] 移除 `open_file_at` 和 `stat_at` 中的 devfs 前置拦截。
- [ ] 验证：`ls /dev`、`echo test > /dev/null`、所有依赖 `/dev/null` 的测试不回退。

### 阶段 6：正规化路径解析与 mount crossing

- [x] 消除 `os/src/fs/vfs/path.rs` 对 `EXT4_ROOT_INO` 的残余依赖。
- [ ] 修复 mounted root 下 `..` 的语义：精确匹配 mount instance 而非 `rposition`。
- [ ] 实现 symlink traversal（max depth = 40）：
  - 非 final component 的 symlink 必须 follow。
  - final component 根据 `O_NOFOLLOW` / `AT_SYMLINK_NOFOLLOW` 决定。
- [ ] 验证：跨 mount symlink、`..` 穿越 mount boundary。

### 阶段 7：补 Linux VFS 关键语义

- [ ] 完善 `openat` 的 `O_CREAT|O_EXCL`、`O_TRUNC`、`O_APPEND`、`O_DIRECTORY`、`O_NOFOLLOW` 组合。
- [ ] 完善 `mkdirat/unlinkat/rmdir` 的 Linux errno 映射。
- [ ] 完善 `newfstatat/fstat/lstat` 的设备号、时间戳、nlink。
- [ ] 完善 `getdents64` 的 offset 稳定性和 buffer 边界。
- [x] `mount/umount2` 已支持 `fstype` 参数（"ext4"/"vfat"/"fat32"/"fat"/"tmpfs"/"ramfs"）和 busy target 基础检查。
- [ ] 继续完善 `mount/umount2` 的比赛语义：FAT/VFAT 真实行为、umount 后状态、分区错误码和 fallback 策略。
- [ ] 所有不完整语义用 `// UNFINISHED:` 标出。

### 阶段 8：缓存与性能

- [ ] 语义稳定后加 inode cache，cache key 使用 `(mount_id, ino)`。
- [ ] 加正向 dentry cache；负向 cache 等 rename/unlink 语义稳定后再考虑。
- [ ] 加简单 page/block cache，先服务 ELF 加载、顺序读、`getdents64`。
- [ ] 为 cache 加失效路径：`create/unlink/rename/truncate/write`。
- [ ] procfs/tmpfs 不需要 block cache（纯内存），但 dentry cache 可覆盖。

### 阶段 9：验收门槛

- [x] `make fmt`。
- [x] `make all`。
- [x] `CARGO_NET_OFFLINE=true make all`。
- [ ] `make run-rv` 下 `/musl/basic_testcode.sh` 通过。
- [ ] pipeline 复现不 panic，重复运行 5 次不死锁。
- [x] `busybox-musl` 完整脚本打印 END marker。
- [x] `busybox-musl` 中 `df`、`free`、`ps`、`uptime` 命令通过（依赖 procfs）。
- [ ] `libctest-musl` 中 `mkstemp`、`mkdtemp` 用例通过（依赖 tmpfs；需在新版 `lseek/rt_sigtimedwait/prlimit64` 后重跑）。
- [ ] `basic-musl` 文件系统相关用例全部通过。
- [x] `/proc/mounts` 输出格式被 `df` 正确解析。
- [x] `/proc/meminfo` 输出格式被 `free` 正确解析。
- [x] LA 的 nonblocking block I/O 不作为当前门槛；当前默认仍以同步块 I/O 稳定性为准。

## P2.6.3 - procfs 实现细节

### 设计决策

1. **直接实现 `FileSystemBackend` trait**：不引入 Dentry/Inode/SuperBlock 三层抽象，当前 trait 已足够。
2. **内容按需生成**：每次 `read_at` 动态生成文件内容，不缓存。procfs 文件通常 < 4KB。
3. **确定性 inode 编号**：PID → ino 映射，避免维护分配器。
4. **只读**：不支持 `/proc/sys` 等可写接口。

### 需要新增的内核基础设施

| 基础设施 | 位置 | 用途 |
|----------|------|------|
| `frame_stats() -> (total, free)` | `os/src/mm/frame_allocator.rs` | `/proc/meminfo` |
| `ProcessControlBlockInner::cmdline` | `os/src/task/process.rs` | `/proc/<PID>/cmdline` |
| `list_mounts() -> Vec<MountInfo>` | `os/src/fs/mount.rs` | `/proc/mounts` |
| `get_time_us()` | `os/src/timer.rs` | `/proc/uptime` |
| `PID2PCB` 只读迭代接口 | `os/src/task/manager.rs` | `/proc` 目录列表 |

### 预期比赛得分影响

procfs 解锁的 busybox 命令（当前因 `/proc` 不存在而失败）：

| 命令 | 依赖的 proc 文件 |
|------|-----------------|
| `df` | `/proc/mounts` |
| `free` | `/proc/meminfo` |
| `ps` | `/proc/uptime` + `/proc/<PID>/stat` |
| `uptime` | `/proc/uptime` |
| `cat /proc/mounts` | `/proc/mounts` |
| `mount`（无参数） | `/proc/mounts` |

### 实现顺序

1. 先完成阶段 2 mount 泛化的最小子集：`register_pseudo_mount` + `root_ino()` 方法。
2. 实现 procfs 骨架：只有 `/proc/mounts` 和 `/proc/meminfo`。
3. 挂载并验证 `busybox df` 和 `busybox free`。
4. 补充 `/proc/uptime` 和 `/proc/<PID>/*`。
5. 重跑 `busybox-musl` judge，确认得分提升。

当前状态：以上 5 步已完成。`busybox-musl` 已从早期 `43 / 55` 提升到 50+ 分；最新校准按 `52 / 55` 记录，剩余项单独跟踪。

## P2.6.4 - tmpfs 实现细节

### 设计决策

1. **纯内存 BTreeMap 存储**：inode 数据存在 `Vec<u8>`，目录结构用 `BTreeMap<String, u32>`。
2. **无容量限制**：当前不设 tmpfs 大小上限（内核堆足够大）。
3. **标准 VFS 语义**：支持 create/read/write/unlink/mkdir/rmdir/rename/link/symlink 全套操作。
4. **时间戳**：使用 `get_time_us()` 填充 ctime/mtime。

### 数据结构

```rust
struct TmpfsInode {
    kind: FsNodeKind,
    mode: u32,
    nlink: u32,
    data: Vec<u8>,
    children: BTreeMap<String, u32>,
    parent_ino: u32,
    ctime_us: u64,
    mtime_us: u64,
}

pub(crate) struct TmpFs {
    inodes: BTreeMap<u32, TmpfsInode>,
    next_ino: u32,
}
```

### 预期比赛得分影响

| 测试组 | 用例 | 依赖 |
|--------|------|------|
| libctest-musl | `mkstemp` | `/tmp` 可写 |
| libctest-musl | `mkdtemp` | `/tmp` 可写 |
| libctest-musl | `tmpfile` | `/tmp` 可写 |
| busybox-musl | `mktemp` | `/tmp` 可写 |

注意：即使不用 tmpfs，在 ext4 上 `mkdir /tmp` 也能部分解决问题。但 tmpfs 是更正确的方案（不污染评测盘、不需要写权限）。

### 实现顺序

1. 临时方案：在 `init_mounts()` 中对 ext4 根执行 `create_dir(root, "tmp", 0o1777)` 作为过渡。
2. 阶段 2 mount 泛化完成后，实现 tmpfs backend。
3. 挂载 tmpfs 到 `/tmp`，替代 ext4 上的 `/tmp` 目录。
4. 验证 libctest mkstemp/mkdtemp。

当前状态：tmpfs backend 和 `/tmp` 挂载已完成；`busybox mktemp` / `mktemp -d` / `touch` 已验证。完整 libctest 需要在新版 `lseek`、`rt_sigtimedwait`、`prlimit64`、`utimensat` 后重跑。

## P2.6.5 - 可选 FAT/VFAT 支持路线图

- [x] 采用 vendored `fatfs`，`os/Cargo.toml` 已通过 `vendor/crates/fatfs` 离线引入。
- [x] 为 WHUSP 块设备实现 `fatfs::Read` / `fatfs::Write` / `fatfs::Seek` 适配层。
- [x] 在 `os/src/fs/fat.rs` 新增 FAT mount wrapper。
- [x] 泛化当前 mount 表，让它能同时承载 EXT4、FAT 和伪文件系统 mount。
- [x] 在 `sys_mount` 中接受 `fstype == "vfat"` / `"fat32"` / `"fat"`。
- [x] 支持 `/dev/vdXN` 分区源解析，basic mount 测例不再卡在无法定位 FAT 分区。
- [ ] 首轮不承诺 symlink、Unix owner/mode、hard link、完整时间戳和大小写规则；缺口用 `// UNFINISHED:` 标明。
- [ ] 验证 FAT32 镜像只读 lookup/read、create/write/read/remove，并重跑 `/musl/basic/mount` 和 `/musl/basic/umount`。

## P2.7 - syscall 层瘦身路线图

### 已完成或部分完成

- [x] Linux syscall 号常量已大体对齐 RISC-V Linux ABI。
- [x] repo-private net syscalls 已从 Linux 29-31 移出并最终退役，避免继续占用私有 ABI。
- [x] `FdTableEntry` 已承载 fd flags 和 status flags，是 `fcntl` / close-on-exec 的基础。
- [x] 部分 UAPI 类型已集中到 `os/src/syscall/fs/uapi.rs`。

### 未完成

- [ ] 明确 syscall adapter 只做参数取值、用户指针复制、flag/errno 转换、fd 引用获取、调用子系统。
- [ ] 将通用 uaccess 从 `syscall/fs/user_ptr.rs` 迁到 `os/src/mm/uaccess.rs` 或 `os/src/uaccess.rs`。
- [ ] 拆出 exec 装载层，把 ELF/shebang 逻辑迁出 `syscall/process.rs`。
- [ ] 收敛 fd table API：`fd_get/fd_alloc/fd_install/fd_close/fd_dup/fd_set_flags`。
- [ ] 等 P2.6 VFS 对象层落地后，再把路径 syscall 变成纯 VFS adapter。
- [ ] UAPI 类型继续归档到 `syscall/*/uapi.rs`。
- [ ] 建立防回退检查，禁止重新引入旧的 repo-private syscall 命名和大块 syscall 内业务逻辑。

## P3 - 扩展 libc 与动态链接

- [x] `mprotect(226)` 源码已接入，用于解锁 glibc 动态加载器的第一道门。
- [x] 重跑 glibc 组，旧的 `cannot apply additional memory protection after relocation` 已不再是全局入口阻塞；`lua-glibc` 可跑到 END。
- [x] `busybox-musl` 的 shell、pipe、pipeline、重定向主路径已可跑完整脚本到 END；剩余 `which ls`、`hwclock`、kill 相关细节单独跟踪。
- [x] `lua-musl` 所需的 mmap/brk/fs/signal/FPU/用户栈主路径已完成，`lua-musl` 9/9。
- [ ] 推进 `libctest-musl` 的工作目录、脚本布局和动态链接运行时。
- [ ] 补齐或验证 `/lib/ld-musl-riscv64.so.1` 路径支持。
- [ ] 推进 glibc 变体运行。
- [ ] 补齐或验证 `/lib/ld-linux-riscv64-lp64d.so.1` 路径支持。
- [ ] 让 `/glibc/basic_testcode.sh` 在当前代码上重新跑到 marker，并刷新 judge 结果。

## P4 - 性能与压力测试

- [x] 记录 EXT4 phase 1 的 `huge_write` 性能回退：约 256 KiB/s，对比旧 `easy-fs` 约 549 KiB/s。
- [ ] 分析 `huge_write` 在 EXT4 路径上的瓶颈：分配、flush、缓存、写入粒度。
- [ ] 优化 EXT4 顺序写路径。
- [ ] 推进 `iozone`。
- [ ] 推进 `unixbench`。
- [ ] 推进 `lmbench`。
- [ ] 推进 `iperf`。
- [ ] 推进 `netperf`。
- [ ] 推进 `cyclictest`。
- [ ] 推进 `ltp`。

## P5 - LoongArch

### 阶段 0：冻结采用路线

- [ ] 采用内置 `arch/` 拆分作为主线，吸收 `NighthawkOS` 的小 HAL facade 组织方式。
- [ ] `polyhal` 只作为设计/代码参考，不先接入完整 runtime。
- [ ] 复查可借用点：LoongArch `_start`、DMW/MMU 初始化、TLB refill、CSR timer、GED shutdown、virtio-pci 块设备、syscall register ABI。

### 阶段 1：先做 RISC-V 行为不变的架构拆分

- [x] 新增 `os/src/arch.rs`，用 `#[cfg(target_arch = ...)]` 选择 `riscv64` / `loongarch64`。
- [x] 新增 `os/src/arch/riscv64/`，迁入当前低层入口、trap、timer、SBI、board。
- [x] 让 generic kernel 只通过 `crate::arch` 调用低层入口。
- [x] 保持当前 RISC-V 启动契约不变。
- [x] 验证 `make fmt`、`make all`、`CARGO_NET_OFFLINE=true make all`、`make run-rv`。

### 阶段 2A：LoongArch 构建入口骨架

- [x] 根 `Makefile` 新增 `kernel-la` 目标，并通过 `os/ ARCH=loongarch64` 构建根目录 `kernel-la`。
- [x] 根 `Makefile` 新增 `run-la` 目标，当前只把 `sdcard-la.img` 作为 `PRIMARY_DISK` / `x0` 传给 `os/ run-inner`。
- [x] `os/Makefile` 支持 `ARCH=loongarch64` 的 target/QEMU/virtio-pci 变量，并使用真实 `kernel` 构建入口。
- [x] `user/Makefile` 对 `ARCH=loongarch64` 明确失败，说明用户态 syscall wrapper 属于阶段 5。
- [x] `rust-toolchain.toml` 纳入 `loongarch64-unknown-none` target。
- [x] 验证 `make kernel-la` / `make run-la` 不再是缺目标；后续仍需按测试场景验证 LoongArch 比赛运行能力。

### 阶段 2B：真正产出 `kernel-la`

- [x] 根 `make all` 同时产出 `kernel-rv` 和 `kernel-la`。
- [x] 新增 LoongArch linker script，不复用 RISC-V `linker-qemu.ld`。
- [x] `os/Makefile` 的 `ARCH=loongarch64 kernel` 已是真实构建。
- [ ] 在官方 contest Docker 中复核 LoongArch 所需 crate/toolchain 依赖的离线构建路径。

### 阶段 3：LoongArch 最小内核可启动

- [ ] 复核并补齐 `arch/loongarch64/entry` 的比赛运行路径。
- [ ] 复核并补齐 `arch/loongarch64/console` 的串口输出/输入路径。
- [ ] 复核并补齐 `arch/loongarch64/shutdown` 的关机路径。
- [ ] 复核并补齐 `arch/loongarch64/time` 的定时器路径。
- [ ] 复核并补齐 `arch/loongarch64/trap` 的异常、syscall 和返回路径。
- [ ] 复核并补齐 `arch/loongarch64/mm` 的 DMW、TLB 和页表切换路径。
- [ ] 复核并补齐 `arch/loongarch64/context` / `switch.S` 的上下文切换路径。
- [ ] 验证 QEMU LoongArch 能稳定跑官方评测盘脚本，并能主动 shutdown。

### 阶段 4：LoongArch 设备与文件系统路径

- [ ] 明确 QEMU LoongArch virt 设备模型：块设备优先走 PCI virtio。
- [ ] 接入 LoongArch PCI/virtio block 发现，至少识别 `x0 = sdcard-la.img`。
- [ ] 保持文件系统上层接口不分叉。
- [ ] 验证从 `sdcard-la.img` 挂载根目录并读取 `/musl`、`/glibc`、测试脚本。
- [ ] 验证 optional `x1` 辅助盘在 LoongArch 下的动态挂载路径。

### 阶段 5：LoongArch 用户态与 submit runner

- [ ] 产出 LoongArch 用户程序 ELF，并确认 ELF loader 识别 `EM_LOONGARCH`。
- [ ] 对齐 LoongArch 用户态入口、栈、TLS、syscall 返回值、errno 负值约定。
- [ ] 补齐 LoongArch musl BusyBox 启动所需的动态链接器路径或兼容路径。
- [ ] 泛化 submit runner，让同一套 runner 能按 `basic-musl` / `busybox-musl` / `glibc` 等组名输出精确 marker。
- [ ] 验证 `submit-la` 或等价入口按固定顺序串行执行测试组，并在结束后主动 shutdown。

### 阶段 6：LoongArch 验收门槛

- [ ] `make fmt`。
- [ ] `make kernel-rv`。
- [ ] `make kernel-la`。
- [ ] `make all`。
- [ ] `CARGO_NET_OFFLINE=true make all`。
- [ ] `make run-rv` 不回退。
- [ ] `make run-la` 或等价命令能启动 `sdcard-la.img` 并完成目标测试场景。
- [ ] 官方 contest Docker 中验证 `kernel-rv`、`kernel-la` 产物名正确。

## 基础设施与研究记录

- [x] 建立官方 QEMU 启动命令的本地复现脚本，CI `ci-riscv-smoke.yml` 只做 smoke：确认 initproc 启动并看到 `basic-musl` 的 START/END marker。
- [x] 建立官方容器里的 smoke test 脚本。
- [x] 建立 basic 用例到 syscall 的逐项对照表：`develop-guide/linux-syscall-implementation-survey.md`。
- [x] 对比 `RustOsWhu` / `NighthawkOS` 的提交路径并提炼可复用做法：`develop-guide/reference-project-notes.md`。
- [x] 评估 EXT4 方案的许可证、维护成本和提交打包方式：`develop-guide/lwext4-rust-research.md` 和 `develop-guide/ext4-phase1-migration-and-validation.md`。
- [x] 建立更适合比赛开发的 GitHub CI。
- [x] 升级 dependencies，并保留离线 vendor 路线。
