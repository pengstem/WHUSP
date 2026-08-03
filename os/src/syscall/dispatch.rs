use super::context::SyscallContext;
use super::fs::*;
use super::kmodule::*;
use super::memory::*;
use super::msg::*;
use super::net::*;
use super::process::*;
#[cfg(feature = "read-mostly-probe")]
use super::read_mostly_probe::sys_read_mostly_probe;
#[cfg(feature = "sleep-rwlock-probe")]
use super::rwlock_probe::sys_fs4_sleep_rwlock_probe;
use super::sem::*;
use super::signal::*;
use super::time::*;
use super::uapi::LinuxTimeSpec;
use super::wait::*;
use super::{sys_futex, sys_get_robust_list, sys_set_robust_list};
use crate::perf;
use crate::task::{ProcessControlBlock, RLimit, TaskControlBlock};
use crate::uapi::errno::{Errno, KResult};
use crate::uapi::linux::fs::LinuxIovec;
use crate::uapi::syscall_nr::*;
use alloc::sync::Arc;

/// Converts a typed syscall result into the Linux register return convention.
fn ret(result: KResult<isize>) -> isize {
    match result {
        Ok(value) => value,
        Err(err) => -(err as isize),
    }
}

fn syscall_identity_fast_path(
    task: &TaskControlBlock,
    process: &ProcessControlBlock,
    syscall_id: usize,
) -> Option<isize> {
    // Keep this path limited to pure identity getters. Anything that touches
    // user memory, can sleep, or depends on an exec-stable address-space token
    // must go through SyscallContext below.
    let value = match syscall_id {
        SYSCALL_GETTID => task.linux_tid() as isize,
        SYSCALL_GETPID | SYSCALL_GETPPID | SYSCALL_GETUID | SYSCALL_GETEUID | SYSCALL_GETGID
        | SYSCALL_GETEGID => match syscall_id {
            SYSCALL_GETPID => process.visible_pid() as isize,
            SYSCALL_GETPPID => process.getppid() as isize,
            SYSCALL_GETUID => process.credentials().ruid as isize,
            SYSCALL_GETEUID => process.credentials().euid as isize,
            SYSCALL_GETGID => process.credentials().rgid as isize,
            SYSCALL_GETEGID => process.credentials().egid as isize,
            _ => unreachable!(),
        },
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

pub(crate) struct SyscallOutcome {
    pub(crate) result: isize,
    pub(crate) task: Arc<TaskControlBlock>,
    pub(crate) process: Arc<ProcessControlBlock>,
}

/// Handles the only syscall paths that may consume the trap frame's current
/// task reference without returning it.
pub(crate) fn syscall_exit_with_current_task(
    current: Arc<TaskControlBlock>,
    syscall_id: usize,
    args: [usize; 6],
) -> ! {
    assert!(
        syscall_is_exit(syscall_id) || syscall_is_exit_group(syscall_id),
        "exit syscall dispatcher received a returning syscall"
    );
    perf::record_syscall_dispatch_call();
    drop(current);
    if syscall_id == SYSCALL_EXIT {
        sys_exit(args[0] as i32);
    }
    sys_exit_group(args[0] as i32);
}

/// Moves the trap frame's owning references through a returning syscall.
///
/// This avoids a task Arc clone/drop and a process Weak upgrade/drop on every
/// ordinary dispatch. `SyscallContext` remains owning, so a handler may sleep
/// or schedule before the same references are moved back to the trap frame.
pub(crate) fn syscall_with_current_task(
    current: Arc<TaskControlBlock>,
    process: Arc<ProcessControlBlock>,
    syscall_id: usize,
    args: [usize; 6],
) -> SyscallOutcome {
    assert!(
        !syscall_is_exit(syscall_id) && !syscall_is_exit_group(syscall_id),
        "returning syscall dispatcher received an exit syscall"
    );
    perf::record_syscall_dispatch_call();
    let _profile_scope = perf::time_scope(perf::ProfilePoint::SyscallDispatch);
    let _syscall_profile_scope = perf::time_syscall(syscall_id);
    if let Some(result) = syscall_identity_fast_path(&current, &process, syscall_id) {
        return SyscallOutcome {
            result,
            task: current,
            process,
        };
    }

    let ctx = SyscallContext::new(current, process);
    let result = ret(syscall_with_context(&ctx, syscall_id, args));
    let (task, process) = ctx.into_current();
    SyscallOutcome {
        result,
        task,
        process,
    }
}

pub(crate) fn syscall_with_context(
    ctx: &SyscallContext,
    syscall_id: usize,
    args: [usize; 6],
) -> Result<isize, Errno> {
    match syscall_id {
        #[cfg(feature = "read-mostly-probe")]
        SYSCALL_READ_MOSTLY_PROBE => sys_read_mostly_probe(args[0], args[1]),
        #[cfg(feature = "sleep-rwlock-probe")]
        SYSCALL_FS4_SLEEP_RWLOCK_PROBE => sys_fs4_sleep_rwlock_probe(args[0], args[1], args[2]),
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
        #[cfg(feature = "inotify")]
        SYSCALL_INOTIFY_INIT1 => sys_inotify_init1(args[0] as u32),
        #[cfg(feature = "inotify")]
        SYSCALL_INOTIFY_ADD_WATCH => {
            sys_inotify_add_watch(args[0], args[1] as *const u8, args[2] as u32)
        }
        #[cfg(feature = "inotify")]
        SYSCALL_INOTIFY_RM_WATCH => sys_inotify_rm_watch(args[0], args[1] as i32),
        SYSCALL_IOCTL => sys_ioctl(args[0], args[1], args[2]),
        SYSCALL_IOPRIO_SET => {
            sys_ioprio_set_ctx(ctx, args[0] as i32, args[1] as isize, args[2] as i32)
        }
        SYSCALL_IOPRIO_GET => sys_ioprio_get_ctx(ctx, args[0] as i32, args[1] as isize),
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
        SYSCALL_CLOSE => sys_close_ctx(ctx, args[0]),
        SYSCALL_VHANGUP => sys_vhangup_ctx(ctx),
        SYSCALL_PIPE2 => sys_pipe2_ctx(ctx, args[0] as *mut i32, args[1] as u32),
        SYSCALL_CLOSE_RANGE => sys_close_range(
            args[0] as u32 as usize,
            args[1] as u32 as usize,
            args[2] as u32,
        ),
        SYSCALL_GETDENTS64 => sys_getdents64_ctx(ctx, args[0], args[1] as *mut u8, args[2]),
        SYSCALL_LSEEK => sys_lseek_ctx(ctx, args[0], args[1] as i64, args[2]),
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
        SYSCALL_COPY_FILE_RANGE => sys_copy_file_range(
            args[0],
            args[1] as *mut i64,
            args[2],
            args[3] as *mut i64,
            args[4],
            args[5] as u32,
        ),
        SYSCALL_VMSPLICE => sys_vmsplice_ctx(
            ctx,
            args[0],
            args[1] as *const LinuxIovec,
            args[2],
            args[3] as u32,
        ),
        SYSCALL_SPLICE => sys_splice(
            args[0],
            args[1] as *mut i64,
            args[2],
            args[3] as *mut i64,
            args[4],
            args[5] as u32,
        ),
        SYSCALL_TEE => sys_tee(args[0], args[1], args[2], args[3] as u32),
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
        SYSCALL_SYNC_FILE_RANGE => {
            sys_sync_file_range(args[0], args[1] as i64, args[2] as i64, args[3] as u32)
        }
        SYSCALL_SYNCFS => sys_syncfs(args[0]),
        SYSCALL_INIT_MODULE => sys_init_module(args[0] as *const u8, args[1], args[2] as *const u8),
        SYSCALL_DELETE_MODULE => sys_delete_module(args[0] as *const u8, args[1] as u32),
        #[cfg(feature = "timerfd")]
        SYSCALL_TIMERFD_CREATE => sys_timerfd_create(args[0] as i32, args[1] as u32),
        #[cfg(feature = "timerfd")]
        SYSCALL_TIMERFD_SETTIME => sys_timerfd_settime(
            args[0] as i32,
            args[1] as u32,
            args[2] as *const LinuxITimerSpec,
            args[3] as *mut LinuxITimerSpec,
        ),
        #[cfg(feature = "timerfd")]
        SYSCALL_TIMERFD_GETTIME => {
            sys_timerfd_gettime(args[0] as i32, args[1] as *mut LinuxITimerSpec)
        }
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
        #[cfg(feature = "setitimer")]
        SYSCALL_GETITIMER => sys_getitimer(args[0] as i32, args[1] as *mut u8),
        #[cfg(feature = "setitimer")]
        SYSCALL_SETITIMER => {
            sys_setitimer(args[0] as i32, args[1] as *const u8, args[2] as *mut u8)
        }
        #[cfg(feature = "posix-timers")]
        SYSCALL_TIMER_CREATE => {
            sys_timer_create(args[0] as i32, args[1] as *const u8, args[2] as *mut i32)
        }
        #[cfg(feature = "posix-timers")]
        SYSCALL_TIMER_GETTIME => sys_timer_gettime(args[0] as i32, args[1] as *mut _),
        #[cfg(feature = "posix-timers")]
        SYSCALL_TIMER_GETOVERRUN => sys_timer_getoverrun(args[0] as i32),
        #[cfg(feature = "posix-timers")]
        SYSCALL_TIMER_SETTIME => sys_timer_settime(
            args[0] as i32,
            args[1] as i32,
            args[2] as *const _,
            args[3] as *mut _,
        ),
        #[cfg(feature = "posix-timers")]
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
        #[cfg(feature = "ptrace")]
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
        SYSCALL_GETCPU => sys_getcpu_ctx(ctx, args[0], args[1]),
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
        SYSCALL_PIDFD_GETFD => sys_pidfd_getfd(args[0] as i32, args[1] as i32, args[2] as u32),
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
        SYSCALL_RT_SIGQUEUEINFO => sys_rt_sigqueueinfo(
            args[0] as isize,
            args[1] as u32,
            args[2] as *const LinuxSigInfo,
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
        SYSCALL_SETHOSTNAME => sys_sethostname_ctx(ctx, args[0] as *const u8, args[1]),
        SYSCALL_SETDOMAINNAME => sys_setdomainname_ctx(ctx, args[0] as *const u8, args[1]),
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
        SYSCALL_SETTIMEOFDAY => sys_settimeofday_ctx(
            ctx,
            args[0] as *const LinuxTimeVal,
            args[1] as *const LinuxTimezone,
        ),
        SYSCALL_ADJTIMEX => sys_adjtimex(args[0] as *mut LinuxTimex),
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
        SYSCALL_BRK => sys_brk_ctx(ctx, args[0]),
        SYSCALL_MUNMAP => sys_munmap_ctx(ctx, args[0], args[1]),
        SYSCALL_MREMAP => sys_mremap(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_MPROTECT => sys_mprotect_ctx(ctx, args[0], args[1], args[2]),
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
        SYSCALL_MMAP => sys_mmap_ctx(ctx, args[0], args[1], args[2], args[3], args[4], args[5]),
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
        #[cfg(feature = "fanotify")]
        SYSCALL_FANOTIFY_INIT => sys_fanotify_init(args[0] as u32, args[1] as u32),
        #[cfg(feature = "fanotify")]
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
        SYSCALL_RT_TGSIGQUEUEINFO => sys_rt_tgsigqueueinfo(
            args[0] as isize,
            args[1] as isize,
            args[2] as u32,
            args[3] as *const LinuxSigInfo,
        ),
        SYSCALL_CLOCK_ADJTIME => sys_clock_adjtime(args[0] as i32, args[1] as *mut LinuxTimex),
        SYSCALL_SETNS => sys_setns(args[0], args[1]),
        SYSCALL_KCMP => sys_kcmp_ctx(
            ctx,
            args[0] as isize,
            args[1] as isize,
            args[2] as i32,
            args[3],
            args[4],
        ),
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
        SYSCALL_SENDMMSG => sys_sendmmsg(args[0], args[1], args[2], args[3] as i32),
        SYSCALL_RECVMSG => sys_recvmsg(args[0], args[1], args[2] as i32),
        SYSCALL_RECVMMSG => sys_recvmmsg(args[0], args[1], args[2], args[3] as i32, args[4]),
        _ => Err(Errno::ENOSYS),
    }
}
