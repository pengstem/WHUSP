use super::{
    ProcessControlBlock, SIGKILL, SIGNAL_INFO_SLOTS, SIGTRAP, SignalFlags, SignalInfo,
    TaskControlBlock, TaskStatus, block_current_task_no_schedule, current_process, current_task,
    pid2process, queue_signal_to_task, remove_ready_tasks_of_process, schedule, wakeup_task,
};
use crate::syscall::errno::{SysError, SysResult};
use alloc::sync::Arc;

const STOPPED_WAIT_LOW_BITS: i32 = 0x7f;
const PTRACE_O_TRACESYSGOOD: usize = 1;
pub(crate) const PTRACE_SYSCALL_INFO_ENTRY: u8 = 1;
pub(crate) const PTRACE_SYSCALL_INFO_EXIT: u8 = 2;

fn stopped_wait_status(signum: i32) -> i32 {
    (signum << 8) | STOPPED_WAIT_LOW_BITS
}

fn wake_waiters_for_process(process: &Arc<ProcessControlBlock>) {
    for task in process.tasks_snapshot() {
        let blocked = task.inner_exclusive_access().task_status == TaskStatus::Blocked;
        if blocked {
            wakeup_task(task);
        }
    }
}

fn wake_waiters_for_pid(pid: usize) {
    if let Some(process) = pid2process(pid) {
        wake_waiters_for_process(&process);
    }
}

pub(crate) fn ptrace_is_traced(process: &Arc<ProcessControlBlock>) -> bool {
    process.inner_exclusive_access().ptrace.tracer_pid.is_some()
}

pub(crate) fn ptrace_traceme_current() -> SysResult {
    let process = current_process();
    let tracer_pid = process
        .parent_process()
        .map(|parent| parent.getpid())
        .ok_or(SysError::EPERM)?;
    let mut inner = process.inner_exclusive_access();
    if inner.ptrace.tracer_pid.is_some() {
        return Err(SysError::EPERM);
    }
    inner.ptrace.tracer_pid = Some(tracer_pid);
    inner.ptrace.options = 0;
    inner.ptrace.syscall_trace = false;
    inner.ptrace.syscall_stop = None;
    Ok(0)
}

pub(crate) fn ptrace_validate_tracee(
    tracee: &Arc<ProcessControlBlock>,
    tracer_pid: usize,
    require_stopped: bool,
) -> SysResult<Arc<TaskControlBlock>> {
    let task = tracee.main_task();
    let inner = tracee.inner_exclusive_access();
    if inner.is_zombie || inner.ptrace.tracer_pid != Some(tracer_pid) {
        return Err(SysError::ESRCH);
    }
    if require_stopped && !inner.ptrace.stopped {
        return Err(SysError::ESRCH);
    }
    drop(inner);
    Ok(task)
}

pub(crate) fn ptrace_attach_process(
    tracee: &Arc<ProcessControlBlock>,
    tracer_pid: usize,
) -> SysResult {
    if tracee.getpid() == tracer_pid {
        return Err(SysError::EPERM);
    }
    let task = tracee.main_task();
    {
        let mut inner = tracee.inner_exclusive_access();
        if inner.is_zombie {
            return Err(SysError::ESRCH);
        }
        if inner.ptrace.tracer_pid.is_some() {
            return Err(SysError::EPERM);
        }
        inner.ptrace.tracer_pid = Some(tracer_pid);
        inner.ptrace.stopped = true;
        inner.ptrace.stop_signal = Some(super::SIGSTOP);
        inner.ptrace.wait_stop_status = Some(super::SIGSTOP as i32);
        inner.ptrace.options = 0;
        inner.ptrace.syscall_trace = false;
        inner.ptrace.syscall_stop = None;
    }
    remove_ready_tasks_of_process(tracee.getpid());
    {
        let mut task_inner = task.inner_exclusive_access();
        if task_inner.task_status != TaskStatus::Exited {
            task_inner.task_status = TaskStatus::Blocked;
        }
    }
    wake_waiters_for_pid(tracer_pid);
    Ok(0)
}

pub(crate) fn ptrace_resume_process(
    tracee: &Arc<ProcessControlBlock>,
    tracer_pid: usize,
    signum: u32,
    detach: bool,
    syscall_trace: bool,
) -> SysResult {
    let signal = if signum == 0 {
        None
    } else if signum == SIGTRAP || signum == (SIGTRAP | 0x80) {
        // CONTEXT: strace may pass the reported stop signal back into a
        // restart request. Linux ignores signal injection for synthetic exec
        // and syscall ptrace stops, so do not turn those SIGTRAP stops into a
        // fatal signal for the tracee.
        None
    } else if signum as usize >= SIGNAL_INFO_SLOTS {
        return Err(SysError::EIO);
    } else {
        Some(SignalFlags::from_signum(signum).ok_or(SysError::EIO)?)
    };
    let task = ptrace_validate_tracee(tracee, tracer_pid, true)?;
    {
        let mut inner = tracee.inner_exclusive_access();
        inner.ptrace.stopped = false;
        inner.ptrace.stop_signal = None;
        inner.ptrace.wait_stop_status = None;
        inner.ptrace.syscall_stop = None;
        inner.ptrace.syscall_trace = syscall_trace && !detach;
        if detach {
            inner.ptrace.tracer_pid = None;
            inner.ptrace.options = 0;
        }
    }
    if let Some(signal) = signal
        && !signal.is_empty()
    {
        queue_signal_to_task(
            Arc::clone(&task),
            signal,
            SignalInfo::user(signum as i32, tracer_pid as i32),
        );
    }
    wakeup_task(task);
    Ok(0)
}

pub(crate) fn ptrace_kill_process(
    tracee: &Arc<ProcessControlBlock>,
    tracer_pid: usize,
) -> SysResult {
    let task = ptrace_validate_tracee(tracee, tracer_pid, false)?;
    {
        let mut inner = tracee.inner_exclusive_access();
        inner.ptrace.stopped = false;
        inner.ptrace.stop_signal = None;
        inner.ptrace.wait_stop_status = None;
        inner.ptrace.syscall_trace = false;
        inner.ptrace.syscall_stop = None;
    }
    queue_signal_to_task(
        Arc::clone(&task),
        SignalFlags::SIGKILL,
        SignalInfo::user(SIGKILL as i32, tracer_pid as i32),
    );
    wakeup_task(task);
    Ok(0)
}

pub(crate) fn ptrace_note_exec_current() {
    let process = current_process();
    if !ptrace_is_traced(&process) {
        return;
    }
    if let Some(task) = current_task() {
        queue_signal_to_task(
            task,
            SignalFlags::SIGTRAP,
            SignalInfo::user(SIGTRAP as i32, 0),
        );
    }
}

fn take_ptrace_stop_signal() -> Option<usize> {
    let task = current_task()?;
    let process = current_process();
    let tracer_pid = process.inner_exclusive_access().ptrace.tracer_pid?;
    let (signum, signal) = {
        let task_inner = task.inner_exclusive_access();
        let pending = SignalFlags::from_bits_retain(
            task_inner.pending_signals.bits() & !task_inner.signal_mask.bits(),
        );
        let signum = pending.bits().trailing_zeros();
        if signum as usize >= SIGNAL_INFO_SLOTS {
            return None;
        }
        let signal = SignalFlags::from_signum(signum)?;
        if signal.is_empty() || signum == SIGKILL {
            return None;
        }
        (signum, signal)
    };
    {
        let mut task_inner = task.inner_exclusive_access();
        if !task_inner.pending_signals.contains(signal) {
            return None;
        }
        task_inner.clear_pending(signum);
    }
    {
        let mut process_inner = process.inner_exclusive_access();
        if process_inner.ptrace.tracer_pid != Some(tracer_pid) {
            return None;
        }
        process_inner.ptrace.stopped = true;
        process_inner.ptrace.stop_signal = Some(signum);
        process_inner.ptrace.wait_stop_status = Some(signum as i32);
        process_inner.ptrace.syscall_stop = None;
    }
    Some(tracer_pid)
}

fn ptrace_syscall_stop_current(
    op: u8,
    nr: usize,
    args: [usize; 6],
    rval: isize,
    instruction_pointer: usize,
    stack_pointer: usize,
) -> bool {
    let Some(task) = current_task() else {
        return false;
    };
    let Some(process) = task.process.upgrade() else {
        return false;
    };
    let tracer_pid = {
        let mut inner = process.inner_exclusive_access();
        let Some(tracer_pid) = inner.ptrace.tracer_pid else {
            return false;
        };
        if !inner.ptrace.syscall_trace {
            return false;
        }
        let stop_signal = if inner.ptrace.options & PTRACE_O_TRACESYSGOOD != 0 {
            SIGTRAP | 0x80
        } else {
            SIGTRAP
        };
        inner.ptrace.stopped = true;
        inner.ptrace.stop_signal = Some(stop_signal);
        inner.ptrace.wait_stop_status = Some(stop_signal as i32);
        inner.ptrace.syscall_stop = Some(super::process::PtraceSyscallStop {
            op,
            nr,
            args,
            rval,
            is_error: rval < 0,
            instruction_pointer,
            stack_pointer,
        });
        tracer_pid
    };
    // CONTEXT: Publish the ptrace stop before blocking this task. wait4()
    // consumes the stored stop state after the tracer is woken.
    let (_task, task_cx_ptr) = block_current_task_no_schedule();
    wake_waiters_for_pid(tracer_pid);
    schedule(task_cx_ptr);
    true
}

pub(crate) fn ptrace_syscall_enter_stop_current(
    nr: usize,
    args: [usize; 6],
    instruction_pointer: usize,
    stack_pointer: usize,
) -> bool {
    ptrace_syscall_stop_current(
        PTRACE_SYSCALL_INFO_ENTRY,
        nr,
        args,
        0,
        instruction_pointer,
        stack_pointer,
    )
}

pub(crate) fn ptrace_syscall_exit_stop_current(
    rval: isize,
    instruction_pointer: usize,
    stack_pointer: usize,
) -> bool {
    ptrace_syscall_stop_current(
        PTRACE_SYSCALL_INFO_EXIT,
        0,
        [0; 6],
        rval,
        instruction_pointer,
        stack_pointer,
    )
}

pub(crate) fn ptrace_stop_current_if_needed() -> bool {
    let Some(tracer_pid) = take_ptrace_stop_signal() else {
        return false;
    };
    let (_task, task_cx_ptr) = block_current_task_no_schedule();
    wake_waiters_for_pid(tracer_pid);
    schedule(task_cx_ptr);
    true
}

pub(crate) fn ptrace_take_wait_status(
    tracee: &Arc<ProcessControlBlock>,
    waiter_pid: usize,
    include_job_control: bool,
) -> Option<i32> {
    let mut inner = tracee.inner_exclusive_access();
    if inner.ptrace.tracer_pid == Some(waiter_pid)
        && let Some(signum) = inner.ptrace.wait_stop_status.take()
    {
        return Some(stopped_wait_status(signum));
    }
    if include_job_control {
        inner.wait_stop_status.take().map(stopped_wait_status)
    } else {
        None
    }
}
