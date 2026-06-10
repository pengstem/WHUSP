# OSKernel2026 开发任务清单

更新时间：2026-06-10

## 快照

- [x] 根目录 `make all` 已作为提交构建入口，当前目标是同时产出根目录 `kernel-rv` 和 `kernel-la`。
- [x] 根 `Makefile` 的 `kernel-rv` 通过 `os/ ARCH=riscv64` 构建，`kernel-la` 通过 `os/ ARCH=loongarch64` 构建。
- [x] 当前仓库已 vendoring Cargo 依赖到 `vendor/crates`，并通过 `vendor/config.toml` 支持离线构建。
- [x] `make validation` 是当前本地预提交入口：依次运行 `make fmt`、`make contest-disk`、`make kernel-rv`、`make kernel-la`。
- [x] `make run-rv` 是默认 RISC-V 比赛形态启动：`x0 = sdcard-rv.img`，`x1 = disk.img`，PID 1 通过 `/x1/entry.sh` 执行生成脚本。
- [x] `make run-la` 已有构建和 QEMU 入口：`x0 = sdcard-la.img`，`x1 = disk.img`。最新自动评分仍为 0，不能等同于 LoongArch 比赛运行闭环。
- [x] 内核当前直接从评测盘加载 `/musl/busybox sh -c <runner command>` 作为 initproc。
- [x] 已有一次 RISC-V 全量手工运行记录：`develop-guide/contest-full-test-run-2026-04-27.md`。这次是主机注入命令，不是最终 submit runner。
- [x] 2026-06-08 自动评分记录：RISC-V 总分 `547 / 1164`，其中 `basic-glibc = 102 / 102`、`basic-musl = 102 / 102`、`busybox-glibc = 54 / 55`、`busybox-musl = 54 / 55`、`lua-glibc = 9 / 9`、`lua-musl = 9 / 9`、`libctest-musl = 217 / 220`。
- [x] RISC-V submit runner、全组串行执行、自动 marker 管理、结束后 `sync; reboot -f` 已闭环。
- [ ] LoongArch 最新自动评分仍是 `0 / 1160`，`tools/score_runs/logs/20260608-212715/run-la.log` 没有 guest marker；LA 仍按单独运行链路跟进。

## 判断

1. P0 RISC-V submit runner 已闭环，不再是阻塞项。
2. `basic-glibc` / `basic-musl` 已拿满；旧的 `dup2`、`times`、`gettimeofday`、`mount/umount`、`wait`、`pipe` 等 basic 缺口已关闭。
3. `busybox-*`、`lua-*` 和 `libctest-musl` 已进入高分状态；当前短线补分集中在 `busybox` 剩余 1 分、`libctest-musl` 剩余 3 分、`libctest-glibc` 未计分项。
4. LTP、iozone、iperf、lmbench、netperf、libcbench 等评分组仍需要单独推进；最新总分里这些组仍为 0。
5. LoongArch 有 `kernel-la` / `run-la` 入口、架构层和脚本盘路径，但最新自动评分仍为 0，不能写成 LA 运行得分闭环。

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
- [x] `make run-rv` 默认使用 `sdcard-rv.img` 作为 `PRIMARY_DISK / x0`，并构建 `disk.img` 作为 `AUX_DISK / x1` 脚本盘。
- [x] `make run-la` 默认使用 `sdcard-la.img` 作为 `PRIMARY_DISK / x0`，并同样接入 `AUX_DISK / x1` 脚本盘；LA guest 侧仍未打分闭环。
- [x] `os/Makefile` 的 `run-inner` 会检查 `PRIMARY_DISK` 和已设置的 `AUX_DISK`，当前 RV/LA QEMU 命令都会把 `x1` 作为辅助块设备挂上。

### 已完成闭环

- [x] 新增 submit runner 用户程序或等价启动入口。
- [x] submit runner 按固定顺序串行执行测试脚本。
- [x] runner 输出精确的 `#### OS COMP TEST GROUP START xxxxx ####`。
- [x] runner 输出精确的 `#### OS COMP TEST GROUP END xxxxx ####`。
- [x] 所有测试组结束后主动关机，而不是依赖超时或主机杀 QEMU。
- [x] 在官方 contest Docker 中重新跑 `make all`、默认单盘 `make run-rv`，并按需要单独验证 `make run-la`。

### 已执行顺序

1. [x] 先做一个最小 submit runner：只跑 `basic-musl`，能输出 START/END marker，最后主动关机。
2. [x] 再扩展到 musl 全组，保持串行执行。
3. [x] 刷新 `basic-musl` judge 分数，确认 2026-04-27 全量记录之后的 syscall 修复是否提升旧的 `55 / 102`。
4. [x] 把 glibc 组纳入 runner；最新 RISC-V 已验证 `basic-glibc`、`busybox-glibc`、`lua-glibc`。

## P1 - 启动、设备、文件系统

- [x] 去掉 QEMU GPU 硬依赖，缺失时打印 `KERN: gpu device unavailable`。
- [x] 去掉键盘硬依赖，缺失时打印 `KERN: keyboard device unavailable`。
- [x] 去掉鼠标硬依赖，缺失时打印 `KERN: mouse device unavailable`。
- [x] 用 CI smoke 覆盖官方评测风格的无头 QEMU 启动。
- [x] 块设备发现支持多个 virtio block 设备，`BLOCK_DEVICE_CAPACITY = 8`，并按 MMIO base 排序。
- [x] 明确区分评测盘 `x0` 和可选辅助块设备 `x1`。
- [x] 当前根 `run-rv` / `run-la` 挂载 `x0` 评测盘和 `x1` 生成脚本盘。
- [x] EXT4 测试盘作为主根文件系统挂载到 `/`。
- [x] 额外块设备 lazy-open 后可动态覆盖真实目录 `/x1`、`/x2`。
- [x] 接入评测 EXT4 测试盘的只读访问和普通路径读取。
- [x] 内核已有 SBI shutdown primitive，panic 或无任务时可关机。
- [x] submit runner 主动调用关机。

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
- [x] `gettid(178)` / `set_tid_address(96)`，支撑 pthread join / clear-child-tid。
- [x] `futex(98)` classic WAIT/WAKE/REQUEUE 路径与 PI futex 最小兼容。
- [x] `set_robust_list(99)` / `get_robust_list(100)`，线程退出时处理 robust futex owner-death。
- [x] `tkill(130)` / `tgkill(131)`、`rt_sigaction(134)`、`rt_sigprocmask(135)`、`rt_sigreturn(139)`，支撑 pthread cancel 和 per-thread signal mask。
- [x] socket/libctest 所需的最小兼容路径已足够让 `entry-static.exe socket` 和 `entry-dynamic.exe socket` 通过。

### 已验证的 `basic-musl` 结果

- [x] 可通过 `cd /musl && ./busybox sh ./basic_testcode.sh` 手工跑到 END marker。
- [x] 上次全量记录中 `basic-musl` 官方 judge 为 `56 / 102`（2026-04-30 phase4-basic）。
- [x] 上次记录中已经拿满的 basic 子项：`brk`、`chdir`、`clone`、`close`、`dup`、`fork`、`fstat`、`getcwd`、`getppid`、`mkdir`、`openat`、`uname`、`yield`。
- [x] 2026-04-27 全量记录之后新增的 `times`、`mprotect`、`nanosleep`、`utimensat` 等实现已在后续重跑中验证；最新 basic 记录为 `60 / 102`。
- [x] 2026-06-08 自动评分已刷新：`basic-glibc = 102 / 102`，`basic-musl = 102 / 102`。

### 已关闭的旧 `basic-musl` 缺口

- [x] `dup2` / `dup3` basic 兼容细节已通过 latest basic judge。
- [x] `times(153)` 已通过 latest basic judge。
- [x] `gettimeofday(169)` 已通过 latest basic judge。
- [x] `mount(40)` / `umount2(39)` 已通过 latest basic judge；更完整的 mount namespace / FAT / LTP 语义另归 LTP/VFS 项。
- [x] `wait4/waitpid` basic status 路径已通过 latest basic judge；更完整的 Linux options/rusage 语义另归 LTP 项。
- [x] `openat` basic flags/mode 路径已通过 latest basic judge；`O_NOFOLLOW` 等完整 VFS 语义另归 P2.6。
- [x] `newfstatat/fstat/lstat` basic 路径已通过 latest basic judge；设备号、时间戳、nlink 等深水语义另归 LTP/VFS 项。
- [x] `getdents64` basic 路径已通过 latest basic judge；offset 稳定性和跨 mount 边界另归 LTP/VFS 项。
- [x] `pipe` basic 路径和 BusyBox pipeline 已通过 latest judge / runner。
- [x] 最小 `/dev/null` / devfs 已实现，支持 `/dev/null`、`/dev/zero`、`/dev/random`、`/dev/urandom`。
- [x] `sys_kill(129)` 已按 Linux signum 解析；`busybox sh -c 'sleep 5' & ./busybox kill $!` 已通过。

### 2026-06-10 当前剩余缺口

2026-05-02 的 basic / pthread / libctest 主线缺口已经基本关闭。剩余工作不再按旧 basic 表推进，而是按最新评分口径收敛。

| 优先级 | 功能 | 关联测试 | 当前症状 | 修复方向 |
|--------|------|----------|----------|----------|
| 1 | `libctest-musl` 剩余 3 分 | `libctest-musl` | 最新 `217 / 220` | 从 `score-rv.json` / raw log 定位剩余未计分项，避免重做已通过 pthread/socket/stat 主线 |
| 2 | `busybox-*` 剩余 1 分 | `busybox-glibc`、`busybox-musl` | 最新均为 `54 / 55` | 对照 judge 项找最后一条格式/输出差异 |
| 3 | `libctest-glibc` | `libctest-glibc` | 当前 runner 仍跳过，得分 0 | 确认是否纳入默认组，再处理 glibc 动态 libctest 细节 |
| 4 | LTP | `ltp-glibc`、`ltp-musl` | 最新默认评分 0 | 按 whitelist / scorer 限制继续做 case-family 修复 |
| 5 | 性能和网络组 | `iozone`、`iperf`、`lmbench`、`netperf`、`libcbench` | 最新默认评分 0 | 分组跑 raw log，避免和 functional 主线混在一起 |
| 6 | LoongArch 评分链路 | `--arch la` | 最新 `0 / 1160` 且无 guest marker | 先恢复 `make run-la` 可观测 guest 输出，再谈 LA 得分 |

### 2026-06-08 测试结果快照

| 测试组 | 通过 | 失败 | 通过率 |
|--------|------|------|--------|
| basic-glibc | 102/102 | 0 | 100% |
| basic-musl | 102/102 | 0 | 100% |
| busybox-glibc | 54/55 | 1 | 98% |
| busybox-musl | 54/55 | 1 | 98% |
| lua-glibc | 9/9 | 0 | 100% |
| lua-musl | 9/9 | 0 | 100% |
| libctest-musl | 217/220 | 3 | 99% |
| RISC-V 总分 | 547/1164 | - | 47% |
| LoongArch 总分 | 0/1160 | - | 0% |

### 已关闭的 2026-05-02 `libctest-musl` 手工 TODO

最新 `libctest-musl` 已跑到 END marker 并取得 `217 / 220`。下面这些旧阻塞点不再按 TODO 处理。

| 状态 | Syscall / 功能 | 关联用例 | 最新证据 |
|------|---------------|----------|----------|
| [x] | fscanf/ungetc/stat 卡死点 | `fscanf`、`stat`、`ungetc` | static/dynamic 对应用例均 Pass |
| [x] | 临时文件和 `/tmp` 写入链路 | `fwscanf`、`utime`、`mkdtemp_failure`、`mkstemp_failure` | static/dynamic 对应用例均 Pass |
| [x] | `fstat/stat` 稳健性 | `stat`、`utime`、`statvfs` | static/dynamic 对应用例均 Pass |
| [x] | pthread/libctest 线程支线 | `pthread_cancel*`、`pthread_cond`、`pthread_tsd`、`pthread_robust_detach` | static/dynamic 主线均 Pass |
| [x] | signal mask 保存恢复 | `setjmp`、`sigprocmask_internal`、pthread cancel | static/dynamic 对应用例均 Pass |
| [x] | socket 最小 libc 兼容 | `socket` | static/dynamic `socket` 均 Pass |
| [x] | 超时/杀进程后的资源回收 | `pthread_*`、`stat`、`ungetc` | `libctest-musl` 跑到 END marker |

## P2.7 - pthread/libctest 线程支线

**目标**：先打通 `rv-musl entry-static.exe pthread_*` 静态用例。当前静态/动态 pthread 主线已在 `libctest-musl` 中通过，后续只跟踪剩余未计分边角项。

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

1. [x] **线程 ID 地基**：区分内核 task slot tid 和 Linux-visible TID；补 `gettid(178)`、`set_tid_address(96)`，并让 `tkill/tgkill`、robust list、clear-child-tid 都使用 Linux-visible TID。
2. [x] **线程退出清理**：消费 `CLONE_CHILD_CLEARTID` / `set_tid_address` 保存的 `clear_child_tid`，非主线程退出时写 0、`futex_wake(1)`、从 task table 移除并回收线程资源。
3. [x] **classic futex 校准**：复查现有 `futex(98)` 的 WAIT、WAKE、WAIT_BITSET、WAKE_BITSET、REQUEUE、CMP_REQUEUE 的 errno、timeout、返回计数和 waiter 清理，先满足 musl pthread mutex/cond/join。
4. [x] **先过非取消 pthread 用例**：按 `pthread_mutex.c` -> `pthread_cond.c` -> `pthread_tsd.c` 顺序验收，确认基础线程创建、TLS、join、futex wake 不再超时。
5. [x] **signal/cancel 地基**：补 `tkill(130)`、`tgkill(131)`、`rt_sigprocmask(135)`、`rt_sigaction(134)`、`rt_sigreturn(139)`；把 signal pending/mask 从 process-wide 位图推进到每线程语义，并能唤醒可中断等待。
6. [x] **pthread cancel**：按 `pthread_cancel.c` -> `pthread_cancel-points.c` 验收，确保 cancel signal 不误杀整个进程，cleanup handler 执行，取消点和非取消点行为分开。
7. [x] **PI futex 最小兼容**：补 `FUTEX_LOCK_PI`、`FUTEX_UNLOCK_PI`、`FUTEX_TRYLOCK_PI` 的 owner/waiter 语义；真实 priority inheritance 可先保留 `// UNFINISHED:`。
8. [x] **robust futex**：补 `set_robust_list(99)` / `get_robust_list(100)`，线程退出时遍历 robust list，设置 `FUTEX_OWNER_DIED` 并唤醒 waiter。

### 验收命令

- [x] `make kernel-rv`。
- [x] item 级运行：`entry-static.exe pthread_cond`。
- [x] item 级运行：`entry-static.exe pthread_tsd`。
- [x] item 级运行：`entry-static.exe pthread_cancel`。
- [x] item 级运行：`entry-static.exe pthread_cancel_points`。
- [x] item 级运行：`entry-static.exe pthread_cancel_sem_wait`、`pthread_cond_smasher`、`pthread_condattr_setclock`、`pthread_exit_cancel`、`pthread_once_deadlock`、`pthread_rwlock_ebusy`。
- [x] item 级运行：`entry-dynamic.exe pthread_cond`、`pthread_tsd`、`pthread_cancel`、`pthread_cancel_points`、`pthread_cond_smasher`、`pthread_condattr_setclock`、`pthread_exit_cancel`、`pthread_once_deadlock`、`pthread_rwlock_ebusy`。
- [x] item 级运行：`pthread_mutex` 原始项当前不在最新 `libctest-musl` log 中单列；mutex/futex 主路径已由 pthread cond/cancel/rwlock/once 组合覆盖，剩余按具体未计分项再拆。
- [x] item 级运行：当前 `sdcard-rv.img` 未打包 `pthread_mutex_pi`；用临时编译的 `src/functional/pthread_mutex_pi.c` 静态 RISC-V musl 二进制，经 guest `/tmp` base64 注入后运行无 `t_error` 输出。
- [x] item 级运行：当前 `sdcard-rv.img` 未打包 `pthread_robust`；用临时编译的 `src/functional/pthread_robust.c` 静态 RISC-V musl 二进制，经 guest base64 注入后运行并返回 `ROBUST_DONE:0`，无 `t_error` 输出。
- [x] 回归：最新 scorer 中 `basic-musl = 102 / 102`、`busybox-musl = 54 / 55`、`lua-musl = 9 / 9`。
- [x] 完整组：最新 scorer 中 `libctest-musl = 217 / 220`，已跑过 pthread 段并到 END marker。

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

### 已接入但仍需深语义校准

- [x] `chroot` 已有 syscall 入口和 cwd/root 状态更新；capability 与目录对象细节仍用 `// UNFINISHED:` 标注。
- [x] `openat2` 已有 Linux syscall 号和 `sys_openat2_ctx` 路径；完整 `RESOLVE_*` 约束仍按 VFS 深语义跟进。
- [x] symlink traversal / nofollow 主路径已接入，路径解析有 max-depth 保护。
- [ ] mounted root 的 `..` 语义仍是临时行为：当前没有记录 covered directory 的父目录。

## P2.6 - VFS 稳健化路线图

### 阶段 0：冻结事实与回归用例

- [x] 文件系统调用链已记录在 `develop-guide/current-filesystem-reading-tutorial.md`。
- [x] BusyBox pipeline / benchmark 并发触发 `UPIntrFreeCell` borrow panic 的现象已记录在 `develop-guide/contest-full-test-run-2026-04-27.md`。
- [x] 已确认 `UPIntrFreeCell` 不适合包住可能阻塞、可能 schedule 的长临界区对象。
- [x] 最新 scorer 已覆盖 BusyBox pipeline 正例，旧的 pipeline 借用 panic 未再复现。
- [x] 最新 scorer 已覆盖 `/musl/basic/pipe`，日志包含 `Write to pipe successfully.`。
- [x] 本地评分 runner 已带 timeout 和 raw log 留存，可区分死锁、panic 和正常退出。

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
- [x] 完整 `libctest-musl` 已重跑确认，最新 `217 / 220`；旧阻塞点 `rt_sigtimedwait(137)`、`prlimit64(261)`、`lseek(62)` 不再作为 ENOSYS TODO。

2026-05-01 阶段 4 验证记录：

- `make fmt`：通过。
- `make all`：通过，产出 `kernel-rv` 和 `kernel-la`。
- `CARGO_NET_OFFLINE=true make all`：通过。
- `make run-rv` 手工验收：`/tmp` 上 create/write/read/mkdir/readdir/unlink/rmdir 均成功；`busybox mktemp` / `mktemp -d` 均成功。
- `tools/contest_runner/run_groups.py --arch rv --libcs musl --groups busybox --out develop-guide/test-run-logs/2026-05-01-phase4/busybox-rv-musl --no-build`：`busybox-musl` end-seen，保持 `49 / 55`，未退化。
- `tools/contest_runner/run_groups.py --arch rv --libcs musl --groups libctest --out develop-guide/test-run-logs/2026-05-01-phase4/libctest-rv-musl --no-build`：未到 END，`0 / 220`；这是旧记录，里面的 `rt_sigtimedwait` / `getrlimit` / `lseek` ENOSYS 结论已过期。

### 阶段 5：devfs 迁移为 VFS backend（LOW）

**目标**：将 devfs 从 VFS 前拦截改为标准 mount，统一路径解析语义。

- [x] 重构 `os/src/fs/devfs.rs`，实现 `FileSystemBackend` trait。
  - `lookup_component_from`：root 下匹配 "null"/"zero"/"tty"/"ttyS0"/"random"/"urandom"。
  - `read_at` / `write_at`：根据 ino 分发到 UART / null / zero 逻辑。
  - `stat`：返回 `S_IFCHR` + 正确的 rdev。
- [x] 在 `init_mounts()` 中挂载 devfs 到 `/dev`。
- [x] `open_file_at` / `stat_at` 主路径已走 mounted `/dev`；当前仍保留少量 dirfd child helper 兼容 devfs 子目录。
- [x] latest scorer 中依赖 `/dev/null`、`/dev/zero`、random/urandom 的 basic/busybox/libctest 主路径未回退。

### 阶段 6：正规化路径解析与 mount crossing

- [x] 消除 `os/src/fs/vfs/path.rs` 对 `EXT4_ROOT_INO` 的残余依赖。
- [ ] 修复 mounted root 下 `..` 的语义：精确匹配 mount instance 而非 `rposition`。
- [x] 实现 symlink traversal（max depth = 40）：
  - 非 final component 的 symlink 必须 follow。
  - final component 根据 `O_NOFOLLOW` / `AT_SYMLINK_NOFOLLOW` 决定。
- [ ] 验证：跨 mount symlink、`..` 穿越 mount boundary。

### 阶段 7：补 Linux VFS 关键语义

- [x] `openat` 的 `O_CREAT|O_EXCL`、`O_TRUNC`、`O_APPEND`、`O_DIRECTORY` 主路径已支撑 latest basic/busybox/libctest；`O_NOFOLLOW` / `openat2 RESOLVE_*` 深语义继续按 case 校准。
- [x] `mkdirat/unlinkat/rmdir` 的 basic/busybox errno 主路径已通过 latest scorer。
- [x] `newfstatat/fstat/lstat` 的 basic/libctest 主路径已通过 latest scorer；设备号、时间戳、nlink 的 Linux 细节继续按 LTP/VFS 项补。
- [x] `getdents64` 的 basic/busybox 主路径已通过 latest scorer；offset 稳定性和跨 mount 边界继续按 LTP/VFS 项补。
- [x] `mount/umount2` 已支持 `fstype` 参数（"ext4"/"vfat"/"fat32"/"fat"/"tmpfs"/"ramfs"）和 busy target 基础检查。
- [ ] 继续完善 `mount/umount2` 的比赛语义：FAT/VFAT 真实行为、umount 后状态、分区错误码和 fallback 策略。
- [x] 已知不完整语义持续用 `// UNFINISHED:` 标出，避免把兼容路径误写成完整 Linux 语义。

### 阶段 8：缓存与性能

- [x] 已落地 VFS node / dentry cache 体系；单独 Linux-grade inode cache 暂不作为当前评分阻塞。
- [x] 已落地正向 dentry cache，mount / create / unlink / rename 等路径会清理相关 cache。
- [x] 已落地 page cache 和 block cache，服务 ELF/mmap、顺序读和块设备读写热点。
- [x] cache 失效路径已覆盖 mount change、create/unlink/rename/truncate/write 等主路径。
- [x] procfs/tmpfs 仍走纯内存后端；cache 支持按 mount/backend capability 控制。

### 阶段 9：验收门槛

- [x] `make fmt`。
- [x] `make all`。
- [x] `CARGO_NET_OFFLINE=true make all`。
- [x] `make run-rv` / latest scorer 下 `/musl/basic_testcode.sh` 通过，`basic-musl = 102 / 102`。
- [x] BusyBox pipeline 主路径已通过 latest scorer，未再复现旧 `UPIntrFreeCell` borrow panic。
- [x] `busybox-musl` 完整脚本打印 END marker。
- [x] `busybox-musl` 中 `df`、`free`、`ps`、`uptime` 命令通过（依赖 procfs）。
- [x] `libctest-musl` 中 `mkstemp` / `mkdtemp` 相关 failure 用例通过，完整组最新 `217 / 220`。
- [x] `basic-musl` 文件系统相关用例全部通过，`basic-musl = 102 / 102`。
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

当前状态：以上 5 步已完成。`busybox-musl` 已从早期 `43 / 55` 提升到 latest `54 / 55`，剩余 1 分单独跟踪。

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

当前状态：tmpfs backend 和 `/tmp` 挂载已完成；`busybox mktemp` / `mktemp -d` / `touch` 已验证。完整 `libctest-musl` 已在新版 `lseek`、`rt_sigtimedwait`、`prlimit64`、`utimensat` 后重跑，最新为 `217 / 220`。

## P2.6.5 - 可选 FAT/VFAT 支持路线图

- [x] 采用 vendored `fatfs`，`os/Cargo.toml` 已通过 `vendor/crates/fatfs` 离线引入。
- [x] 为 WHUSP 块设备实现 `fatfs::Read` / `fatfs::Write` / `fatfs::Seek` 适配层。
- [x] 在 `os/src/fs/fat.rs` 新增 FAT mount wrapper。
- [x] 泛化当前 mount 表，让它能同时承载 EXT4、FAT 和伪文件系统 mount。
- [x] 在 `sys_mount` 中接受 `fstype == "vfat"` / `"fat32"` / `"fat"`。
- [x] 支持 `/dev/vdXN` 分区源解析，basic mount 测例不再卡在无法定位 FAT 分区。
- [x] 首轮不承诺 symlink、Unix owner/mode、hard link、完整时间戳和大小写规则；缺口已用 `// UNFINISHED:` 标明。
- [x] FAT/VFAT adapter 已接入 lookup/read/create/write/remove 主路径；latest basic 中 `/musl/basic/mount` 和 `/musl/basic/umount` 已通过。

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
- [x] `busybox-musl` 的 shell、pipe、pipeline、重定向主路径已可跑完整脚本到 END；`which ls`、`hwclock` 和 `sleep & kill $!` 已通过，当前只剩 `busybox kill 10` 评分项需单独跟踪。
- [x] `lua-musl` 所需的 mmap/brk/fs/signal/FPU/用户栈主路径已完成，`lua-musl` 9/9。
- [x] 推进 `libctest-musl` 的工作目录、脚本布局和动态链接运行时；`entry-static.exe` / `entry-dynamic.exe` 主线均已进入 latest scorer。
- [x] 验证 musl 动态链接路径支持；`libctest-musl` dynamic 用例大量通过。
- [x] 推进 glibc 变体运行；`basic-glibc = 102 / 102`、`busybox-glibc = 54 / 55`、`lua-glibc = 9 / 9`。
- [x] 验证 glibc 动态链接路径支持；`basic-glibc` / `busybox-glibc` / `lua-glibc` 已跑到 marker。
- [x] `/glibc/basic_testcode.sh` 在当前代码上重新跑到 marker，并刷新为 `102 / 102`。
- [ ] `libctest-glibc` 当前仍跳过或未计分，后续如需冲总分再纳入默认 runner。

## P4 - 性能与压力测试

- [x] 记录 EXT4 phase 1 的 `huge_write` 性能回退：约 256 KiB/s，对比旧 `easy-fs` 约 549 KiB/s。
- [x] 分析 `huge_write` / iozone / lmbench 相关热点，记录在 `develop-guide/kernel-performance-deep-scan-2026-06-04.md`。
- [x] 已推进多轮 EXT4 / VFS / block-cache / read-cache / syscall-context 性能切片；具体 stage 记录在 `develop-guide/`。
- [ ] 推进 `iozone`。
- [ ] 推进 `lmbench`。
- [ ] 推进 `iperf`。
- [ ] 推进 `netperf`。
- [x] `cyclictest` 在当前 RV narrowed runner 中推进到 glibc/musl 四个子项均 `end: success`，且两组均 `kill hackbench: success`；记录见 `develop-guide/test-run-logs/2026-05-10-cyclictest/after-stack-window-256k-420s.raw.log`。
- [x] 推进 LTP runner、whitelist、case-script 生成和多个 focused case family 修复。
- [ ] 默认 scorer 中 `ltp-*`、`iozone-*`、`iperf-*`、`lmbench-*`、`netperf-*`、`libcbench-*` 仍为 0，需要按组继续冲分。

## P5 - LoongArch

### 阶段 0：冻结采用路线

- [x] 采用内置 `arch/` 拆分作为主线，吸收 `NighthawkOS` 的小 HAL facade 组织方式。
- [x] `polyhal` 只作为设计/代码参考，不接入完整 runtime。
- [x] 复查并落地主要借用点：LoongArch `_start`、DMW/MMU 初始化、TLB refill、CSR timer、GED shutdown、virtio-pci 块设备、syscall register ABI。

### 阶段 1：先做 RISC-V 行为不变的架构拆分

- [x] 新增 `os/src/arch.rs`，用 `#[cfg(target_arch = ...)]` 选择 `riscv64` / `loongarch64`。
- [x] 新增 `os/src/arch/riscv64/`，迁入当前低层入口、trap、timer、SBI、board。
- [x] 让 generic kernel 只通过 `crate::arch` 调用低层入口。
- [x] 保持当前 RISC-V 启动契约不变。
- [x] 验证 `make fmt`、`make all`、`CARGO_NET_OFFLINE=true make all`、`make run-rv`。

### 阶段 2A：LoongArch 构建入口骨架

- [x] 根 `Makefile` 新增 `kernel-la` 目标，并通过 `os/ ARCH=loongarch64` 构建根目录 `kernel-la`。
- [x] 根 `Makefile` 新增 `run-la` 目标，当前把 `sdcard-la.img` 作为 `PRIMARY_DISK / x0`，并把生成脚本盘作为 `AUX_DISK / x1` 传给 `os/ run-inner`。
- [x] `os/Makefile` 支持 `ARCH=loongarch64` 的 target/QEMU/virtio-pci 变量，并使用真实 `kernel` 构建入口。
- [x] `user/Makefile` 对 `ARCH=loongarch64` 明确失败，说明用户态 syscall wrapper 属于阶段 5。
- [x] `rust-toolchain.toml` 纳入 `loongarch64-unknown-none` target。
- [x] 验证 `make kernel-la` / `make run-la` 不再是缺目标；后续仍需按测试场景验证 LoongArch 比赛运行能力。

### 阶段 2B：真正产出 `kernel-la`

- [x] 根 `make all` 同时产出 `kernel-rv` 和 `kernel-la`。
- [x] 新增 LoongArch linker script，不复用 RISC-V `linker-qemu.ld`。
- [x] `os/Makefile` 的 `ARCH=loongarch64 kernel` 已是真实构建。
- [x] `make validation` 会构建 `kernel-la`，本地离线 vendor 路径已覆盖 LoongArch 构建。
- [ ] 在官方 contest Docker 中单独复核 LoongArch 运行/评分链路。

### 阶段 3：LoongArch 最小内核可启动

- [x] 复核并补齐 `arch/loongarch64/entry` 的比赛运行路径。
- [x] 复核并补齐 `arch/loongarch64/console` 的串口输出/输入路径。
- [x] 复核并补齐 `arch/loongarch64/shutdown` 的 GED poweroff 路径。
- [x] 复核并补齐 `arch/loongarch64/time` 的定时器路径。
- [x] 复核并补齐 `arch/loongarch64/trap` 的异常、syscall 和返回路径。
- [x] 复核并补齐 `arch/loongarch64/mm` 的 DMW、TLB 和页表切换路径。
- [x] 复核并补齐 `arch/loongarch64/context` / `switch.S` 的上下文切换路径。
- [ ] 验证 QEMU LoongArch 能稳定跑官方评测盘脚本，并能主动 shutdown。

### 阶段 4：LoongArch 设备与文件系统路径

- [x] 明确 QEMU LoongArch virt 设备模型：块设备优先走 PCI virtio。
- [x] 接入 LoongArch PCI/virtio block 发现，至少识别 `x0 = sdcard-la.img`。
- [x] 保持文件系统上层接口不分叉。
- [ ] 验证从 `sdcard-la.img` 挂载根目录并读取 `/musl`、`/glibc`、测试脚本。
- [x] LoongArch QEMU 命令已接入 optional `x1` 辅助盘参数。
- [ ] 验证 optional `x1` 辅助盘在 LoongArch guest 内的动态挂载和脚本执行路径。

### 阶段 5：LoongArch 用户态与 submit runner

- [x] ELF / vDSO / loader 路径已纳入 LoongArch 架构支持，包含 `EM_LOONGARCH` 和 LoongArch 动态链接器路径。
- [x] 对齐 LoongArch 用户态入口、栈、TLS、syscall 返回值、errno 负值约定的代码路径。
- [x] 补齐 LoongArch musl / glibc BusyBox 启动所需的动态链接器兼容路径。
- [x] 泛化 submit runner，让同一套脚本盘能按 `basic-musl` / `busybox-musl` / `glibc` 等组名输出精确 marker。
- [ ] 验证 `submit-la` 或等价入口按固定顺序串行执行测试组，并在结束后主动 shutdown。

### 阶段 6：LoongArch 验收门槛

- [x] `make fmt`。
- [x] `make kernel-rv`。
- [x] `make kernel-la`。
- [x] `make all` / `make validation`。
- [x] `CARGO_NET_OFFLINE=true make all`。
- [x] `make run-rv` 不回退；最新 RISC-V scorer 为 `547 / 1164`。
- [x] `make run-la` 有真实 QEMU 启动入口并会构建脚本盘。
- [ ] `make run-la` / `--arch la` 完成目标测试场景并产生 guest marker；最新 scorer 仍为 `0 / 1160`。
- [x] 官方 contest Docker 中验证 `kernel-rv`、`kernel-la` 产物名正确。

## 基础设施与研究记录

- [x] 建立官方 QEMU 启动命令的本地复现脚本，CI `ci-riscv-smoke.yml` 只做 smoke：确认 initproc 启动并看到 `basic-musl` 的 START/END marker。
- [x] 建立官方容器里的 smoke test 脚本。
- [x] 建立 basic 用例到 syscall 的逐项对照表：`develop-guide/linux-syscall-implementation-survey.md`。
- [x] 对比 `RustOsWhu` / `NighthawkOS` 的提交路径并提炼可复用做法：`develop-guide/reference-project-notes.md`。
- [x] 评估 EXT4 方案的许可证、维护成本和提交打包方式：`develop-guide/lwext4-rust-research.md` 和 `develop-guide/ext4-phase1-migration-and-validation.md`。
- [x] 建立更适合比赛开发的 GitHub CI。
- [x] 升级 dependencies，并保留离线 vendor 路线。
