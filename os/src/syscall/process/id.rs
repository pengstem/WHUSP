use crate::sync::SpinNoIrqLock;
use crate::syscall::SyscallContext;
use crate::task::{
    ProcessControlBlock, SignalFlags, SignalInfo, current_process, current_task,
    exit_current_and_run_next, exit_current_group_and_run_next,
    notify_if_orphaned_stopped_process_group, pid2process, processes_snapshot,
    queue_signal_to_process as task_queue_signal_to_process, suspend_current_and_run_next,
};
use crate::uapi::errno::{Errno, KResult};
use alloc::{sync::Arc, vec::Vec};

pub fn sys_exit(exit_code: i32) -> ! {
    exit_current_and_run_next(exit_code & 0xff)
}

pub fn sys_exit_group(exit_code: i32) -> ! {
    exit_current_group_and_run_next(exit_code & 0xff)
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
    let pgid = target.process_group_id();
    pid2process(pgid)
        .and_then(|leader| leader.pid_visible_from_namespace(namespace))
        .or_else(|| {
            // A process group outlives its leader. Internal PIDs are monotonic,
            // so the numeric group identity remains stable until the last
            // member leaves even after the leader has been reaped.
            processes_snapshot()
                .into_iter()
                .find(|member| {
                    member.process_group_id() == pgid
                        && member.pid_visible_from_namespace(namespace).is_some()
                })
                .map(|_| pgid)
        })
        .unwrap_or(0)
}

fn visible_session_id(
    target: &Arc<ProcessControlBlock>,
    caller: &Arc<ProcessControlBlock>,
) -> usize {
    let sid = target.session_id();
    let namespace = caller.pid_namespace();
    pid2process(sid)
        .and_then(|leader| leader.pid_visible_from_namespace(namespace))
        .or_else(|| {
            processes_snapshot()
                .into_iter()
                .find(|member| {
                    member.session_id() == sid
                        && member.pid_visible_from_namespace(namespace).is_some()
                })
                .map(|_| sid)
        })
        .unwrap_or(0)
}

fn process_group_session(pgid: usize) -> Option<usize> {
    processes_snapshot()
        .into_iter()
        .find(|process| process.process_group_id() == pgid)
        .map(|process| process.session_id())
}

fn process_group_from_visible_id(
    caller: &Arc<ProcessControlBlock>,
    visible_pgid: usize,
) -> Option<(usize, usize)> {
    let namespace = caller.pid_namespace();
    let processes = processes_snapshot();
    processes.iter().find_map(|member| {
        let pgid = member.process_group_id();
        let visible = pid2process(pgid)
            .and_then(|leader| leader.pid_visible_from_namespace(namespace))
            .or_else(|| member.pid_visible_from_namespace(namespace).map(|_| pgid));
        (visible == Some(visible_pgid)).then(|| (pgid, member.session_id()))
    })
}

// Serializes setpgid()/setsid() validation with their membership commit. The
// current numeric identity model is safe from ABA because Linux-visible PIDs
// are allocated monotonically and are deliberately not recycled.
static JOB_CONTROL_IDENTITY_LOCK: SpinNoIrqLock<()> = SpinNoIrqLock::new(());

pub fn sys_setpgid_ctx(ctx: &SyscallContext, pid: isize, pgid: isize) -> KResult {
    if pid < 0 || pgid < 0 {
        return Err(Errno::EINVAL);
    }
    let current = ctx.process();
    let target_pid = if pid == 0 {
        current.getpid()
    } else {
        pid as usize
    };
    let target = if target_pid == current.getpid() {
        Arc::clone(current)
    } else {
        process_from_visible_pid(current, target_pid).ok_or(Errno::ESRCH)?
    };
    let target_is_caller = Arc::ptr_eq(&target, current);
    if !target_is_caller
        && !target
            .parent_process()
            .is_some_and(|parent| Arc::ptr_eq(&parent, current))
    {
        return Err(Errno::ESRCH);
    }

    let identity = JOB_CONTROL_IDENTITY_LOCK.lock();
    let (_, caller_sid, _) = current.job_control_identity();
    let (old_pgid, target_sid, target_did_exec) = target.job_control_identity();
    if !target_is_caller && target_did_exec {
        return Err(Errno::EACCES);
    }
    if target_sid != caller_sid {
        return Err(Errno::EPERM);
    }
    if target_sid == target.getpid() {
        return Err(Errno::EPERM);
    }

    let target_visible_pid = target
        .pid_visible_from_namespace(current.pid_namespace())
        .ok_or(Errno::ESRCH)?;
    let new_pgid = if pgid == 0 || pgid as usize == target_visible_pid {
        target.getpid()
    } else {
        let (existing_pgid, existing_sid) =
            process_group_from_visible_id(current, pgid as usize).ok_or(Errno::EPERM)?;
        if existing_sid != target_sid {
            return Err(Errno::EPERM);
        }
        existing_pgid
    };
    if let Some(existing_sid) = process_group_session(new_pgid)
        && existing_sid != target_sid
    {
        return Err(Errno::EPERM);
    }
    target.set_process_group_identity(new_pgid, target_sid);
    drop(identity);
    if old_pgid != new_pgid {
        notify_if_orphaned_stopped_process_group(old_pgid, target_sid);
        notify_if_orphaned_stopped_process_group(new_pgid, target_sid);
    }
    Ok(0)
}

pub fn sys_getpgid_ctx(ctx: &SyscallContext, pid: isize) -> KResult {
    if pid < 0 {
        return Err(Errno::ESRCH);
    }
    let current = ctx.process();
    let target = if pid == 0 || pid as usize == current.getpid() {
        Arc::clone(current)
    } else {
        process_from_visible_pid(current, pid as usize).ok_or(Errno::ESRCH)?
    };
    Ok(visible_process_group_id(&target, current) as isize)
}

pub fn sys_getsid_ctx(ctx: &SyscallContext, pid: isize) -> KResult {
    if pid < 0 {
        return Err(Errno::ESRCH);
    }
    let current = ctx.process();
    let target = if pid == 0 || pid as usize == current.getpid() {
        Arc::clone(current)
    } else {
        process_from_visible_pid(current, pid as usize).ok_or(Errno::ESRCH)?
    };
    Ok(visible_session_id(&target, current) as isize)
}

pub fn sys_setsid() -> KResult {
    let current = current_process();
    let pid = current.getpid();
    let identity = JOB_CONTROL_IDENTITY_LOCK.lock();
    if processes_snapshot()
        .iter()
        .any(|process| process.process_group_id() == pid)
    {
        return Err(Errno::EPERM);
    }
    let (old_pgid, old_sid, _) = current.job_control_identity();
    current.set_process_group_identity(pid, pid);
    current.set_controlling_tty_detached(false);
    drop(identity);
    notify_if_orphaned_stopped_process_group(old_pgid, old_sid);
    Ok(current.visible_pid() as isize)
}

pub fn sys_set_tid_address_ctx(ctx: &SyscallContext, tidptr: usize) -> KResult {
    let tid = ctx.task().linux_tid();
    ctx.task().inner_exclusive_access().clear_child_tid =
        if tidptr == 0 { None } else { Some(tidptr) };
    Ok(tid as isize)
}

fn caller_can_signal_target(
    caller: &Arc<ProcessControlBlock>,
    target: &Arc<ProcessControlBlock>,
    signal: SignalFlags,
) -> bool {
    if signal.contains(SignalFlags::SIGCONT) && caller.session_id() == target.session_id() {
        return true;
    }
    // UNFINISHED: Linux kill permission also checks CAP_KILL in the target's
    // user namespace. This kernel currently has one credential namespace and
    // process-wide credentials.
    let caller = caller.credentials();
    let target = target.credentials();
    caller.can_signal(&target)
}

fn queue_signal_to_process(
    process: &Arc<ProcessControlBlock>,
    signal: SignalFlags,
    info: SignalInfo,
) {
    if signal.is_empty() {
        return;
    }
    task_queue_signal_to_process(process, signal, info);
}

fn kill_targets(
    pid: isize,
    caller: &Arc<ProcessControlBlock>,
) -> KResult<Vec<Arc<ProcessControlBlock>>> {
    let caller_namespace = caller.pid_namespace();
    if pid > 0 {
        return Ok(alloc::vec![
            processes_snapshot()
                .into_iter()
                .find(|process| {
                    process.pid_visible_from_namespace(caller_namespace) == Some(pid as usize)
                })
                .ok_or(Errno::ESRCH)?
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
    let pgid = pid.checked_neg().ok_or(Errno::EINVAL)? as usize;
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

pub fn sys_kill(pid: isize, signal: u32) -> KResult {
    let flag = SignalFlags::from_signum(signal).ok_or(Errno::EINVAL)?;
    let current = current_process();
    let targets = kill_targets(pid, &current)?;
    if targets.is_empty() {
        return Err(Errno::ESRCH);
    }

    let mut permitted = false;
    for process in targets {
        if !caller_can_signal_target(&current, &process, flag) {
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
        return Err(Errno::EPERM);
    }
    Ok(0)
}
