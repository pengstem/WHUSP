use crate::syscall::errno::{SysError, SysResult};
use crate::task::{
    SignalFlags, SignalInfo, current_process, current_task, exit_current_and_run_next,
    exit_current_group_and_run_next, pid2process, processes_snapshot, queue_signal_to_task,
    suspend_current_and_run_next, wakeup_task,
};

pub fn sys_exit(exit_code: i32) -> ! {
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

pub fn sys_exit_group(exit_code: i32) -> ! {
    exit_current_group_and_run_next(exit_code);
    panic!("Unreachable in sys_exit_group!");
}

pub fn sys_sched_yield() -> isize {
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    current_task().unwrap().process.upgrade().unwrap().getpid() as isize
}

pub fn sys_gettid() -> isize {
    current_task().unwrap().linux_tid() as isize
}

pub fn sys_getppid() -> isize {
    // UNFINISHED: PID namespaces and child subreapers are not modeled yet, so
    // this returns the single-namespace parent recorded in the PCB.
    current_process().getppid() as isize
}

pub fn sys_setpgid(pid: isize, pgid: isize) -> SysResult {
    if pid < 0 || pgid < 0 {
        return Err(SysError::EINVAL);
    }
    let current = current_process();
    let target_pid = if pid == 0 {
        current.getpid()
    } else {
        pid as usize
    };
    let target = if target_pid == current.getpid() {
        current
    } else {
        pid2process(target_pid).ok_or(SysError::ESRCH)?
    };
    let new_pgid = if pgid == 0 {
        target.getpid()
    } else {
        pgid as usize
    };
    // UNFINISHED: Linux setpgid enforces sessions, exec-time constraints, and
    // parent/child relationship checks. This compatibility layer exists first
    // to satisfy libc/LTP harness calls such as setpgid(0, 0).
    target.set_process_group_id(new_pgid);
    Ok(0)
}

pub fn sys_getpgid(pid: isize) -> SysResult {
    if pid < 0 {
        return Err(SysError::EINVAL);
    }
    let current = current_process();
    let target = if pid == 0 || pid as usize == current.getpid() {
        current
    } else {
        pid2process(pid as usize).ok_or(SysError::ESRCH)?
    };
    Ok(target.process_group_id() as isize)
}

pub fn sys_setsid() -> SysResult {
    let current = current_process();
    let pid = current.getpid();
    if processes_snapshot()
        .iter()
        .any(|process| process.process_group_id() == pid)
    {
        return Err(SysError::EPERM);
    }
    // UNFINISHED: This kernel does not yet store a separate session ID or
    // controlling-terminal state. Setting PGID to PID provides the Linux-visible
    // process-group effect needed by libc daemonization and LTP compatibility.
    current.set_process_group_id(pid);
    Ok(pid as isize)
}

pub fn sys_set_tid_address(tidptr: usize) -> SysResult {
    let task = current_task().unwrap();
    let tid = task.linux_tid();
    task.inner_exclusive_access().clear_child_tid = if tidptr == 0 { None } else { Some(tidptr) };
    Ok(tid as isize)
}

pub fn sys_kill(pid: usize, signal: u32) -> SysResult {
    let flag = SignalFlags::from_signum(signal).ok_or(SysError::EINVAL)?;
    let process = pid2process(pid).ok_or(SysError::ESRCH)?;
    if !flag.is_empty() {
        let sender_pid = current_process().getpid() as i32;
        let target = {
            let tasks = process.tasks_snapshot();
            tasks
                .iter()
                .find(|task| {
                    let task_inner = task.inner_exclusive_access();
                    !(task_inner.signal_mask & flag).contains(flag)
                })
                .cloned()
                .or_else(|| tasks.first().cloned())
        };
        if let Some(task) = target {
            queue_signal_to_task(task, flag, SignalInfo::user(signal as i32, sender_pid));
        }
    }
    if flag.check_error().is_some() {
        for task in process.tasks_snapshot() {
            wakeup_task(task);
        }
    }
    Ok(0)
}
