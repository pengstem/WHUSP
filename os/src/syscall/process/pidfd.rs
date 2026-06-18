use crate::fs::{File, FileStat, OpenFlags};
use crate::mm::UserBuffer;
use crate::syscall::LinuxSigInfo;
use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::install_file_fd;
use crate::syscall::user_ptr::read_user_value;
use crate::task::{
    Credentials, FdFlags, FdTableEntry, ProcessControlBlock, SignalFlags, SignalInfo,
    current_process, current_user_token, pid2process, queue_signal_to_task, wakeup_task,
};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::any::Any;

const PIDFD_NONBLOCK: u32 = OpenFlags::NONBLOCK.bits();

struct PidFdFile {
    pid: usize,
}

impl PidFdFile {
    fn new(pid: usize) -> Self {
        Self { pid }
    }
}

impl File for PidFdFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        false
    }

    fn writable(&self) -> bool {
        false
    }

    fn read(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn write(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn stat(&self) -> crate::fs::FsResult<FileStat> {
        Ok(FileStat::default())
    }
}

fn install_pidfd_with_flags(pid: usize, open_flags: OpenFlags) -> SysResult<usize> {
    install_file_fd(Arc::new(PidFdFile::new(pid)), open_flags, None).map(|fd| fd as usize)
}

pub(super) fn reserve_pidfd_for_current_process() -> SysResult<usize> {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    inner.alloc_fd_from(0).ok_or(SysError::EMFILE)
}

pub(super) fn install_reserved_pidfd_for_current_process(fd: usize, pid: usize) {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let previous = inner.set_fd_entry(
        fd,
        FdTableEntry::from_file(Arc::new(PidFdFile::new(pid)), OpenFlags::CLOEXEC),
    );
    debug_assert!(previous.is_none());
}

pub(crate) fn install_pidfd_for_fanotify(pid: usize) -> SysResult<usize> {
    pid2process(pid).ok_or(SysError::ESRCH)?;
    install_pidfd_with_flags(pid, OpenFlags::CLOEXEC)
}

pub(crate) fn pidfd_fdinfo(file: &Arc<dyn File + Send + Sync>, flags: u32) -> Option<String> {
    let pidfd = file.as_any().downcast_ref::<PidFdFile>()?;
    Some(format!(
        "pos:\t0\nflags:\t{flags:o}\nmnt_id:\t0\nPid:\t{}\nNSpid:\t{}\n",
        pidfd.pid, pidfd.pid
    ))
}

fn pid_from_fd(pidfd: usize) -> SysResult<usize> {
    let entry = {
        let process = current_process();
        let inner = process.inner_exclusive_access();
        inner
            .fd_table
            .get(pidfd)
            .and_then(|entry| entry.as_ref())
            .cloned()
            .ok_or(SysError::EBADF)?
    };
    let file = entry.file();
    file.as_any()
        .downcast_ref::<PidFdFile>()
        .map(|pidfd| pidfd.pid)
        .ok_or(SysError::EBADF)
}

fn caller_can_signal_target(caller: &Credentials, target: &Credentials) -> bool {
    // UNFINISHED: Linux also checks CAP_KILL in the target user namespace and
    // PID namespace reachability. This kernel has one credential/PID namespace.
    caller.is_root()
        || target.uid_matches_saved_set(caller.ruid)
        || target.uid_matches_saved_set(caller.euid)
}

fn queue_signal_to_process(
    process: &Arc<ProcessControlBlock>,
    signal: SignalFlags,
    info: SignalInfo,
) {
    if signal.is_empty() {
        return;
    }
    let target = {
        let tasks = process.tasks_snapshot();
        tasks
            .iter()
            .find(|task| {
                let task_inner = task.inner_exclusive_access();
                !(task_inner.signal_mask & signal).contains(signal)
            })
            .cloned()
            .or_else(|| tasks.first().cloned())
    };
    if let Some(task) = target {
        queue_signal_to_task(task, signal, info);
    }
    if signal.check_error().is_some() {
        for task in process.tasks_snapshot() {
            wakeup_task(task);
        }
    }
}

fn signal_info_from_user(signum: u32, info: *const LinuxSigInfo) -> SysResult<SignalInfo> {
    let sender_pid = current_process().getpid() as i32;
    if info.is_null() {
        return Ok(SignalInfo::user(signum as i32, sender_pid));
    }
    let info = read_user_value(current_user_token(), info)?;
    let info = info.to_signal_info(signum, sender_pid);
    if info.signo != signum as i32 {
        return Err(SysError::EINVAL);
    }
    Ok(info)
}

pub fn sys_pidfd_send_signal(
    pidfd: usize,
    signum: u32,
    info: *const LinuxSigInfo,
    flags: u32,
) -> SysResult {
    if flags != 0 {
        return Err(SysError::EINVAL);
    }
    let signal = SignalFlags::from_signum(signum).ok_or(SysError::EINVAL)?;
    let pid = pid_from_fd(pidfd)?;
    let target = pid2process(pid).ok_or(SysError::ESRCH)?;

    let sender = current_process();
    if !caller_can_signal_target(&sender.credentials(), &target.credentials()) {
        return Err(SysError::EPERM);
    }
    let info = signal_info_from_user(signum, info)?;
    queue_signal_to_process(&target, signal, info);
    Ok(0)
}

pub fn sys_pidfd_getfd(pidfd: i32, target_fd: i32, flags: u32) -> SysResult {
    if flags != 0 {
        return Err(SysError::EINVAL);
    }
    if pidfd < 0 || target_fd < 0 {
        return Err(SysError::EBADF);
    }

    let pid = pid_from_fd(pidfd as usize)?;
    let target = pid2process(pid).ok_or(SysError::ESRCH)?;

    let caller = current_process();
    // UNFINISHED: Linux gates pidfd_getfd() with a
    // PTRACE_MODE_ATTACH_REALCREDS ptrace check. This kernel has no complete
    // ptrace access-mode implementation yet, so reuse the existing pidfd
    // credential check in the single user/PID namespace.
    if !caller_can_signal_target(&caller.credentials(), &target.credentials()) {
        return Err(SysError::EPERM);
    }

    let source_entry = {
        let inner = target.inner_exclusive_access();
        inner.fd_entry(target_fd as usize).ok_or(SysError::EBADF)?
    };
    let new_fd = {
        let mut inner = caller.inner_exclusive_access();
        let fd = inner.alloc_fd_from(0).ok_or(SysError::EMFILE)?;
        let previous = inner.set_fd_entry(fd, source_entry.duplicate(FdFlags::CLOEXEC));
        debug_assert!(previous.is_none());
        fd
    };
    Ok(new_fd as isize)
}

pub fn sys_pidfd_open(pid: usize, flags: u32) -> SysResult {
    if flags & !PIDFD_NONBLOCK != 0 {
        return Err(SysError::EINVAL);
    }
    pid2process(pid).ok_or(SysError::ESRCH)?;
    let mut open_flags = OpenFlags::CLOEXEC;
    if flags & PIDFD_NONBLOCK != 0 {
        open_flags |= OpenFlags::NONBLOCK;
    }
    Ok(install_pidfd_with_flags(pid, open_flags)? as isize)
}
