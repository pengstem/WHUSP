# OSKernel2026 开发任务清单

更新时间：2026-04-30

## 快照

- [x] 根目录 `make all` 已作为提交构建入口，当前目标是同时产出根目录 `kernel-rv` 和 `kernel-la`。
- [x] 根 `Makefile` 的 `kernel-rv` 通过 `os/ ARCH=riscv64` 构建，`kernel-la` 通过 `os/ ARCH=loongarch64` 构建。
- [x] 当前仓库已 vendoring Cargo 依赖到 `vendor/crates`，并通过 `vendor/config.toml` 支持离线构建。
- [x] 2026-04-28 移除默认 `user/` / `disk.img` 链路后，本地重新验证 `CARGO_NET_OFFLINE=true make all` 成功。
- [x] `make run-rv` 是默认 RISC-V 比赛形态启动：`x0 = sdcard-rv.img`，当前不传 `CONTEST_AUX_DISK` / `AUX_DISK`，也不挂载 `x1`。
- [x] `make run-la` 已有入口：`x0 = sdcard-la.img`。这只表示有构建/启动入口，不代表 LoongArch 比赛运行已经完整可用。
- [x] 内核当前直接从评测盘加载 `/musl/busybox sh` 作为 initproc。
- [x] 已有一次 RISC-V 全量手工运行记录：`develop-guide/contest-full-test-run-2026-04-27.md`。这次是主机注入命令，不是最终 submit runner。
- [x] `basic-musl` 在该记录中能跑到 END marker，官方 `judge_basic.py` 结果是 `55 / 102`。
- [x] 在 2026-04-27 全量手工运行记录之后，源码又接入了 `times(153)`、`mprotect(226)`、`nanosleep(101)`、`clock_nanosleep(115)`、`gettimeofday(169)`、`uname(160)` 等修复，旧的跑分记录需要重跑刷新。
- [ ] LoongArch 运行验证、submit runner、全组串行执行、自动 marker 管理、结束后主动关机。

## 判断

1. 最高优先级仍是 P0 submit runner 闭环。没有它，每次测试都依赖主机注入命令，不能稳定复现比赛提交形态。
2. P2.6 的 VFS/EXT4 锁问题是第二优先级。它会同时影响 BusyBox pipeline、UnixBench、Lmbench、LTP 和重复 exec。
3. `basic-musl` 还能短线补分，但要避免把 `/dev/vda2`、vfat、VFS 并发、FAT 支持混成一个大改。
4. `mprotect` 已经接入源码，glibc 的旧失败结论需要重新验证；如果 glibc 仍失败，再从动态加载器日志继续收窄。
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
- [x] `umount2(39)`，当前仅支持严格的动态 EXT4 mount 范围。
- [x] `mount(40)`，当前仅支持 whole-disk ext4，例如 `/dev/vda`。
- [x] `chdir(49)`。
- [x] `openat(56)` 基础路径、dirfd、目录 fd 能力。
- [x] `pipe2(59)`，使用 Linux `int[2]` ABI，支持 `O_NONBLOCK` / `O_CLOEXEC`。
- [x] `getdents64(61)`。
- [x] `readv(65)` 和 `writev(66)`。
- [x] `ppoll(73)` 最小 BusyBox 兼容。
- [x] `newfstatat(79)` 和 `fstat(80)` 基础实现。
- [x] `exit_group(94)` 单线程兼容实现。
- [x] `waitid(95)` 基础实现。
- [x] `nanosleep(101)` 和 `clock_nanosleep(115)` 基础实现。
- [x] `times(153)` 源码已接入，使用当前进程 CPU time 快照。
- [x] `uname(160)` 源码已接入，返回 Linux 风格 `utsname`。
- [x] `gettimeofday(169)` 源码已接入，当前基于 monotonic timer。
- [x] `getppid(173)`。
- [x] `brk(214)`。
- [x] `mmap(222)`、`munmap(215)`、`mprotect(226)`。
- [x] `clone(220)`、`execve(221)`、`wait4(260)` 基础路径。
- [x] `execve` 支持 `argv/envp`，并支持 shebang 脚本解释器重写。
- [x] 官方评测盘无 `/bin/sh` 时，脚本解释器可 fallback 到 `/musl/busybox` 或 `/glibc/busybox`。
- [x] fd table 已有 `FdTableEntry`，区分 fd flags 和 file status flags，并支持 close-on-exec。

### 已验证的 `basic-musl` 结果

- [x] 可通过 `cd /musl && ./busybox sh ./basic_testcode.sh` 手工跑到 END marker。
- [x] 上次全量记录中 `basic-musl` 官方 judge 为 `55 / 102`。
- [x] 上次记录中已经拿满的 basic 子项：`brk`、`chdir`、`clone`、`close`、`dup`、`fork`、`fstat`、`getcwd`、`getppid`、`mkdir`、`uname`、`yield`。
- [ ] 2026-04-27 全量记录之后新增的 `times`、`mprotect`、`nanosleep` 等实现尚未重新跑完整 judge。

### 当前 `basic-musl` 剩余缺口

- [ ] `dup2` / `dup3` 兼容细节：上次 judge 中 `test_dup2` 为 `0 / 2`。
- [ ] `times(153)` 需要重跑 judge；源码已接入，但旧日志仍是 `0 / 6`。
- [ ] `gettimeofday(169)` 需要补齐 judge 细粒度断言，旧结果是 `1 / 3`。
- [ ] `mount(40)` / `umount2(39)` 的比赛测试语义未完成：`/dev/vda2` 分区源和 `vfat` 均未支持。
- [ ] `wait4/waitpid` 的 status、options、rusage 细节仍不足。
- [ ] `openat` 的 `flags/mode/O_CREAT/O_DIRECTORY/O_APPEND/O_TRUNC/O_EXCL/O_NOFOLLOW` 仍需补齐。
- [ ] `newfstatat/fstat/lstat` 的目录、pipe、stdio、设备号、时间戳、nlink 等细节仍需审计。
- [ ] `getdents64` 的 offset 稳定性、跨 mount readdir、buffer 边界还需补齐。
- [ ] `pipe` 的阻塞、非阻塞、关闭端、错误码等 Linux 细节还不完整。
- [ ] 最小 `/dev/null` / devfs 尚未实现，影响 `netperf`、`iperf`、LTP 等 harness。
- [ ] `sys_kill(129)` 仍把 Linux signum 当 bitflags 解析，需要按整数信号号修正。

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

### 未完成

- [ ] `fchdir(50)`。
- [ ] `readlinkat`。
- [ ] `faccessat`。
- [ ] `renameat2`。
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

### 阶段 2：拆出最小 VFS 对象层

- [ ] 新建 `os/src/fs/vfs/`，先只承载类型与转发逻辑，不改用户可见语义。
- [ ] 定义 `VfsNodeId { mount_id, ino }`。
- [ ] 定义 `VfsPath { node, kind }`。
- [ ] 定义 `VfsFile { node, offset, readable, writable, status_flags }`。
- [ ] 保留现有 `File` trait 作为 fd table 对外接口。
- [ ] 将 `open_file_at/stat_at/lookup_dir_at` 改成调用 VFS 层。
- [ ] 每次迁移后跑 `make all`、默认单盘 `make run-rv`、contest-style basic 文件系统抽测。

### 阶段 3：正规化路径解析与 mount crossing

- [ ] 把 `path.rs` 的 `PathCursor` 迁到 VFS 层。
- [ ] 明确处理覆盖目录和被挂载文件系统根目录的关系。
- [ ] 记录 mounted root 的父目录信息，修复 mounted root 下 `..` 的临时行为。
- [ ] 区分普通 lookup、mount target lookup、create parent lookup。
- [ ] 为 symlink 预留接口；未实现处保留 `// UNFINISHED:`。
- [ ] 验证绝对路径、相对路径、`..`、`/x1`、mount target、unmount 后路径行为。

### 阶段 4：把 EXT4 后端收敛成 VFS backend trait

- [ ] 定义 `FileSystemBackend` trait：`lookup/read/write/stat/create/unlink/readdir`。
- [ ] 让 `Ext4Mount` 实现 backend trait。
- [ ] 将 backend 锁放在 mount 实例内部。
- [ ] 为后续 `tmpfs/devfs/procfs` 预留 backend 注册点。
- [ ] 验证现有 `File` trait 对 fd table 的行为不变。

### 阶段 5：补 Linux VFS 关键语义

- [ ] 建立 VFS/FS 错误传播模型：将 pathname/create/unlink/rmdir 等接口从 `Option` 折叠错误逐步迁到 `Result<_, FsError>` 或等价类型，并在 syscall 边界统一映射为 Linux `errno`。
- [ ] 完善 `openat`。
- [ ] 完善 `mkdirat/unlinkat/rmdir` 的 Linux errno：区分 `EEXIST`、`ENOENT`、`ENOTDIR`、`EISDIR`、`EINVAL`、`ENOTEMPTY`、`EBUSY`、`EIO` 等场景，并补齐 `unlinkat(AT_REMOVEDIR)` 到 rmdir 语义的分流。
- [ ] 完善 `newfstatat/fstat/lstat`。
- [ ] 完善 `getdents64`。
- [ ] 完善 `renameat2/linkat/symlinkat/readlinkat/faccessat/fchdir`。
- [ ] 完善 `mount/umount2` 的 busy target、mounted root、相对路径、错误码。
- [ ] 所有不完整语义必须用 `// UNFINISHED:` 标出具体 Linux 缺口。

### 阶段 6：缓存与性能

- [ ] 语义稳定后再加 inode cache，cache key 使用 `(mount_id, ino)`。
- [ ] 加正向 dentry cache；负向 cache 等 rename/unlink 语义稳定后再考虑。
- [ ] 加简单 page/block cache，先服务 ELF 加载、顺序读、`getdents64`。
- [ ] 为 cache 加失效路径：`create/unlink/rename/truncate/write`。
- [ ] 对比 `huge_write`、BusyBox pipeline、重复 exec BusyBox 的性能与正确性。

### 阶段 7：验收门槛

- [ ] `make fmt`。
- [ ] `make all`。
- [ ] `CARGO_NET_OFFLINE=true make all`。
- [ ] `make run-rv` 下执行 `/musl/basic_testcode.sh` 或等价 BusyBox shell 包装。
- [ ] pipeline 复现不 panic，重复运行 5 次不死锁。
- [ ] `busybox-musl` 完整脚本能打印 END marker；如果重新打开 RV nonblocking，同一回归必须继续通过。
- [ ] LA 的 nonblocking block I/O 不作为当前门槛，等 virtio-pci IRQ / 外部中断路径单独验收。
- [ ] `basic-musl` 文件系统相关用例全部通过。

## P2.6.5 - 可选 FAT/VFAT 支持路线图

- [ ] 采用 `starry-fatfs` 作为首选 FAT 库，导入 crate 名仍按 `fatfs` 使用。
- [ ] vendoring 前复核离线构建，把 FAT 依赖纳入 `vendor/crates`。
- [ ] 为 WHUSP 块设备实现 `fatfs::Read` / `fatfs::Write` / `fatfs::Seek` 适配层。
- [ ] 在 `os/src/fs` 新增 FAT mount wrapper。
- [ ] 泛化当前 mount 表，让它能同时承载 EXT4 与 FAT。
- [ ] 在 `sys_mount` 中接受 `fstype == "vfat"` / `"fat32"`。
- [ ] 先补 `/dev/vda2` 分区源解析，否则 basic mount 测例仍无法定位 FAT 分区。
- [ ] 首轮不承诺 symlink、Unix owner/mode、hard link、完整时间戳和大小写规则；缺口用 `// UNFINISHED:` 标明。
- [ ] 验证顺序：FAT32 镜像只读 lookup/read，create/write/read/remove，最后跑 `/musl/basic/mount` 和 `/musl/basic/umount`。

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
- [ ] 重跑 glibc 组，确认旧的 `cannot apply additional memory protection after relocation` 是否消失。
- [ ] 推进 `busybox-musl` 的 shell、pipe、pipeline、重定向语义。
- [ ] 推进 `lua-musl` 所需的 mmap/brk/fs/signal 兼容性。
- [ ] 推进 `libctest-musl` 的工作目录、脚本布局和动态链接运行时。
- [ ] 补齐 `/lib/ld-musl-riscv64.so.1` 路径支持。
- [ ] 推进 glibc 变体运行。
- [ ] 补齐 `/lib/ld-linux-riscv64-lp64d.so.1` 路径支持。
- [ ] 让 `/glibc/basic_testcode.sh` 可以进入真实测试主体并跑到 marker。

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

- [x] 建立官方 QEMU 启动命令的本地复现脚本，CI `ci-riscv-smoke.yml` 覆盖。
- [x] 建立官方容器里的 smoke test 脚本。
- [x] 建立 basic 用例到 syscall 的逐项对照表：`develop-guide/linux-syscall-implementation-survey.md`。
- [x] 对比 `RustOsWhu` / `NighthawkOS` 的提交路径并提炼可复用做法：`develop-guide/reference-project-notes.md`。
- [x] 评估 EXT4 方案的许可证、维护成本和提交打包方式：`develop-guide/lwext4-rust-research.md` 和 `develop-guide/ext4-phase1-migration-and-validation.md`。
- [x] 建立更适合比赛开发的 GitHub CI。
- [x] 升级 dependencies，并保留离线 vendor 路线。
