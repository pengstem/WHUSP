use crate::syscall::errno::{SysError, SysResult};
use crate::task::{
    Credentials, ProcessControlBlock, SignalFlags, SignalInfo, current_process, current_task,
    exit_current_and_run_next, exit_current_group_and_run_next, pid2process, processes_snapshot,
    queue_signal_to_task, suspend_current_and_run_next, wakeup_task,
};
use alloc::{sync::Arc, vec::Vec};

pub fn sys_exit(exit_code: i32) -> ! {
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

pub fn sys_exit_group(exit_code: i32) -> ! {
    if current_task()
        .map(|task| task.inner_exclusive_access().clone_vm_process_helper)
        .unwrap_or(false)
    {
        // CONTEXT: CLONE_VM process-compatibility children run as same-process
        // helper tasks. libc _exit() may issue exit_group(), but Linux would
        // terminate only the distinct cloned child process, so keep the parent
        // process alive and release the helper task instead.
        exit_current_and_run_next(exit_code);
    }
    exit_current_group_and_run_next(exit_code);
    panic!("Unreachable in sys_exit_group!");
}

pub fn sys_sched_yield() -> isize {
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    current_process().visible_pid() as isize
}

pub fn sys_gettid() -> isize {
    current_task()
        .expect("gettid requires a current task")
        .linux_tid() as isize
}

pub fn sys_getppid() -> isize {
    // UNFINISHED: PID namespace parent translation and child subreapers are
    // not modeled yet, so
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
    let task = current_task().expect("set_tid_address requires a current task");
    let tid = task.linux_tid();
    task.inner_exclusive_access().clear_child_tid = if tidptr == 0 { None } else { Some(tidptr) };
    Ok(tid as isize)
}

fn caller_can_signal_target(caller: &Credentials, target: &Credentials) -> bool {
    // UNFINISHED: Linux kill permission also checks CAP_KILL in the target's
    // user namespace and has a SIGCONT session rule. This kernel currently has
    // one credential namespace and process-wide credentials.
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

fn kill_targets(pid: isize) -> SysResult<Vec<Arc<ProcessControlBlock>>> {
    if pid > 0 {
        return Ok(alloc::vec![
            pid2process(pid as usize).ok_or(SysError::ESRCH)?
        ]);
    }
    if pid == 0 {
        let pgid = current_process().process_group_id();
        return Ok(processes_snapshot()
            .into_iter()
            .filter(|process| process.process_group_id() == pgid)
            .collect());
    }
    if pid == -1 {
        return Ok(processes_snapshot()
            .into_iter()
            .filter(|process| process.getpid() != 1)
            .collect());
    }
    let pgid = pid.checked_neg().ok_or(SysError::EINVAL)? as usize;
    Ok(processes_snapshot()
        .into_iter()
        .filter(|process| process.process_group_id() == pgid)
        .collect())
}

pub fn sys_kill(pid: isize, signal: u32) -> SysResult {
    let flag = SignalFlags::from_signum(signal).ok_or(SysError::EINVAL)?;
    let current = current_process();
    let sender_pid = current.getpid() as i32;
    let sender_credentials = current.credentials();
    let targets = kill_targets(pid)?;
    if targets.is_empty() {
        return Err(SysError::ESRCH);
    }

    let mut permitted = false;
    for process in targets {
        let target_credentials = process.credentials();
        if !caller_can_signal_target(&sender_credentials, &target_credentials) {
            continue;
        }
        permitted = true;
        queue_signal_to_process(&process, flag, SignalInfo::user(signal as i32, sender_pid));
    }

    if !permitted {
        return Err(SysError::EPERM);
    }
    Ok(0)
}
