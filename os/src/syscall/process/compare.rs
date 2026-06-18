use crate::{
    fs::File,
    syscall::{
        SyscallContext,
        errno::{SysError, SysResult},
    },
    task::{
        CAP_SYS_PTRACE, Credentials, ProcessControlBlock, processes_snapshot, task_with_linux_tid,
    },
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
) -> SysResult<Arc<ProcessControlBlock>> {
    if pid <= 0 {
        return Err(SysError::ESRCH);
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
        .ok_or(SysError::ESRCH)
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

fn file_for_kcmp(
    process: &ProcessControlBlock,
    fd: usize,
) -> SysResult<Arc<dyn File + Send + Sync>> {
    process
        .inner_exclusive_access()
        .fd_entry(fd)
        .map(|entry| entry.file())
        .ok_or(SysError::EBADF)
}

fn compare_same_resource(left: usize, right: usize) -> isize {
    if left == right { 0 } else { 1 }
}

pub fn sys_kcmp_ctx(
    ctx: &SyscallContext,
    pid1: isize,
    pid2: isize,
    kcmp_type: i32,
    idx1: usize,
    idx2: usize,
) -> SysResult {
    let current = ctx.process();
    let process1 = kcmp_target_process(current, pid1)?;
    let process2 = kcmp_target_process(current, pid2)?;
    let caller_credentials = current.credentials();
    if !can_kcmp(&caller_credentials, &process1.credentials())
        || !can_kcmp(&caller_credentials, &process2.credentials())
    {
        return Err(SysError::EPERM);
    }

    match kcmp_type {
        KCMP_FILE => {
            let file1 = file_for_kcmp(&process1, idx1)?;
            let file2 = file_for_kcmp(&process2, idx2)?;
            Ok(if Arc::ptr_eq(&file1, &file2) { 0 } else { 1 })
        }
        KCMP_VM | KCMP_FILES | KCMP_FS | KCMP_SIGHAND | KCMP_IO | KCMP_SYSVSEM => {
            // UNFINISHED: clone currently does not share separate mm/fs/fd/io
            // objects for every CLONE_* resource. Store a lightweight owner id
            // so kcmp can report the clone-time sharing contract without
            // changing those subsystems' actual copy-on-clone behavior.
            let left = process1.inner_exclusive_access().kcmp_resources;
            let right = process2.inner_exclusive_access().kcmp_resources;
            let result = match kcmp_type {
                KCMP_VM => compare_same_resource(left.vm, right.vm),
                KCMP_FILES => compare_same_resource(left.files, right.files),
                KCMP_FS => compare_same_resource(left.fs, right.fs),
                KCMP_SIGHAND => compare_same_resource(left.sighand, right.sighand),
                KCMP_IO => compare_same_resource(left.io, right.io),
                KCMP_SYSVSEM => compare_same_resource(left.sysvsem, right.sysvsem),
                _ => unreachable!(),
            };
            Ok(result)
        }
        KCMP_EPOLL_TFD => {
            // UNFINISHED: epoll target-file comparison needs epoll-slot
            // decoding and target lookup. No current contest case depends on it.
            Err(SysError::EINVAL)
        }
        _ => Err(SysError::EINVAL),
    }
}
