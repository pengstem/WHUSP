// Linux generic syscall numbers used by both contest RISC-V and LoongArch
// ABIs. Keep this table aligned with the userspace libc headers, not with
// local implementation order.
const SYSCALL_IO_SETUP: usize = 0;
const SYSCALL_IO_DESTROY: usize = 1;
const SYSCALL_IO_SUBMIT: usize = 2;
const SYSCALL_IO_CANCEL: usize = 3;
const SYSCALL_IO_GETEVENTS: usize = 4;
const SYSCALL_SETXATTR: usize = 5;
const SYSCALL_LSETXATTR: usize = 6;
const SYSCALL_FSETXATTR: usize = 7;
const SYSCALL_GETXATTR: usize = 8;
const SYSCALL_LGETXATTR: usize = 9;
const SYSCALL_FGETXATTR: usize = 10;
const SYSCALL_LISTXATTR: usize = 11;
const SYSCALL_LLISTXATTR: usize = 12;
const SYSCALL_FLISTXATTR: usize = 13;
const SYSCALL_REMOVEXATTR: usize = 14;
const SYSCALL_LREMOVEXATTR: usize = 15;
const SYSCALL_FREMOVEXATTR: usize = 16;
const SYSCALL_GETCWD: usize = 17;
const SYSCALL_EVENTFD2: usize = 19;
const SYSCALL_EPOLL_CREATE1: usize = 20;
const SYSCALL_EPOLL_CTL: usize = 21;
const SYSCALL_EPOLL_PWAIT: usize = 22;
const SYSCALL_DUP: usize = 23;
const SYSCALL_DUP3: usize = 24;
const SYSCALL_FCNTL: usize = 25;
const SYSCALL_INOTIFY_INIT1: usize = 26;
const SYSCALL_INOTIFY_ADD_WATCH: usize = 27;
const SYSCALL_INOTIFY_RM_WATCH: usize = 28;
const SYSCALL_IOCTL: usize = 29;
const SYSCALL_FLOCK: usize = 32;
const SYSCALL_MKNODAT: usize = 33;
const SYSCALL_MKDIRAT: usize = 34;
const SYSCALL_UNLINKAT: usize = 35;
const SYSCALL_SYMLINKAT: usize = 36;
const SYSCALL_LINKAT: usize = 37;
const SYSCALL_UMOUNT2: usize = 39;
const SYSCALL_MOUNT: usize = 40;
const SYSCALL_STATFS: usize = 43;
const SYSCALL_FSTATFS: usize = 44;
const SYSCALL_TRUNCATE: usize = 45;
const SYSCALL_FTRUNCATE: usize = 46;
const SYSCALL_FALLOCATE: usize = 47;
const SYSCALL_FACCESSAT: usize = 48;
const SYSCALL_CHDIR: usize = 49;
const SYSCALL_FCHDIR: usize = 50;
const SYSCALL_CHROOT: usize = 51;
const SYSCALL_FCHMOD: usize = 52;
const SYSCALL_FCHMODAT: usize = 53;
const SYSCALL_FCHOWNAT: usize = 54;
const SYSCALL_FCHOWN: usize = 55;
const SYSCALL_OPENAT: usize = 56;
const SYSCALL_CLOSE: usize = 57;
const SYSCALL_PIPE2: usize = 59;
const SYSCALL_QUOTACTL: usize = 60;
const SYSCALL_GETDENTS64: usize = 61;
const SYSCALL_LSEEK: usize = 62;
const SYSCALL_READ: usize = 63;
const SYSCALL_WRITE: usize = 64;
const SYSCALL_READV: usize = 65;
const SYSCALL_WRITEV: usize = 66;
const SYSCALL_PREAD64: usize = 67;
const SYSCALL_PWRITE64: usize = 68;
const SYSCALL_PREADV: usize = 69;
const SYSCALL_PWRITEV: usize = 70;
const SYSCALL_SENDFILE: usize = 71;
const SYSCALL_PSELECT6: usize = 72;
const SYSCALL_PPOLL: usize = 73;
const SYSCALL_SIGNALFD4: usize = 74;
const SYSCALL_SPLICE: usize = 76;
const SYSCALL_READLINKAT: usize = 78;
const SYSCALL_NEWFSTATAT: usize = 79;
const SYSCALL_FSTAT: usize = 80;
const SYSCALL_SYNC: usize = 81;
const SYSCALL_FSYNC: usize = 82;
const SYSCALL_FDATASYNC: usize = 83;
const SYSCALL_TIMERFD_CREATE: usize = 85;
const SYSCALL_UTIMENSAT: usize = 88;
const SYSCALL_CAPGET: usize = 90;
const SYSCALL_CAPSET: usize = 91;
const SYSCALL_PERSONALITY: usize = 92;
const SYSCALL_EXIT: usize = 93;
const SYSCALL_EXIT_GROUP: usize = 94;
const SYSCALL_WAITID: usize = 95;
const SYSCALL_SET_TID_ADDRESS: usize = 96;
const SYSCALL_UNSHARE: usize = 97;
const SYSCALL_FUTEX: usize = 98;
const SYSCALL_SET_ROBUST_LIST: usize = 99;
const SYSCALL_GET_ROBUST_LIST: usize = 100;
const SYSCALL_NANOSLEEP: usize = 101;
const SYSCALL_GETITIMER: usize = 102;
const SYSCALL_SETITIMER: usize = 103;
const SYSCALL_INIT_MODULE: usize = 105;
const SYSCALL_DELETE_MODULE: usize = 106;
const SYSCALL_TIMER_CREATE: usize = 107;
const SYSCALL_TIMER_GETTIME: usize = 108;
const SYSCALL_TIMER_GETOVERRUN: usize = 109;
const SYSCALL_TIMER_SETTIME: usize = 110;
const SYSCALL_TIMER_DELETE: usize = 111;
const SYSCALL_CLOCK_SETTIME: usize = 112;
const SYSCALL_CLOCK_GETTIME: usize = 113;
const SYSCALL_CLOCK_GETRES: usize = 114;
const SYSCALL_CLOCK_NANOSLEEP: usize = 115;
const SYSCALL_SYSLOG: usize = 116;
const SYSCALL_PTRACE: usize = 117;
const SYSCALL_SCHED_SETPARAM: usize = 118;
const SYSCALL_SCHED_SETSCHEDULER: usize = 119;
const SYSCALL_SCHED_GETSCHEDULER: usize = 120;
const SYSCALL_SCHED_GETPARAM: usize = 121;
const SYSCALL_SCHED_SETAFFINITY: usize = 122;
const SYSCALL_SCHED_GETAFFINITY: usize = 123;
const SYSCALL_SCHED_YIELD: usize = 124;
const SYSCALL_SCHED_GET_PRIORITY_MAX: usize = 125;
const SYSCALL_SCHED_GET_PRIORITY_MIN: usize = 126;
const SYSCALL_SCHED_RR_GET_INTERVAL: usize = 127;
const SYSCALL_KILL: usize = 129;
const SYSCALL_TKILL: usize = 130;
const SYSCALL_TGKILL: usize = 131;
const SYSCALL_SIGALTSTACK: usize = 132;
const SYSCALL_RT_SIGSUSPEND: usize = 133;
const SYSCALL_RT_SIGACTION: usize = 134;
const SYSCALL_RT_SIGPROCMASK: usize = 135;
const SYSCALL_RT_SIGPENDING: usize = 136;
const SYSCALL_RT_SIGTIMEDWAIT: usize = 137;
const SYSCALL_RT_SIGRETURN: usize = 139;
const SYSCALL_SETPRIORITY: usize = 140;
const SYSCALL_GETPRIORITY: usize = 141;
const SYSCALL_REBOOT: usize = 142;
const SYSCALL_SETREGID: usize = 143;
const SYSCALL_SETGID: usize = 144;
const SYSCALL_SETREUID: usize = 145;
const SYSCALL_SETUID: usize = 146;
const SYSCALL_SETRESUID: usize = 147;
const SYSCALL_GETRESUID: usize = 148;
const SYSCALL_SETRESGID: usize = 149;
const SYSCALL_GETRESGID: usize = 150;
const SYSCALL_SETFSUID: usize = 151;
const SYSCALL_SETFSGID: usize = 152;
const SYSCALL_TIMES: usize = 153;
const SYSCALL_SETPGID: usize = 154;
const SYSCALL_GETPGID: usize = 155;
const SYSCALL_GETSID: usize = 156;
const SYSCALL_SETSID: usize = 157;
const SYSCALL_GETGROUPS: usize = 158;
const SYSCALL_SETGROUPS: usize = 159;
const SYSCALL_UNAME: usize = 160;
const SYSCALL_GETRLIMIT: usize = 163;
const SYSCALL_SETRLIMIT: usize = 164;
const SYSCALL_GETRUSAGE: usize = 165;
const SYSCALL_UMASK: usize = 166;
const SYSCALL_PRCTL: usize = 167;
const SYSCALL_GETTIMEOFDAY: usize = 169;
const SYSCALL_GETPID: usize = 172;
const SYSCALL_GETPPID: usize = 173;
const SYSCALL_GETUID: usize = 174;
const SYSCALL_GETEUID: usize = 175;
const SYSCALL_GETGID: usize = 176;
const SYSCALL_GETEGID: usize = 177;
const SYSCALL_GETTID: usize = 178;
const SYSCALL_SYSINFO: usize = 179;
const SYSCALL_MSGGET: usize = 186;
const SYSCALL_MSGCTL: usize = 187;
const SYSCALL_MSGRCV: usize = 188;
const SYSCALL_MSGSND: usize = 189;
const SYSCALL_SEMGET: usize = 190;
const SYSCALL_SEMCTL: usize = 191;
const SYSCALL_SEMTIMEDOP: usize = 192;
const SYSCALL_SEMOP: usize = 193;
const SYSCALL_SHMGET: usize = 194;
const SYSCALL_SHMCTL: usize = 195;
const SYSCALL_SHMAT: usize = 196;
const SYSCALL_SHMDT: usize = 197;
const SYSCALL_SOCKET: usize = 198;
const SYSCALL_SOCKETPAIR: usize = 199;
const SYSCALL_BIND: usize = 200;
const SYSCALL_LISTEN: usize = 201;
const SYSCALL_ACCEPT: usize = 202;
const SYSCALL_CONNECT: usize = 203;
const SYSCALL_GETSOCKNAME: usize = 204;
const SYSCALL_GETPEERNAME: usize = 205;
const SYSCALL_SENDTO: usize = 206;
const SYSCALL_RECVFROM: usize = 207;
const SYSCALL_SETSOCKOPT: usize = 208;
const SYSCALL_GETSOCKOPT: usize = 209;
const SYSCALL_SHUTDOWN: usize = 210;
const SYSCALL_SENDMSG: usize = 211;
const SYSCALL_RECVMSG: usize = 212;
const SYSCALL_READAHEAD: usize = 213;
const SYSCALL_BRK: usize = 214;
const SYSCALL_MUNMAP: usize = 215;
const SYSCALL_MREMAP: usize = 216;
const SYSCALL_ADD_KEY: usize = 217;
const SYSCALL_REQUEST_KEY: usize = 218;
const SYSCALL_KEYCTL: usize = 219;
const SYSCALL_CLONE: usize = 220;
const SYSCALL_EXECVE: usize = 221;
const SYSCALL_MMAP: usize = 222;
const SYSCALL_FADVISE64: usize = 223;
const SYSCALL_SWAPON: usize = 224;
const SYSCALL_SWAPOFF: usize = 225;
const SYSCALL_MPROTECT: usize = 226;
const SYSCALL_MSYNC: usize = 227;
const SYSCALL_MLOCK: usize = 228;
const SYSCALL_MUNLOCK: usize = 229;
const SYSCALL_MLOCKALL: usize = 230;
const SYSCALL_MUNLOCKALL: usize = 231;
const SYSCALL_MINCORE: usize = 232;
const SYSCALL_MADVISE: usize = 233;
const SYSCALL_REMAP_FILE_PAGES: usize = 234;
const SYSCALL_PERF_EVENT_OPEN: usize = 241;
const SYSCALL_ACCEPT4: usize = 242;
#[cfg(target_arch = "riscv64")]
const SYSCALL_RISCV_HWPROBE: usize = 258;
#[cfg(target_arch = "riscv64")]
const SYSCALL_RISCV_FLUSH_ICACHE: usize = 259;
const SYSCALL_WAIT4: usize = 260;
const SYSCALL_PRLIMIT64: usize = 261;
const SYSCALL_FANOTIFY_INIT: usize = 262;
const SYSCALL_FANOTIFY_MARK: usize = 263;
const SYSCALL_NAME_TO_HANDLE_AT: usize = 264;
const SYSCALL_OPEN_BY_HANDLE_AT: usize = 265;
const SYSCALL_CLOCK_ADJTIME: usize = 266;
const SYSCALL_SYNCFS: usize = 267;
const SYSCALL_SETNS: usize = 268;
const SYSCALL_FINIT_MODULE: usize = 273;
const SYSCALL_SCHED_SETATTR: usize = 274;
const SYSCALL_SCHED_GETATTR: usize = 275;
const SYSCALL_RENAMEAT2: usize = 276;
const SYSCALL_GETRANDOM: usize = 278;
const SYSCALL_MEMFD_CREATE: usize = 279;
const SYSCALL_BPF: usize = 280;
const SYSCALL_EXECVEAT: usize = 281;
const SYSCALL_USERFAULTFD: usize = 282;
const SYSCALL_MEMBARRIER: usize = 283;
const SYSCALL_MLOCK2: usize = 284;
const SYSCALL_COPY_FILE_RANGE: usize = 285;
const SYSCALL_PREADV2: usize = 286;
const SYSCALL_PWRITEV2: usize = 287;
const SYSCALL_PKEY_MPROTECT: usize = 288;
const SYSCALL_PKEY_ALLOC: usize = 289;
const SYSCALL_PKEY_FREE: usize = 290;
const SYSCALL_STATX: usize = 291;
const SYSCALL_IO_PGETEVENTS: usize = 292;
const SYSCALL_PIDFD_SEND_SIGNAL: usize = 424;
const SYSCALL_IO_URING_SETUP: usize = 425;
const SYSCALL_IO_URING_ENTER: usize = 426;
const SYSCALL_IO_URING_REGISTER: usize = 427;
const SYSCALL_OPEN_TREE: usize = 428;
const SYSCALL_MOVE_MOUNT: usize = 429;
const SYSCALL_FSOPEN: usize = 430;
const SYSCALL_FSCONFIG: usize = 431;
const SYSCALL_FSMOUNT: usize = 432;
const SYSCALL_FSPICK: usize = 433;
const SYSCALL_PIDFD_OPEN: usize = 434;
const SYSCALL_CLONE3: usize = 435;
const SYSCALL_OPENAT2: usize = 437;
const SYSCALL_FACCESSAT2: usize = 439;
const SYSCALL_EPOLL_PWAIT2: usize = 441;
const SYSCALL_QUOTACTL_FD: usize = 443;
const SYSCALL_MEMFD_SECRET: usize = 447;
const SYSCALL_FCHMODAT2: usize = 452;

mod aio;
mod context;
pub(crate) mod errno;
mod fs;
mod futex;
pub(crate) mod keyring;
mod kmodule;
mod memory;
pub(crate) mod msg;
mod net;
mod process;
pub(crate) mod sem;
mod signal;
pub(crate) mod time;
pub(crate) mod uapi;
pub(crate) mod user_ptr;
mod wait;

use crate::perf;
use crate::task::{
    RLimit, SeccompSockFilter, SignalFlags, SignalInfo, TaskControlBlock, process_of_task,
    queue_signal_to_task,
};
use aio::*;
use alloc::sync::Arc;
use errno::{SysError, ret};
use fs::*;
use futex::*;
use keyring::*;
use kmodule::*;
use memory::*;
use msg::*;
use net::*;
use process::*;
use sem::*;
use signal::*;
use time::*;
use uapi::LinuxTimeSpec;
use wait::*;

pub(crate) use aio::aio_max_nr_content;
pub(crate) use context::SyscallContext;
pub(crate) use fs::{
    INOTIFY_MAX_QUEUED_EVENTS, INOTIFY_MAX_USER_INSTANCES, INOTIFY_MAX_USER_WATCHES,
    close_detached_fd_entry, close_detached_fd_entry_for_process_teardown,
    fanotify_evict_evictable_marks, fanotify_fdinfo, fanotify_max_queued_events, inotify_fdinfo,
    install_file_fd, release_record_locks_for_process,
};
pub(crate) use process::pidfd_fdinfo;
pub(crate) use process::{proc_sys_kernel_printk_content, write_proc_sys_kernel_printk};
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
pub(crate) use wait::LinuxSigInfo;

fn seccomp_filter_allows(filter: &[SeccompSockFilter], syscall_id: usize) -> bool {
    const BPF_LD_W_ABS: u16 = 0x20;
    const BPF_JMP_JEQ_K: u16 = 0x15;
    const BPF_RET_K: u16 = 0x06;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;

    let mut accumulator = 0u32;
    let mut pc = 0usize;
    while let Some(instruction) = filter.get(pc) {
        match instruction.code {
            BPF_LD_W_ABS => {
                accumulator = syscall_id as u32;
                pc += 1;
            }
            BPF_JMP_JEQ_K => {
                pc += 1 + if accumulator == instruction.k {
                    instruction.jt as usize
                } else {
                    instruction.jf as usize
                };
            }
            BPF_RET_K => return instruction.k & 0xffff_0000 == SECCOMP_RET_ALLOW,
            _ => return false,
        }
    }
    false
}

fn seccomp_signal_for_syscall(task: &TaskControlBlock, syscall_id: usize) -> Option<SignalFlags> {
    const SECCOMP_MODE_STRICT: u8 = 1;
    const SECCOMP_MODE_FILTER: u8 = 2;
    let inner = task.inner_exclusive_access();
    match inner.seccomp_mode {
        SECCOMP_MODE_STRICT => (!matches!(
            syscall_id,
            SYSCALL_READ | SYSCALL_WRITE | SYSCALL_EXIT | SYSCALL_RT_SIGRETURN
        ))
        .then_some(SignalFlags::SIGKILL),
        SECCOMP_MODE_FILTER => {
            if inner
                .seccomp_filter
                .as_deref()
                .is_some_and(|filter| seccomp_filter_allows(filter, syscall_id))
            {
                None
            } else {
                Some(SignalFlags::SIGSYS)
            }
        }
        _ => None,
    }
}

fn syscall_identity_fast_path(task: &TaskControlBlock, syscall_id: usize) -> Option<isize> {
    // Keep this path limited to pure identity getters. Anything that touches
    // user memory, can sleep, or depends on an exec-stable address-space token
    // must go through SyscallContext below.
    let value = match syscall_id {
        SYSCALL_GETTID => task.linux_tid() as isize,
        SYSCALL_GETPID | SYSCALL_GETPPID | SYSCALL_GETUID | SYSCALL_GETEUID | SYSCALL_GETGID
        | SYSCALL_GETEGID => {
            let process = task
                .process
                .upgrade()
                .expect("current task process must outlive the task");
            match syscall_id {
                SYSCALL_GETPID => process.visible_pid() as isize,
                SYSCALL_GETPPID => process.getppid() as isize,
                SYSCALL_GETUID => process.credentials().ruid as isize,
                SYSCALL_GETEUID => process.credentials().euid as isize,
                SYSCALL_GETGID => process.credentials().rgid as isize,
                SYSCALL_GETEGID => process.credentials().egid as isize,
                _ => unreachable!(),
            }
        }
        _ => return None,
    };
    perf::record_syscall_identity_fast_path();
    Some(value)
}

pub fn syscall_is_exit(syscall_id: usize) -> bool {
    syscall_id == SYSCALL_EXIT
}

pub fn syscall_is_exit_group(syscall_id: usize) -> bool {
    syscall_id == SYSCALL_EXIT_GROUP
}

pub fn syscall_with_current_task(
    current: Arc<TaskControlBlock>,
    syscall_id: usize,
    args: [usize; 6],
) -> isize {
    perf::record_syscall_dispatch_call();
    if syscall_id == SYSCALL_EXIT {
        drop(current);
        sys_exit(args[0] as i32);
    }
    if let Some(signal) = seccomp_signal_for_syscall(&current, syscall_id) {
        // UNFINISHED: Filter mode supports only a small classic-BPF subset.
        // Unsupported or denied filter paths fail closed with SIGSYS.
        let signum = signal.bits().trailing_zeros() as i32;
        queue_signal_to_task(Arc::clone(&current), signal, SignalInfo::user(signum, 0));
        return 0;
    }
    if syscall_id == SYSCALL_EXIT_GROUP {
        drop(current);
        sys_exit_group(args[0] as i32);
    }
    let _profile_scope = perf::time_scope(perf::ProfilePoint::SyscallDispatch);
    let _syscall_profile_scope = perf::time_syscall(syscall_id);
    if let Some(value) = syscall_identity_fast_path(&current, syscall_id) {
        return value;
    }

    let process = process_of_task(&current);
    let ctx = SyscallContext::new(current, process);
    ret(syscall_with_context(&ctx, syscall_id, args))
}

pub(crate) fn syscall_with_context(
    ctx: &SyscallContext,
    syscall_id: usize,
    args: [usize; 6],
) -> Result<isize, SysError> {
    match syscall_id {
        SYSCALL_IO_SETUP => sys_io_setup(args[0], args[1] as *mut usize),
        SYSCALL_IO_DESTROY => sys_io_destroy(args[0]),
        SYSCALL_IO_SUBMIT => sys_io_submit(args[0], args[1] as isize, args[2] as *const _),
        SYSCALL_IO_CANCEL => sys_io_cancel(args[0], args[1] as *const _, args[2] as *mut _),
        SYSCALL_IO_GETEVENTS => sys_io_getevents(
            args[0],
            args[1] as isize,
            args[2] as isize,
            args[3] as *mut _,
            args[4] as *const u8,
        ),
        SYSCALL_SETXATTR => sys_setxattr(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
            args[4] as u32,
        ),
        SYSCALL_LSETXATTR => sys_lsetxattr(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
            args[4] as u32,
        ),
        SYSCALL_FSETXATTR => sys_fsetxattr(
            args[0],
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
            args[4] as u32,
        ),
        SYSCALL_GETXATTR => sys_getxattr(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
        ),
        SYSCALL_LGETXATTR => sys_lgetxattr(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
        ),
        SYSCALL_FGETXATTR => {
            sys_fgetxattr(args[0], args[1] as *const u8, args[2] as *mut u8, args[3])
        }
        SYSCALL_LISTXATTR => sys_listxattr(args[0] as *const u8, args[1] as *mut u8, args[2]),
        SYSCALL_LLISTXATTR => sys_llistxattr(args[0] as *const u8, args[1] as *mut u8, args[2]),
        SYSCALL_FLISTXATTR => sys_flistxattr(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_REMOVEXATTR => sys_removexattr(args[0] as *const u8, args[1] as *const u8),
        SYSCALL_LREMOVEXATTR => sys_lremovexattr(args[0] as *const u8, args[1] as *const u8),
        SYSCALL_FREMOVEXATTR => sys_fremovexattr(args[0], args[1] as *const u8),
        SYSCALL_EVENTFD2 => sys_eventfd2(args[0] as u32, args[1] as u32),
        SYSCALL_GETCWD => sys_getcwd_ctx(ctx, args[0] as *mut u8, args[1]),
        SYSCALL_EPOLL_CREATE1 => sys_epoll_create1(args[0] as u32),
        SYSCALL_EPOLL_CTL => sys_epoll_ctl(args[0], args[1] as i32, args[2], args[3] as *const u8),
        SYSCALL_EPOLL_PWAIT => sys_epoll_pwait(
            args[0],
            args[1] as *mut u8,
            args[2] as i32,
            args[3] as i32,
            args[4] as *const u8,
            args[5],
        ),
        SYSCALL_EPOLL_PWAIT2 => sys_epoll_pwait2(
            args[0],
            args[1] as *mut u8,
            args[2] as i32,
            args[3] as *const LinuxTimeSpec,
            args[4] as *const u8,
            args[5],
        ),
        SYSCALL_DUP => sys_dup(args[0]),
        SYSCALL_DUP3 => sys_dup3(args[0], args[1], args[2] as u32),
        SYSCALL_FCNTL => sys_fcntl_ctx(ctx, args[0], args[1], args[2]),
        SYSCALL_INOTIFY_INIT1 => sys_inotify_init1(args[0] as u32),
        SYSCALL_INOTIFY_ADD_WATCH => {
            sys_inotify_add_watch(args[0], args[1] as *const u8, args[2] as u32)
        }
        SYSCALL_INOTIFY_RM_WATCH => sys_inotify_rm_watch(args[0], args[1] as i32),
        SYSCALL_IOCTL => sys_ioctl(args[0], args[1], args[2]),
        SYSCALL_FLOCK => sys_flock(args[0], args[1] as i32),
        SYSCALL_MKNODAT => sys_mknodat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as u64,
        ),
        SYSCALL_MKDIRAT => sys_mkdirat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_UNLINKAT => sys_unlinkat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_SYMLINKAT => {
            sys_symlinkat(args[0] as *const u8, args[1] as isize, args[2] as *const u8)
        }
        SYSCALL_LINKAT => sys_linkat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as isize,
            args[3] as *const u8,
            args[4] as u32,
        ),
        SYSCALL_RENAMEAT2 => sys_renameat2(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as isize,
            args[3] as *const u8,
            args[4] as u32,
        ),
        SYSCALL_GETRANDOM => sys_getrandom_ctx(ctx, args[0] as *mut u8, args[1], args[2] as u32),
        SYSCALL_MEMFD_CREATE => sys_memfd_create(args[0] as *const u8, args[1] as u32),
        SYSCALL_MEMFD_SECRET => sys_memfd_secret(args[0] as u32),
        SYSCALL_BPF => sys_bpf(args[0] as u32, args[1] as *const u8, args[2] as u32),
        SYSCALL_USERFAULTFD => sys_userfaultfd(args[0] as u32),
        SYSCALL_MEMBARRIER => sys_membarrier(args[0] as i32, args[1] as u32, args[2] as i32),
        SYSCALL_UMOUNT2 => sys_umount2(args[0] as *const u8, args[1] as i32),
        SYSCALL_MOUNT => sys_mount(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
            args[4] as *const u8,
        ),
        SYSCALL_STATFS => sys_statfs_ctx(ctx, args[0] as *const u8, args[1] as *mut LinuxStatfs),
        SYSCALL_FSTATFS => sys_fstatfs_ctx(ctx, args[0], args[1] as *mut LinuxStatfs),
        SYSCALL_TRUNCATE => sys_truncate(args[0] as *const u8, args[1]),
        SYSCALL_FTRUNCATE => sys_ftruncate(args[0], args[1]),
        SYSCALL_FALLOCATE => sys_fallocate(args[0], args[1] as u32, args[2], args[3]),
        SYSCALL_FACCESSAT => sys_faccessat(args[0] as isize, args[1] as *const u8, args[2] as i32),
        SYSCALL_FACCESSAT2 => sys_faccessat2(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as i32,
            args[3] as i32,
        ),
        SYSCALL_CHDIR => sys_chdir(args[0] as *const u8),
        SYSCALL_FCHDIR => sys_fchdir(args[0]),
        SYSCALL_CHROOT => sys_chroot(args[0] as *const u8),
        SYSCALL_FCHMOD => sys_fchmod(args[0], args[1] as u32),
        SYSCALL_FCHMODAT => sys_fchmodat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_FCHMODAT2 => sys_fchmodat2(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as i32,
        ),
        SYSCALL_FCHOWNAT => sys_fchownat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as u32,
            args[4] as i32,
        ),
        SYSCALL_FCHOWN => sys_fchown(args[0], args[1] as u32, args[2] as u32),
        SYSCALL_OPENAT => sys_openat_ctx(
            ctx,
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as u32,
        ),
        SYSCALL_OPENAT2 => sys_openat2_ctx(
            ctx,
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
        ),
        SYSCALL_CLOSE => sys_close(args[0]),
        SYSCALL_PIPE2 => sys_pipe2_ctx(ctx, args[0] as *mut i32, args[1] as u32),
        SYSCALL_QUOTACTL => sys_quotactl(
            args[0] as i32,
            args[1] as *const u8,
            args[2] as u32,
            args[3],
        ),
        SYSCALL_GETDENTS64 => sys_getdents64_ctx(ctx, args[0], args[1] as *mut u8, args[2]),
        SYSCALL_LSEEK => sys_lseek(args[0], args[1] as i64, args[2]),
        SYSCALL_READV => sys_readv_ctx(ctx, args[0], args[1] as *const LinuxIovec, args[2]),
        SYSCALL_READ => sys_read_ctx(ctx, args[0], args[1] as *const u8, args[2]),
        SYSCALL_WRITE => sys_write_ctx(ctx, args[0], args[1] as *const u8, args[2]),
        SYSCALL_WRITEV => sys_writev_ctx(ctx, args[0], args[1] as *const LinuxIovec, args[2]),
        SYSCALL_PREAD64 => sys_pread64(args[0], args[1] as *mut u8, args[2], args[3]),
        SYSCALL_PWRITE64 => sys_pwrite64(args[0], args[1] as *const u8, args[2], args[3]),
        SYSCALL_PREADV => sys_preadv(
            args[0],
            args[1] as *const LinuxIovec,
            args[2],
            args[3],
            args[4],
        ),
        SYSCALL_PWRITEV => sys_pwritev(
            args[0],
            args[1] as *const LinuxIovec,
            args[2],
            args[3],
            args[4],
        ),
        SYSCALL_SENDFILE => sys_sendfile(args[0], args[1], args[2] as *mut i64, args[3]),
        SYSCALL_PREADV2 => sys_preadv2(
            args[0],
            args[1] as *const LinuxIovec,
            args[2],
            args[3],
            args[4],
            args[5],
        ),
        SYSCALL_PWRITEV2 => sys_pwritev2(
            args[0],
            args[1] as *const LinuxIovec,
            args[2],
            args[3],
            args[4],
            args[5],
        ),
        SYSCALL_READAHEAD => sys_readahead(args[0], args[1], args[2]),
        SYSCALL_FADVISE64 => sys_fadvise64(args[0], args[1] as i64, args[2] as i64, args[3] as i32),
        SYSCALL_SWAPON => sys_swapon(args[0] as *const u8, args[1] as u32),
        SYSCALL_SWAPOFF => sys_swapoff(args[0] as *const u8),
        SYSCALL_COPY_FILE_RANGE => sys_copy_file_range(
            args[0],
            args[1] as *mut i64,
            args[2],
            args[3] as *mut i64,
            args[4],
            args[5] as u32,
        ),
        SYSCALL_SPLICE => sys_splice(
            args[0],
            args[1] as *mut i64,
            args[2],
            args[3] as *mut i64,
            args[4],
            args[5] as u32,
        ),
        SYSCALL_PSELECT6 => sys_pselect6(
            args[0],
            args[1],
            args[2],
            args[3],
            args[4] as *const LinuxTimeSpec,
            args[5],
        ),
        SYSCALL_PPOLL => sys_ppoll(
            args[0] as *mut LinuxPollFd,
            args[1],
            args[2] as *const LinuxTimeSpec,
            args[3] as *const u8,
            args[4],
        ),
        SYSCALL_SIGNALFD4 => sys_signalfd4(
            args[0] as isize,
            args[1] as *const u8,
            args[2],
            args[3] as u32,
        ),
        SYSCALL_READLINKAT => sys_readlinkat_ctx(
            ctx,
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
        ),
        SYSCALL_NEWFSTATAT => sys_newfstatat_ctx(
            ctx,
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *mut LinuxKstat,
            args[3] as i32,
        ),
        SYSCALL_FSTAT => sys_fstat_ctx(ctx, args[0], args[1] as *mut LinuxKstat),
        SYSCALL_SYNC => sys_sync(),
        SYSCALL_FSYNC => sys_fsync(args[0]),
        SYSCALL_FDATASYNC => sys_fdatasync(args[0]),
        SYSCALL_SYNCFS => sys_syncfs(args[0]),
        SYSCALL_INIT_MODULE => sys_init_module(args[0] as *const u8, args[1], args[2] as *const u8),
        SYSCALL_DELETE_MODULE => sys_delete_module(args[0] as *const u8, args[1] as u32),
        SYSCALL_TIMERFD_CREATE => sys_timerfd_create(args[0] as i32, args[1] as u32),
        SYSCALL_UTIMENSAT => sys_utimensat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *const LinuxTimeSpec,
            args[3] as i32,
        ),
        SYSCALL_CAPGET => sys_capget_ctx(
            ctx,
            args[0] as *mut LinuxCapUserHeader,
            args[1] as *mut LinuxCapUserData,
        ),
        SYSCALL_CAPSET => sys_capset_ctx(
            ctx,
            args[0] as *mut LinuxCapUserHeader,
            args[1] as *const LinuxCapUserData,
        ),
        SYSCALL_PERSONALITY => sys_personality(args[0]),
        SYSCALL_STATX => sys_statx_ctx(
            ctx,
            args[0] as isize,
            args[1] as *const u8,
            args[2] as i32,
            args[3] as u32,
            args[4] as *mut LinuxStatx,
        ),
        SYSCALL_OPEN_TREE => sys_open_tree(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_MOVE_MOUNT => sys_move_mount(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as isize,
            args[3] as *const u8,
            args[4] as u32,
        ),
        SYSCALL_FSOPEN => sys_fsopen(args[0] as *const u8, args[1] as u32),
        SYSCALL_FSCONFIG => sys_fsconfig(
            args[0] as isize,
            args[1] as u32,
            args[2] as *const u8,
            args[3] as *const u8,
            args[4] as i32,
        ),
        SYSCALL_FSMOUNT => sys_fsmount(args[0] as isize, args[1] as u32, args[2] as u32),
        SYSCALL_FSPICK => sys_fspick(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_IO_URING_SETUP => sys_io_uring_setup(args[0] as u32, args[1] as *mut u8),
        SYSCALL_IO_URING_ENTER => sys_io_uring_enter(
            args[0],
            args[1] as u32,
            args[2] as u32,
            args[3] as u32,
            args[4],
        ),
        SYSCALL_IO_URING_REGISTER => {
            sys_io_uring_register(args[0], args[1] as u32, args[2], args[3] as u32)
        }
        SYSCALL_QUOTACTL_FD => sys_quotactl_fd(args[0], args[1] as i32, args[2] as u32, args[3]),
        SYSCALL_IO_PGETEVENTS => sys_io_pgetevents(
            args[0],
            args[1] as isize,
            args[2] as isize,
            args[3] as *mut _,
            args[4] as *const u8,
            args[5] as *const u8,
        ),
        SYSCALL_WAITID => sys_waitid(
            args[0] as i32,
            args[1] as i32,
            args[2] as *mut LinuxSigInfo,
            args[3] as i32,
            args[4] as *mut RUsage,
        ),
        SYSCALL_SET_TID_ADDRESS => sys_set_tid_address_ctx(ctx, args[0]),
        SYSCALL_UNSHARE => sys_unshare(args[0]),
        SYSCALL_FUTEX => sys_futex(
            args[0] as *mut u32,
            args[1] as u32,
            args[2] as u32,
            args[3] as *const LinuxTimeSpec,
            args[4] as *mut u32,
            args[5] as u32,
        ),
        SYSCALL_SET_ROBUST_LIST => sys_set_robust_list(args[0], args[1]),
        SYSCALL_GET_ROBUST_LIST => sys_get_robust_list(
            args[0] as isize,
            args[1] as *mut usize,
            args[2] as *mut usize,
        ),
        SYSCALL_NANOSLEEP => sys_nanosleep(
            args[0] as *const LinuxTimeSpec,
            args[1] as *mut LinuxTimeSpec,
        ),
        SYSCALL_GETITIMER => sys_getitimer(args[0] as i32, args[1] as *mut u8),
        SYSCALL_SETITIMER => {
            sys_setitimer(args[0] as i32, args[1] as *const u8, args[2] as *mut u8)
        }
        SYSCALL_TIMER_CREATE => {
            sys_timer_create(args[0] as i32, args[1] as *const u8, args[2] as *mut i32)
        }
        SYSCALL_TIMER_GETTIME => sys_timer_gettime(args[0] as i32, args[1] as *mut _),
        SYSCALL_TIMER_GETOVERRUN => sys_timer_getoverrun(args[0] as i32),
        SYSCALL_TIMER_SETTIME => sys_timer_settime(
            args[0] as i32,
            args[1] as i32,
            args[2] as *const _,
            args[3] as *mut _,
        ),
        SYSCALL_TIMER_DELETE => sys_timer_delete(args[0] as i32),
        SYSCALL_CLOCK_SETTIME => sys_clock_settime(args[0] as i32, args[1] as *const LinuxTimeSpec),
        SYSCALL_CLOCK_GETTIME => {
            sys_clock_gettime_ctx(ctx, args[0] as i32, args[1] as *mut LinuxTimeSpec)
        }
        SYSCALL_CLOCK_GETRES => {
            sys_clock_getres_ctx(ctx, args[0] as i32, args[1] as *mut LinuxTimeSpec)
        }
        SYSCALL_CLOCK_NANOSLEEP => sys_clock_nanosleep(
            args[0] as i32,
            args[1] as u32,
            args[2] as *const LinuxTimeSpec,
            args[3] as *mut LinuxTimeSpec,
        ),
        SYSCALL_SYSLOG => sys_syslog(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_PTRACE => sys_ptrace(args[0], args[1] as isize, args[2], args[3]),
        SYSCALL_SCHED_SETPARAM => sys_sched_setparam(args[0] as isize, args[1]),
        SYSCALL_SCHED_SETSCHEDULER => {
            sys_sched_setscheduler(args[0] as isize, args[1] as i32, args[2])
        }
        SYSCALL_SCHED_GETSCHEDULER => sys_sched_getscheduler(args[0] as isize),
        SYSCALL_SCHED_GETPARAM => sys_sched_getparam(args[0] as isize, args[1]),
        SYSCALL_SCHED_SETAFFINITY => {
            sys_sched_setaffinity_ctx(ctx, args[0] as isize, args[1], args[2])
        }
        SYSCALL_SCHED_GETAFFINITY => {
            sys_sched_getaffinity_ctx(ctx, args[0] as isize, args[1], args[2])
        }
        SYSCALL_SCHED_YIELD => Ok(sys_sched_yield()),
        SYSCALL_SCHED_GET_PRIORITY_MAX => sys_sched_get_priority_max(args[0] as i32),
        SYSCALL_SCHED_GET_PRIORITY_MIN => sys_sched_get_priority_min(args[0] as i32),
        SYSCALL_SCHED_RR_GET_INTERVAL => {
            sys_sched_rr_get_interval(args[0] as isize, args[1] as *mut LinuxTimeSpec)
        }
        SYSCALL_SCHED_SETATTR => sys_sched_setattr(args[0] as isize, args[1], args[2] as u32),
        SYSCALL_SCHED_GETATTR => {
            sys_sched_getattr(args[0] as isize, args[1], args[2], args[3] as u32)
        }
        SYSCALL_KILL => sys_kill(args[0] as isize, args[1] as u32),
        SYSCALL_TKILL => sys_tkill(args[0] as isize, args[1] as u32),
        SYSCALL_TGKILL => sys_tgkill(args[0] as isize, args[1] as isize, args[2] as u32),
        SYSCALL_PIDFD_SEND_SIGNAL => sys_pidfd_send_signal(
            args[0],
            args[1] as u32,
            args[2] as *const LinuxSigInfo,
            args[3] as u32,
        ),
        SYSCALL_PIDFD_OPEN => sys_pidfd_open(args[0], args[1] as u32),
        SYSCALL_SIGALTSTACK => sys_sigaltstack_ctx(ctx, args[0] as *const u8, args[1] as *mut u8),
        SYSCALL_RT_SIGSUSPEND => sys_rt_sigsuspend(args[0] as *const u8, args[1]),
        SYSCALL_RT_SIGACTION => sys_rt_sigaction_ctx(
            ctx,
            args[0] as u32,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
        ),
        SYSCALL_RT_SIGPROCMASK => sys_rt_sigprocmask_ctx(
            ctx,
            args[0],
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
        ),
        SYSCALL_RT_SIGPENDING => sys_rt_sigpending_ctx(ctx, args[0] as *mut u8, args[1]),
        SYSCALL_RT_SIGTIMEDWAIT => sys_rt_sigtimedwait(
            args[0] as *const u8,
            args[1] as *mut LinuxSigInfo,
            args[2] as *const LinuxTimeSpec,
            args[3],
        ),
        SYSCALL_RT_SIGRETURN => sys_rt_sigreturn(),
        SYSCALL_SETPRIORITY => sys_setpriority(args[0] as i32, args[1] as isize, args[2] as i32),
        SYSCALL_GETPRIORITY => sys_getpriority(args[0] as i32, args[1] as isize),
        SYSCALL_REBOOT => sys_reboot(args[0], args[1], args[2], args[3]),
        SYSCALL_SETREGID => sys_setregid(args[0] as i32, args[1] as i32),
        SYSCALL_SETGID => sys_setgid(args[0] as u32),
        SYSCALL_SETREUID => sys_setreuid(args[0] as i32, args[1] as i32),
        SYSCALL_SETUID => sys_setuid(args[0] as u32),
        SYSCALL_SETRESUID => sys_setresuid(args[0] as i32, args[1] as i32, args[2] as i32),
        SYSCALL_GETRESUID => sys_getresuid_ctx(
            ctx,
            args[0] as *mut u32,
            args[1] as *mut u32,
            args[2] as *mut u32,
        ),
        SYSCALL_SETRESGID => sys_setresgid(args[0] as i32, args[1] as i32, args[2] as i32),
        SYSCALL_GETRESGID => sys_getresgid_ctx(
            ctx,
            args[0] as *mut u32,
            args[1] as *mut u32,
            args[2] as *mut u32,
        ),
        SYSCALL_SETFSUID => sys_setfsuid(args[0] as i32),
        SYSCALL_SETFSGID => sys_setfsgid(args[0] as i32),
        SYSCALL_TIMES => sys_times_ctx(ctx, args[0] as *mut LinuxTms),
        SYSCALL_SETPGID => sys_setpgid_ctx(ctx, args[0] as isize, args[1] as isize),
        SYSCALL_GETPGID => sys_getpgid_ctx(ctx, args[0] as isize),
        SYSCALL_GETSID => sys_getsid_ctx(ctx, args[0] as isize),
        SYSCALL_SETSID => sys_setsid(),
        SYSCALL_GETGROUPS => sys_getgroups_ctx(ctx, args[0], args[1] as *mut u32),
        SYSCALL_SETGROUPS => sys_setgroups_ctx(ctx, args[0], args[1] as *const u32),
        SYSCALL_UNAME => sys_uname_ctx(ctx, args[0] as *mut LinuxUtsName),
        SYSCALL_GETRLIMIT => sys_getrlimit_ctx(ctx, args[0] as i32, args[1] as *mut RLimit),
        SYSCALL_SETRLIMIT => sys_setrlimit_ctx(ctx, args[0] as i32, args[1] as *const RLimit),
        SYSCALL_GETRUSAGE => sys_getrusage(args[0] as i32, args[1] as *mut RUsage),
        SYSCALL_UMASK => sys_umask(args[0] as u32),
        SYSCALL_PRCTL => sys_prctl_ctx(ctx, args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_GETTIMEOFDAY => sys_gettimeofday_ctx(
            ctx,
            args[0] as *mut LinuxTimeVal,
            args[1] as *mut LinuxTimezone,
        ),
        SYSCALL_GETPID => Ok(sys_getpid()),
        SYSCALL_GETPPID => Ok(sys_getppid()),
        SYSCALL_GETUID => Ok(sys_getuid()),
        SYSCALL_GETEUID => Ok(sys_geteuid()),
        SYSCALL_GETGID => Ok(sys_getgid()),
        SYSCALL_GETEGID => Ok(sys_getegid()),
        SYSCALL_GETTID => Ok(sys_gettid()),
        SYSCALL_SYSINFO => sys_sysinfo_ctx(ctx, args[0] as *mut LinuxSysInfo),
        SYSCALL_MSGGET => sys_msgget(args[0] as isize, args[1] as i32),
        SYSCALL_MSGCTL => sys_msgctl(args[0], args[1] as i32, args[2]),
        SYSCALL_MSGRCV => sys_msgrcv(
            args[0],
            args[1] as *mut u8,
            args[2],
            args[3] as isize,
            args[4] as i32,
        ),
        SYSCALL_MSGSND => sys_msgsnd(args[0], args[1] as *const u8, args[2], args[3] as i32),
        SYSCALL_SEMGET => sys_semget(args[0] as isize, args[1], args[2] as i32),
        SYSCALL_SEMCTL => sys_semctl(args[0], args[1], args[2] as i32, args[3]),
        SYSCALL_SEMTIMEDOP => {
            sys_semtimedop(args[0], args[1] as *const _, args[2], args[3] as *const _)
        }
        SYSCALL_SEMOP => sys_semop(args[0], args[1] as *const _, args[2]),
        SYSCALL_SHMGET => sys_shmget(args[0] as isize, args[1], args[2] as i32),
        SYSCALL_SHMCTL => sys_shmctl(args[0], args[1] as i32, args[2]),
        SYSCALL_SHMAT => sys_shmat(args[0], args[1], args[2] as i32),
        SYSCALL_SHMDT => sys_shmdt(args[0]),
        SYSCALL_BRK => sys_brk(args[0]),
        SYSCALL_MUNMAP => sys_munmap(args[0], args[1]),
        SYSCALL_MREMAP => sys_mremap(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_ADD_KEY => sys_add_key(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
            args[4] as i32,
        ),
        SYSCALL_REQUEST_KEY => sys_request_key(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *const u8,
            args[3] as i32,
        ),
        SYSCALL_KEYCTL => sys_keyctl(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_MPROTECT => sys_mprotect(args[0], args[1], args[2]),
        SYSCALL_MLOCK => sys_mlock(args[0], args[1]),
        SYSCALL_MUNLOCK => sys_munlock(args[0], args[1]),
        SYSCALL_MLOCKALL => sys_mlockall(args[0]),
        SYSCALL_MUNLOCKALL => sys_munlockall(),
        SYSCALL_MINCORE => sys_mincore(args[0], args[1], args[2] as *mut u8),
        SYSCALL_MADVISE => sys_madvise(args[0], args[1], args[2] as i32),
        SYSCALL_REMAP_FILE_PAGES => {
            sys_remap_file_pages(args[0], args[1], args[2] as i32, args[3], args[4] as i32)
        }
        SYSCALL_PKEY_MPROTECT => sys_pkey_mprotect(args[0], args[1], args[2], args[3] as isize),
        SYSCALL_PKEY_ALLOC => sys_pkey_alloc(args[0], args[1]),
        SYSCALL_PKEY_FREE => sys_pkey_free(args[0] as isize),
        SYSCALL_CLONE => sys_clone(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_CLONE3 => sys_clone3(args[0] as *const LinuxCloneArgs, args[1]),
        SYSCALL_EXECVE => sys_execve_ctx(
            ctx,
            args[0] as *const u8,
            args[1] as *const usize,
            args[2] as *const usize,
        ),
        SYSCALL_EXECVEAT => sys_execveat_ctx(
            ctx,
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *const usize,
            args[3] as *const usize,
            args[4],
        ),
        SYSCALL_MMAP => sys_mmap(args[0], args[1], args[2], args[3], args[4], args[5]),
        SYSCALL_MSYNC => sys_msync(args[0], args[1], args[2] as i32),
        SYSCALL_MLOCK2 => sys_mlock2(args[0], args[1], args[2]),
        SYSCALL_WAIT4 => sys_wait4_ctx(
            ctx,
            args[0] as isize,
            args[1] as *mut i32,
            args[2] as i32,
            args[3] as *mut RUsage,
        ),
        SYSCALL_PRLIMIT64 => sys_prlimit64_ctx(
            ctx,
            args[0],
            args[1] as i32,
            args[2] as *const RLimit,
            args[3] as *mut RLimit,
        ),
        SYSCALL_FANOTIFY_INIT => sys_fanotify_init(args[0] as u32, args[1] as u32),
        SYSCALL_FANOTIFY_MARK => sys_fanotify_mark(
            args[0],
            args[1] as u32,
            args[2] as u64,
            args[3] as isize,
            args[4] as *const u8,
        ),
        SYSCALL_NAME_TO_HANDLE_AT => sys_name_to_handle_at(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3] as *mut i32,
            args[4] as i32,
        ),
        SYSCALL_OPEN_BY_HANDLE_AT => {
            sys_open_by_handle_at(args[0] as isize, args[1] as *const u8, args[2] as u32)
        }
        SYSCALL_PERF_EVENT_OPEN => sys_perf_event_open(
            args[0] as *const u8,
            args[1] as isize,
            args[2] as isize,
            args[3] as isize,
            args[4] as u64,
        ),
        SYSCALL_CLOCK_ADJTIME => sys_clock_adjtime(args[0] as i32, args[1] as *mut LinuxTimex),
        SYSCALL_SETNS => sys_setns(args[0], args[1]),
        #[cfg(target_arch = "riscv64")]
        SYSCALL_RISCV_HWPROBE => sys_riscv_hwprobe_ctx(
            ctx,
            args[0] as *mut u8,
            args[1],
            args[2],
            args[3],
            args[4] as u32,
        ),
        #[cfg(target_arch = "riscv64")]
        SYSCALL_RISCV_FLUSH_ICACHE => sys_riscv_flush_icache(args[0], args[1], args[2]),
        SYSCALL_FINIT_MODULE => sys_finit_module(args[0], args[1] as *const u8, args[2] as u32),
        SYSCALL_SOCKET => sys_socket(args[0] as i32, args[1] as i32, args[2] as i32),
        SYSCALL_SOCKETPAIR => {
            sys_socketpair(args[0] as i32, args[1] as i32, args[2] as i32, args[3])
        }
        SYSCALL_BIND => sys_bind(args[0], args[1], args[2] as u32),
        SYSCALL_LISTEN => sys_listen(args[0], args[1] as i32),
        SYSCALL_ACCEPT => sys_accept(args[0], args[1], args[2]),
        SYSCALL_ACCEPT4 => sys_accept4(args[0], args[1], args[2], args[3] as i32),
        SYSCALL_CONNECT => sys_connect(args[0], args[1], args[2] as u32),
        SYSCALL_GETSOCKNAME => sys_getsockname(args[0], args[1], args[2]),
        SYSCALL_GETPEERNAME => sys_getpeername(args[0], args[1], args[2]),
        SYSCALL_SENDTO => sys_sendto(
            args[0],
            args[1],
            args[2],
            args[3] as i32,
            args[4],
            args[5] as u32,
        ),
        SYSCALL_RECVFROM => {
            sys_recvfrom(args[0], args[1], args[2], args[3] as i32, args[4], args[5])
        }
        SYSCALL_SETSOCKOPT => sys_setsockopt(
            args[0],
            args[1] as i32,
            args[2] as i32,
            args[3],
            args[4] as u32,
        ),
        SYSCALL_GETSOCKOPT => {
            sys_getsockopt(args[0], args[1] as i32, args[2] as i32, args[3], args[4])
        }
        SYSCALL_SHUTDOWN => sys_shutdown(args[0], args[1] as i32),
        SYSCALL_SENDMSG => sys_sendmsg(args[0], args[1], args[2] as i32),
        SYSCALL_RECVMSG => sys_recvmsg(args[0], args[1], args[2] as i32),
        _ => Err(SysError::ENOSYS),
    }
}
