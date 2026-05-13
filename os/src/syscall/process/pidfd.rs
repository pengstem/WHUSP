use crate::fs::{File, FileStat, OpenFlags};
use crate::mm::UserBuffer;
use crate::syscall::LinuxSigInfo;
use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::install_file_fd;
use crate::syscall::user_ptr::read_user_value;
use crate::task::{
    Credentials, ProcessControlBlock, SignalFlags, SignalInfo, current_process, current_user_token,
    pid2process, queue_signal_to_task, wakeup_task,
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

pub(super) fn install_pidfd_for_current_process(pid: usize) -> SysResult<usize> {
    install_pidfd_with_flags(pid, OpenFlags::CLOEXEC)
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
