use crate::task::{
    ProcessControlBlock, SIGKILL, SIGNAL_INFO_SLOTS, SIGSTOP, SignalAction, SignalFlags,
    SignalInfo, TaskControlBlock, current_process, current_task, current_user_token, pid2process,
    processes_snapshot, queue_signal_to_task,
};
use crate::timer::get_time_ms;
use alloc::sync::Arc;

use super::errno::{SysError, SysResult};
use super::fs::LinuxTimeSpec;
use super::fs::user_ptr::{read_user_value, write_user_value};
use super::sync::relative_timeout_deadline_ms;
use super::wait::LinuxSigInfo;

const LINUX_RT_SIGSET_SIZE: usize = 8;
const SIG_BLOCK: usize = 0;
const SIG_UNBLOCK: usize = 1;
const SIG_SETMASK: usize = 2;

fn linux_sigset_to_flags(raw: u64) -> SignalFlags {
    SignalFlags::from_bits_retain((raw as u128) << 1)
}

fn flags_to_linux_sigset(flags: SignalFlags) -> u64 {
    (flags.bits() >> 1) as u64
}

fn read_signal_set(token: usize, set: *const u8, sigsetsize: usize) -> SysResult<SignalFlags> {
    if sigsetsize != LINUX_RT_SIGSET_SIZE {
        return Err(SysError::EINVAL);
    }
    let raw_set = read_user_value(token, set.cast::<u64>())?;
    let mut flags = linux_sigset_to_flags(raw_set);
    flags.remove(SignalFlags::SIGKILL);
    flags.remove(SignalFlags::SIGSTOP);
    Ok(flags)
}

fn lowest_signal(flags: SignalFlags) -> Option<u32> {
    if flags.is_empty() {
        None
    } else {
        Some(flags.bits().trailing_zeros())
    }
}

fn default_signal_info(signum: u32) -> SignalInfo {
    SignalInfo::user(signum as i32, 0)
}

fn peek_pending_signal(wanted: SignalFlags) -> Option<(u32, SignalInfo)> {
    let task = current_task()?;
    let inner = task.inner_exclusive_access();
    let signum = lowest_signal(inner.pending_signals & wanted)?;
    let info = inner
        .signal_infos
        .get(signum as usize)
        .copied()
        .flatten()
        .unwrap_or_else(|| default_signal_info(signum));
    Some((signum, info))
}

fn consume_pending_signal(signum: u32) {
    let Some(task) = current_task() else {
        return;
    };
    let mut inner = task.inner_exclusive_access();
    inner.clear_pending(signum);
}

fn try_return_pending_signal(
    token: usize,
    wanted: SignalFlags,
    info_ptr: *mut LinuxSigInfo,
) -> SysResult<Option<isize>> {
    let Some((signum, info)) = peek_pending_signal(wanted) else {
        return Ok(None);
    };

    if !info_ptr.is_null() {
        let info = LinuxSigInfo::from_signal_info(info);
        write_user_value(token, info_ptr, &info)?;
    }
    consume_pending_signal(signum);
    Ok(Some(signum as isize))
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxKernelSigAction {
    handler: usize,
    flags: usize,
    mask: u64,
}

impl From<SignalAction> for LinuxKernelSigAction {
    fn from(action: SignalAction) -> Self {
        Self {
            handler: action.handler,
            flags: action.flags,
            mask: flags_to_linux_sigset(action.mask),
        }
    }
}

fn signal_action_from_linux(raw: LinuxKernelSigAction) -> SignalAction {
    let mut mask = linux_sigset_to_flags(raw.mask);
    mask.remove(SignalFlags::SIGKILL);
    mask.remove(SignalFlags::SIGSTOP);
    SignalAction {
        handler: raw.handler,
        flags: raw.flags,
        mask,
    }
}

fn validate_kill_signum(signum: u32) -> SysResult<SignalFlags> {
    SignalFlags::from_signum(signum).ok_or(SysError::EINVAL)
}

fn validate_action_signum(signum: u32) -> SysResult<usize> {
    if signum == 0 || signum as usize >= SIGNAL_INFO_SLOTS {
        return Err(SysError::EINVAL);
    }
    Ok(signum as usize)
}

fn task_with_tid(process: &ProcessControlBlock, tid: usize) -> Option<Arc<TaskControlBlock>> {
    process
        .tasks_snapshot()
        .into_iter()
        .find(|task| task.linux_tid() == tid)
}

fn find_task_by_linux_tid(tid: usize) -> Option<(usize, Arc<TaskControlBlock>)> {
    for process in processes_snapshot() {
        if let Some(task) = task_with_tid(&process, tid) {
            return Some((process.getpid(), task));
        }
    }
    None
}

fn find_task_in_process_by_linux_tid(tgid: usize, tid: usize) -> Option<Arc<TaskControlBlock>> {
    let process = pid2process(tgid)?;
    task_with_tid(&process, tid)
}

fn queue_user_signal(task: Arc<TaskControlBlock>, signum: u32, sender_pid: i32) -> SysResult<()> {
    let signal = validate_kill_signum(signum)?;
    if signal.is_empty() {
        return Ok(());
    }
    queue_signal_to_task(task, signal, SignalInfo::user(signum as i32, sender_pid));
    Ok(())
}

pub fn sys_tkill(tid: usize, signum: u32) -> SysResult {
    let (_, task) = find_task_by_linux_tid(tid).ok_or(SysError::ESRCH)?;
    let sender_pid = current_process().getpid() as i32;
    queue_user_signal(task, signum, sender_pid)?;
    Ok(0)
}

pub fn sys_tgkill(tgid: usize, tid: usize, signum: u32) -> SysResult {
    let task = find_task_in_process_by_linux_tid(tgid, tid).ok_or(SysError::ESRCH)?;
    let sender_pid = current_process().getpid() as i32;
    queue_user_signal(task, signum, sender_pid)?;
    Ok(0)
}

pub fn sys_rt_sigaction(
    signum: u32,
    action: *const u8,
    old_action: *mut u8,
    sigsetsize: usize,
) -> SysResult {
    if sigsetsize != LINUX_RT_SIGSET_SIZE {
        return Err(SysError::EINVAL);
    }
    let signal_index = validate_action_signum(signum)?;
    if !action.is_null() && (signum == SIGKILL || signum == SIGSTOP) {
        return Err(SysError::EINVAL);
    }

    let token = current_user_token();
    let new_action = if action.is_null() {
        None
    } else {
        Some(signal_action_from_linux(read_user_value(
            token,
            action.cast::<LinuxKernelSigAction>(),
        )?))
    };
    let process = current_process();
    let old = process.inner_exclusive_access().signal_actions[signal_index];
    // CONTEXT: user memory writes can fault, so release the process lock before
    // copying out the old action and reacquire it only when installing the new one.
    if !old_action.is_null() {
        let old = LinuxKernelSigAction::from(old);
        write_user_value(token, old_action.cast::<LinuxKernelSigAction>(), &old)?;
    }
    if let Some(new_action) = new_action {
        process.inner_exclusive_access().signal_actions[signal_index] = new_action;
    }
    Ok(0)
}

pub fn sys_rt_sigprocmask(
    how: usize,
    set: *const u8,
    old_set: *mut u8,
    sigsetsize: usize,
) -> SysResult {
    if sigsetsize != LINUX_RT_SIGSET_SIZE {
        return Err(SysError::EINVAL);
    }
    let token = current_user_token();
    let new_set = if set.is_null() {
        None
    } else {
        Some(read_signal_set(token, set, sigsetsize)?)
    };
    let task = current_task().ok_or(SysError::ESRCH)?;
    let old_mask = task.inner_exclusive_access().signal_mask;
    // CONTEXT: user memory writes can fault, so release the task lock before
    // copying out the old mask and reacquire it only when installing the new one.
    if !old_set.is_null() {
        let old_raw = flags_to_linux_sigset(old_mask);
        write_user_value(token, old_set.cast::<u64>(), &old_raw)?;
    }
    if let Some(new_set) = new_set {
        let mut task_inner = task.inner_exclusive_access();
        match how {
            SIG_BLOCK => task_inner.signal_mask |= new_set,
            SIG_UNBLOCK => task_inner.signal_mask.remove(new_set),
            SIG_SETMASK => task_inner.signal_mask = new_set,
            _ => return Err(SysError::EINVAL),
        }
    }
    Ok(0)
}

pub fn sys_rt_sigreturn() -> SysResult {
    crate::arch::signal::sys_rt_sigreturn()
}

pub fn sys_rt_sigtimedwait(
    set: *const u8,
    info: *mut LinuxSigInfo,
    timeout: *const LinuxTimeSpec,
    sigsetsize: usize,
) -> SysResult {
    let token = current_user_token();
    let wanted = read_signal_set(token, set, sigsetsize)?;
    let deadline_ms = relative_timeout_deadline_ms(token, timeout)?;

    loop {
        if let Some(signum) = try_return_pending_signal(token, wanted, info)? {
            return Ok(signum);
        }

        if let Some(deadline_ms) = deadline_ms {
            if get_time_ms() >= deadline_ms {
                return Err(SysError::EAGAIN);
            }
        }

        // UNFINISHED: A real Linux implementation sleeps interruptibly and is
        // woken by signal delivery. Until this kernel has signal wait queues,
        // yield cooperatively so child exit and kill paths can run.
        crate::task::suspend_current_and_run_next();
    }
}
