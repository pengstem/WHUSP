mod aio;
mod context;
mod dispatch;
mod fs;
mod futex;
pub(crate) mod keyring;
mod kmodule;
mod memory;
pub(crate) mod msg;
pub(crate) use crate::net::socket::syscall_adapter as net;
mod process;
#[cfg(feature = "read-mostly-probe")]
mod read_mostly_probe;
#[cfg(feature = "sleep-rwlock-probe")]
mod rwlock_probe;
mod seccomp;
pub(crate) mod sem;
mod signal;
pub(crate) mod time;
pub(crate) mod uapi;
pub(crate) mod user_ptr;
mod wait;

pub(crate) use aio::aio_max_nr_content;
pub(crate) use context::SyscallContext;
#[allow(unused_imports)]
pub(crate) use dispatch::{
    SyscallOutcome, syscall_exit_with_current_task, syscall_with_context, syscall_with_current_task,
};
pub use dispatch::{syscall_is_exit, syscall_is_exit_group};
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
