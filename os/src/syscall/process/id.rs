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
    let is_clone_vm_process_helper = current_task()
        .map(|task| task.inner_exclusive_access().clone_vm_process_helper)
        .unwrap_or(false);
    if is_clone_vm_process_helper {
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
    current_process().getppid() as isize
}

fn process_from_visible_pid(
    caller: &Arc<ProcessControlBlock>,
    pid: usize,
) -> Option<Arc<ProcessControlBlock>> {
    let namespace = caller.pid_namespace();
    processes_snapshot()
        .into_iter()
        .find(|process| process.pid_visible_from_namespace(namespace) == Some(pid))
}

fn visible_process_group_id(
    target: &Arc<ProcessControlBlock>,
    caller: &Arc<ProcessControlBlock>,
) -> usize {
    let namespace = caller.pid_namespace();
    pid2process(target.process_group_id())
        .and_then(|leader| leader.pid_visible_from_namespace(namespace))
        .unwrap_or(0)
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
        Arc::clone(&current)
    } else {
        process_from_visible_pid(&current, target_pid).ok_or(SysError::ESRCH)?
    };
    let new_pgid = if pgid == 0 {
        target.getpid()
    } else {
        process_from_visible_pid(&current, pgid as usize)
            .map(|process| process.getpid())
            .unwrap_or(pgid as usize)
    };
    // UNFINISHED: Linux setpgid enforces sessions, exec-time constraints, and
    // parent/child relationship checks. This compatibility layer exists first
    // to satisfy libc/LTP harness calls such as setpgid(0, 0).
    target.set_process_group_id(new_pgid);
    Ok(0)
}

pub fn sys_getpgid(pid: isize) -> SysResult {
    if pid < 0 {
        return Err(SysError::ESRCH);
    }
    let current = current_process();
    let target = if pid == 0 || pid as usize == current.getpid() {
        Arc::clone(&current)
    } else {
        process_from_visible_pid(&current, pid as usize).ok_or(SysError::ESRCH)?
    };
    Ok(visible_process_group_id(&target, &current) as isize)
}

pub fn sys_getsid(pid: isize) -> SysResult {
    if pid < 0 {
        return Err(SysError::EINVAL);
    }
    let current = current_process();
    let target = if pid == 0 || pid as usize == current.getpid() {
        Arc::clone(&current)
    } else {
        process_from_visible_pid(&current, pid as usize).ok_or(SysError::ESRCH)?
    };
    // UNFINISHED: A distinct session ID is not modeled yet. The existing
    // process-group leader value gives the Linux-visible PID namespace behavior
    // needed by getsid()/setsid() tests.
    Ok(visible_process_group_id(&target, &current) as isize)
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
    Ok(current.visible_pid() as isize)
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

fn kill_targets(
    pid: isize,
    caller: &Arc<ProcessControlBlock>,
) -> SysResult<Vec<Arc<ProcessControlBlock>>> {
    let caller_namespace = caller.pid_namespace();
    if pid > 0 {
        return Ok(alloc::vec![
            processes_snapshot()
                .into_iter()
                .find(|process| {
                    process.pid_visible_from_namespace(caller_namespace) == Some(pid as usize)
                })
                .ok_or(SysError::ESRCH)?
        ]);
    }
    if pid == 0 {
        let pgid = current_process().process_group_id();
        return Ok(processes_snapshot()
            .into_iter()
            .filter(|process| {
                process.process_group_id() == pgid
                    && process
                        .pid_visible_from_namespace(caller_namespace)
                        .is_some()
            })
            .collect());
    }
    if pid == -1 {
        let caller_pid = caller.getpid();
        return Ok(processes_snapshot()
            .into_iter()
            .filter(|process| {
                let visible_pid = process.pid_visible_from_namespace(caller_namespace);
                visible_pid.is_some() && visible_pid != Some(1) && process.getpid() != caller_pid
            })
            .collect());
    }
    let pgid = pid.checked_neg().ok_or(SysError::EINVAL)? as usize;
    Ok(processes_snapshot()
        .into_iter()
        .filter(|process| {
            process.process_group_id() == pgid
                && process
                    .pid_visible_from_namespace(caller_namespace)
                    .is_some()
        })
        .collect())
}

fn signal_sender_pid_for_target(
    sender: &Arc<ProcessControlBlock>,
    target: &Arc<ProcessControlBlock>,
) -> i32 {
    sender
        .pid_visible_from_namespace(target.pid_namespace())
        .unwrap_or(0) as i32
}

fn signal_ignored_by_namespace_init(
    sender: &Arc<ProcessControlBlock>,
    target: &Arc<ProcessControlBlock>,
    signal: SignalFlags,
) -> bool {
    let sender_namespace = sender.pid_namespace();
    target.pid_namespace().id == sender_namespace.id
        && target.pid_visible_from_namespace(sender_namespace) == Some(1)
        && signal.check_error().is_some()
}

pub fn sys_kill(pid: isize, signal: u32) -> SysResult {
    let flag = SignalFlags::from_signum(signal).ok_or(SysError::EINVAL)?;
    let current = current_process();
    let sender_credentials = current.credentials();
    let targets = kill_targets(pid, &current)?;
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
        if signal_ignored_by_namespace_init(&current, &process, flag) {
            continue;
        }
        let sender_pid = signal_sender_pid_for_target(&current, &process);
        queue_signal_to_process(&process, flag, SignalInfo::user(signal as i32, sender_pid));
    }

    if !permitted {
        return Err(SysError::EPERM);
    }
    Ok(0)
}
