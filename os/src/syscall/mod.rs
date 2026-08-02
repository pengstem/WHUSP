pub(crate) use crate::task::futex::{sys_futex, sys_get_robust_list, sys_set_robust_list};

mod context;
mod dispatch;
mod fs;
pub(crate) mod ipc_util;
mod kmodule;
mod memory;
pub(crate) mod msg;
mod net;
mod process;
#[cfg(feature = "read-mostly-probe")]
mod read_mostly_probe;
#[cfg(feature = "sleep-rwlock-probe")]
mod rwlock_probe;
pub(crate) mod sem;
mod signal;
pub(crate) mod time;
pub(crate) mod uapi;
pub(crate) mod user_ptr;
mod wait;

pub(crate) use context::SyscallContext;
pub(crate) use dispatch::{syscall_exit_with_current_task, syscall_with_current_task};
pub use dispatch::{syscall_is_exit, syscall_is_exit_group};
#[cfg(feature = "inotify")]
pub(crate) use fs::{
    INOTIFY_MAX_QUEUED_EVENTS, INOTIFY_MAX_USER_INSTANCES, INOTIFY_MAX_USER_WATCHES, inotify_fdinfo,
};
pub(crate) use fs::{
    close_detached_fd_entry, close_detached_fd_entry_for_process_teardown, install_file_fd,
    release_record_locks_for_process,
};
#[cfg(feature = "fanotify")]
pub(crate) use fs::{fanotify_evict_evictable_marks, fanotify_fdinfo, fanotify_max_queued_events};
pub(crate) use process::pidfd_fdinfo;
pub(crate) use process::{proc_sys_kernel_printk_content, write_proc_sys_kernel_printk};
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
pub(crate) use wait::LinuxSigInfo;
