use crate::{
    fs::File,
    syscall::SyscallContext,
    task::{
        CAP_SYS_PTRACE, Credentials, ProcessControlBlock, processes_snapshot, task_with_linux_tid,
    },
    uapi::errno::{Errno, KResult},
};
use alloc::sync::Arc;

const KCMP_FILE: i32 = 0;
const KCMP_VM: i32 = 1;
const KCMP_FILES: i32 = 2;
const KCMP_FS: i32 = 3;
const KCMP_SIGHAND: i32 = 4;
const KCMP_IO: i32 = 5;
const KCMP_SYSVSEM: i32 = 6;
const KCMP_EPOLL_TFD: i32 = 7;

fn kcmp_target_process(
    caller: &ProcessControlBlock,
    pid: isize,
) -> KResult<Arc<ProcessControlBlock>> {
    if pid <= 0 {
        return Err(Errno::ESRCH);
    }
    let visible_pid = pid as usize;
    let namespace = caller.pid_namespace();
    if let Some(process) = processes_snapshot()
        .into_iter()
        .find(|process| process.pid_visible_from_namespace(namespace) == Some(visible_pid))
    {
        return Ok(process);
    }
    task_with_linux_tid(visible_pid)
        .and_then(|task| task.process.upgrade())
        .ok_or(Errno::ESRCH)
}

fn can_kcmp(caller: &Credentials, target: &Credentials) -> bool {
    caller.is_root()
        || caller
            .capabilities
            .has_effective(CAP_SYS_PTRACE)
            .unwrap_or(false)
        || target.uid_matches_saved_set(caller.ruid)
        || target.uid_matches_saved_set(caller.euid)
}

fn file_for_kcmp(process: &ProcessControlBlock, fd: usize) -> KResult<Arc<dyn File + Send + Sync>> {
    process
        .inner_exclusive_access()
        .fd_entry(fd)
        .map(|entry| entry.file())
        .ok_or(Errno::EBADF)
}

pub fn sys_kcmp_ctx(
    ctx: &SyscallContext,
    pid1: isize,
    pid2: isize,
    kcmp_type: i32,
    idx1: usize,
    idx2: usize,
) -> KResult {
    let current = ctx.process();
    let process1 = kcmp_target_process(current, pid1)?;
    let process2 = kcmp_target_process(current, pid2)?;
    let caller_credentials = current.credentials();
    if !can_kcmp(&caller_credentials, &process1.credentials())
        || !can_kcmp(&caller_credentials, &process2.credentials())
    {
        return Err(Errno::EPERM);
    }

    match kcmp_type {
        KCMP_FILE => {
            let file1 = file_for_kcmp(&process1, idx1)?;
            let file2 = file_for_kcmp(&process2, idx2)?;
            Ok(if Arc::ptr_eq(&file1, &file2) { 0 } else { 1 })
        }
        KCMP_VM | KCMP_FILES | KCMP_FS | KCMP_SIGHAND | KCMP_IO | KCMP_SYSVSEM | KCMP_EPOLL_TFD => {
            Err(Errno::ENOTSUP)
        }
        _ => Err(Errno::EINVAL),
    }
}
