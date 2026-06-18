use crate::arch::interrupt;
use crate::task::{
    Credentials, MINSIGSTKSZ, ProcessControlBlock, SI_TKILL, SIGKILL, SIGNAL_INFO_SLOTS, SIGSTOP,
    SS_DISABLE, SS_ONSTACK, SigAltStack, SignalAction, SignalFlags, SignalInfo, TaskControlBlock,
    block_current_task_no_schedule, current_has_interrupting_signal, current_process, current_task,
    current_trap_cx, current_user_token, flags_to_linux_sigset, linux_sigset_to_flags,
    processes_snapshot, queue_signal_to_task, schedule, task_with_linux_tid, wakeup_task,
};
use crate::timer::{add_timer, get_time_ms};
use alloc::sync::Arc;
use core::mem::size_of;

use super::SyscallContext;
use super::errno::{SysError, SysResult};
use super::time::relative_timeout_deadline_ms;
use super::uapi::LinuxTimeSpec;
use super::user_ptr::{
    copy_to_user_ctx, read_user_value, read_user_value_ctx, write_user_value, write_user_value_ctx,
};
use super::wait::LinuxSigInfo;

const LINUX_RT_SIGSET_SIZE: usize = 8;
const SIG_BLOCK: usize = 0;
const SIG_UNBLOCK: usize = 1;
const SIG_SETMASK: usize = 2;
const LINUX_SS_AUTODISARM: i32 = 1 << 31;

fn read_signal_set(token: usize, set: *const u8, sigsetsize: usize) -> SysResult<SignalFlags> {
    // Linux rt-signal syscalls pass one 64-bit sigset on these ABIs. SIGKILL
    // and SIGSTOP are never blockable, so strip them at the syscall boundary
    // before the mask reaches TaskControlBlockInner.
    if sigsetsize != LINUX_RT_SIGSET_SIZE {
        return Err(SysError::EINVAL);
    }
    if (set as usize) < size_of::<u64>() {
        return Err(SysError::EFAULT);
    }
    let raw_set = read_user_value(token, set.cast::<u64>())?;
    let mut flags = linux_sigset_to_flags(raw_set);
    flags.remove(SignalFlags::SIGKILL);
    flags.remove(SignalFlags::SIGSTOP);
    Ok(flags)
}

fn read_signal_set_ctx(
    ctx: &SyscallContext,
    set: *const u8,
    sigsetsize: usize,
) -> SysResult<SignalFlags> {
    // Linux rt-signal syscalls pass one 64-bit sigset on these ABIs. SIGKILL
    // and SIGSTOP are never blockable, so strip them at the syscall boundary
    // before the mask reaches TaskControlBlockInner.
    if sigsetsize != LINUX_RT_SIGSET_SIZE {
        return Err(SysError::EINVAL);
    }
    if (set as usize) < size_of::<u64>() {
        return Err(SysError::EFAULT);
    }
    let raw_set = read_user_value_ctx(ctx, set.cast::<u64>())?;
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
        let info = LinuxSigInfo::from(info);
        write_user_value(token, info_ptr, &info)?;
    }
    consume_pending_signal(signum);
    Ok(Some(signum as isize))
}

fn try_return_waitable_sigchld(
    token: usize,
    wanted: SignalFlags,
    info_ptr: *mut LinuxSigInfo,
) -> SysResult<Option<isize>> {
    if !wanted.contains(SignalFlags::SIGCHLD) {
        return Ok(None);
    }
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if !inner.signal_actions[crate::task::SIGCHLD as usize].has_user_handler() {
        return Ok(None);
    }
    let Some(info) = inner.children.iter().find_map(|child| {
        let child_inner = child.inner_exclusive_access();
        child_inner.is_zombie.then(|| {
            SignalInfo::child_exit(
                crate::task::SIGCHLD as i32,
                child.getpid() as i32,
                child_inner.exit_code,
            )
        })
    }) else {
        return Ok(None);
    };
    drop(inner);

    if !info_ptr.is_null() {
        let info = LinuxSigInfo::from(info);
        write_user_value(token, info_ptr, &info)?;
    }
    Ok(Some(crate::task::SIGCHLD as isize))
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxKernelSigAction {
    handler: usize,
    flags: usize,
    restorer: usize,
    mask: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxStackT {
    sp: usize,
    flags: i32,
    pad: u32,
    size: usize,
}

impl LinuxStackT {
    fn from_altstack(stack: SigAltStack, current_sp: usize) -> Self {
        Self {
            sp: stack.sp,
            flags: stack.flags_for_sp(current_sp),
            pad: 0,
            size: stack.size,
        }
    }

    fn into_altstack(self) -> SysResult<SigAltStack> {
        let allowed_flags = SS_DISABLE | LINUX_SS_AUTODISARM;
        if self.flags & !allowed_flags != 0 || self.flags & SS_ONSTACK != 0 {
            return Err(SysError::EINVAL);
        }
        if self.flags & SS_DISABLE != 0 {
            return Ok(SigAltStack::disabled());
        }
        if self.size < MINSIGSTKSZ {
            return Err(SysError::ENOMEM);
        }
        // UNFINISHED: SS_AUTODISARM is accepted for libc compatibility but this
        // kernel does not yet clear the altstack on handler entry.
        Ok(SigAltStack {
            sp: self.sp,
            size: self.size,
            flags: self.flags & LINUX_SS_AUTODISARM,
        })
    }
}

impl From<SignalAction> for LinuxKernelSigAction {
    fn from(action: SignalAction) -> Self {
        Self {
            handler: action.handler,
            flags: action.flags,
            restorer: action.restorer,
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
        restorer: raw.restorer,
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

fn find_task_by_linux_tid(tid: usize) -> Option<(usize, Arc<TaskControlBlock>)> {
    let task = task_with_linux_tid(tid)?;
    let process = task.process.upgrade()?;
    Some((process.getpid(), task))
}

fn find_task_in_process_by_linux_tid(tgid: usize, tid: usize) -> Option<Arc<TaskControlBlock>> {
    let task = task_with_linux_tid(tid)?;
    let process = task.process.upgrade()?;
    (process.getpid() == tgid).then_some(task)
}

fn process_by_visible_pid(
    pid: isize,
    caller: &Arc<ProcessControlBlock>,
) -> SysResult<Arc<ProcessControlBlock>> {
    if pid <= 0 {
        return Err(SysError::EINVAL);
    }
    let namespace = caller.pid_namespace();
    processes_snapshot()
        .into_iter()
        .find(|process| process.pid_visible_from_namespace(namespace) == Some(pid as usize))
        .ok_or(SysError::ESRCH)
}

fn caller_can_signal_target(caller: &Credentials, target: &Credentials) -> bool {
    // UNFINISHED: Linux also checks CAP_KILL in the target user namespace and
    // permits SIGCONT inside the same session. This kernel has one credential
    // namespace and process-wide credentials.
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

fn sigqueue_info_from_user(signum: u32, info: *const LinuxSigInfo) -> SysResult<SignalInfo> {
    if info.is_null() {
        return Err(SysError::EFAULT);
    }
    let sender_pid = current_process().getpid() as i32;
    let mut info = read_user_value(current_user_token(), info)?;
    info.si_signo = signum as i32;
    Ok(info.to_signal_info(signum, sender_pid))
}

fn validate_sigqueue_info_code(target_is_current: bool, info: &SignalInfo) -> SysResult<()> {
    if !target_is_current && (info.code >= 0 || info.code == SI_TKILL) {
        return Err(SysError::EPERM);
    }
    Ok(())
}

pub fn sys_rt_sigqueueinfo(pid: isize, signum: u32, info: *const LinuxSigInfo) -> SysResult {
    let caller = current_process();
    let target = process_by_visible_pid(pid, &caller)?;
    let info = sigqueue_info_from_user(signum, info)?;
    let signal = validate_kill_signum(signum)?;
    validate_sigqueue_info_code(caller.getpid() == target.getpid(), &info)?;

    if !caller_can_signal_target(&caller.credentials(), &target.credentials()) {
        return Err(SysError::EPERM);
    }
    // UNFINISHED: Linux queues multiple realtime siginfo records and can return
    // EAGAIN at RLIMIT_SIGPENDING. This kernel stores one siginfo slot per
    // signum, so repeated sends of the same signal replace the previous info.
    queue_signal_to_process(&target, signal, info);
    Ok(0)
}

pub fn sys_rt_tgsigqueueinfo(
    tgid: isize,
    tid: isize,
    signum: u32,
    info: *const LinuxSigInfo,
) -> SysResult {
    if tgid <= 0 || tid <= 0 {
        return Err(SysError::EINVAL);
    }
    let task =
        find_task_in_process_by_linux_tid(tgid as usize, tid as usize).ok_or(SysError::ESRCH)?;
    let target_process = task.process.upgrade().ok_or(SysError::ESRCH)?;
    let info = sigqueue_info_from_user(signum, info)?;
    let signal = validate_kill_signum(signum)?;
    let target_is_current = current_task().is_some_and(|task| task.linux_tid() == tid as usize);
    validate_sigqueue_info_code(target_is_current, &info)?;

    let caller = current_process();
    if !caller_can_signal_target(&caller.credentials(), &target_process.credentials()) {
        return Err(SysError::EPERM);
    }
    // UNFINISHED: See sys_rt_sigqueueinfo(); realtime signal queue capacity is
    // not modeled yet.
    queue_signal_to_task(task, signal, info);
    Ok(0)
}

pub fn sys_tkill(tid: isize, signum: u32) -> SysResult {
    let signal = validate_kill_signum(signum)?;
    if tid < 0 {
        return Err(SysError::EINVAL);
    }
    let (_, task) = find_task_by_linux_tid(tid as usize).ok_or(SysError::ESRCH)?;
    if !signal.is_empty() {
        let sender_pid = current_process().getpid() as i32;
        queue_signal_to_task(task, signal, SignalInfo::tkill(signum as i32, sender_pid));
    }
    Ok(0)
}

pub fn sys_tgkill(tgid: isize, tid: isize, signum: u32) -> SysResult {
    let signal = validate_kill_signum(signum)?;
    if tgid < 0 || tid < 0 {
        return Err(SysError::EINVAL);
    }
    let task =
        find_task_in_process_by_linux_tid(tgid as usize, tid as usize).ok_or(SysError::ESRCH)?;
    if !signal.is_empty() {
        let sender_pid = current_process().getpid() as i32;
        queue_signal_to_task(task, signal, SignalInfo::tkill(signum as i32, sender_pid));
    }
    Ok(0)
}

pub fn sys_rt_sigaction_ctx(
    ctx: &SyscallContext,
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

    let new_action = if action.is_null() {
        None
    } else {
        Some(signal_action_from_linux(read_user_value_ctx(
            ctx,
            action.cast::<LinuxKernelSigAction>(),
        )?))
    };
    let process = ctx.process();
    let old = process.inner_exclusive_access().signal_actions[signal_index];
    // CONTEXT: user memory writes can fault, so release the process lock before
    // copying out the old action and reacquire it only when installing the new one.
    if !old_action.is_null() {
        let old = LinuxKernelSigAction::from(old);
        write_user_value_ctx(ctx, old_action.cast::<LinuxKernelSigAction>(), &old)?;
    }
    if let Some(new_action) = new_action {
        process.inner_exclusive_access().signal_actions[signal_index] = new_action;
    }
    Ok(0)
}

pub fn sys_rt_sigprocmask_ctx(
    ctx: &SyscallContext,
    how: usize,
    set: *const u8,
    old_set: *mut u8,
    sigsetsize: usize,
) -> SysResult {
    if sigsetsize != LINUX_RT_SIGSET_SIZE {
        return Err(SysError::EINVAL);
    }
    let new_set = if set.is_null() {
        None
    } else {
        Some(read_signal_set_ctx(ctx, set, sigsetsize)?)
    };
    let task = ctx.task();
    let old_mask = task.inner_exclusive_access().signal_mask;
    // CONTEXT: user memory writes can fault, so release the task lock before
    // copying out the old mask and reacquire it only when installing the new one.
    if !old_set.is_null() {
        let old_raw = flags_to_linux_sigset(old_mask);
        write_user_value_ctx(ctx, old_set.cast::<u64>(), &old_raw)?;
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

pub fn sys_rt_sigpending_ctx(ctx: &SyscallContext, set: *mut u8, sigsetsize: usize) -> SysResult {
    if sigsetsize > LINUX_RT_SIGSET_SIZE {
        return Err(SysError::EINVAL);
    }
    let pending = {
        let task = ctx.task();
        let task_inner = task.inner_exclusive_access();
        // UNFINISHED: Linux reports the union of thread-local pending signals
        // and process-wide shared pending signals. This kernel currently routes
        // process-directed signals to a concrete task instead of keeping a
        // separate shared-pending queue, so the task-local set is the only
        // pending source available here.
        task_inner.pending_signals & task_inner.signal_mask
    };
    let raw = flags_to_linux_sigset(pending);
    copy_to_user_ctx(ctx, set, &raw.to_ne_bytes()[..sigsetsize])?;
    Ok(0)
}

pub fn sys_sigaltstack_ctx(
    ctx: &SyscallContext,
    new_stack: *const u8,
    old_stack: *mut u8,
) -> SysResult {
    let task = ctx.task();
    #[cfg(target_arch = "riscv64")]
    let current_sp = current_trap_cx().x[2];
    #[cfg(target_arch = "loongarch64")]
    let current_sp = current_trap_cx().x[3];
    let old = task.inner_exclusive_access().sigaltstack;
    if !old_stack.is_null() {
        let old_raw = LinuxStackT::from_altstack(old, current_sp);
        write_user_value_ctx(ctx, old_stack.cast::<LinuxStackT>(), &old_raw)?;
    }
    if !new_stack.is_null() {
        if old.contains(current_sp) {
            return Err(SysError::EPERM);
        }
        let new_raw = read_user_value_ctx(ctx, new_stack.cast::<LinuxStackT>())?;
        let new = new_raw.into_altstack()?;
        task.inner_exclusive_access().sigaltstack = new;
    }
    Ok(0)
}

pub fn sys_rt_sigsuspend(mask: *const u8, sigsetsize: usize) -> SysResult {
    let token = current_user_token();
    let new_mask = read_signal_set(token, mask, sigsetsize)?;
    let task = current_task().ok_or(SysError::ESRCH)?;
    let old_mask = {
        let mut task_inner = task.inner_exclusive_access();
        let old_mask = task_inner.signal_mask;
        task_inner.signal_mask = new_mask;
        old_mask
    };

    loop {
        if current_has_interrupting_signal() {
            // Signal delivery restores this saved mask when it builds the
            // user signal frame. Restoring here would let the handler run
            // under the wrong mask after sigsuspend returns EINTR.
            task.inner_exclusive_access().sigsuspend_restore_mask = Some(old_mask);
            return Err(SysError::EINTR);
        }
        let (blocked_task, task_cx_ptr) = block_current_task_no_schedule();
        drop(blocked_task);
        schedule(task_cx_ptr);
    }
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
    if !timeout.is_null() && (timeout as usize) < size_of::<LinuxTimeSpec>() {
        return Err(SysError::EFAULT);
    }
    let deadline_ms = relative_timeout_deadline_ms(token, timeout)?;

    loop {
        if let Some(signum) = try_return_pending_signal(token, wanted, info)? {
            return Ok(signum);
        }
        // CONTEXT: libc-test's runtest wrapper can briefly unblock SIGCHLD
        // around fork on LoongArch; a fast child may run its empty SIGCHLD
        // handler before the parent reaches sigtimedwait(). Linux still keeps
        // the child waitable, so let sigtimedwait(SIGCHLD) observe that state
        // without reaping it. The later waitpid() remains the reap boundary.
        if let Some(signum) = try_return_waitable_sigchld(token, wanted, info)? {
            return Ok(signum);
        }

        if current_has_interrupting_signal() {
            return Err(SysError::EINTR);
        }

        if let Some(deadline_ms) = deadline_ms
            && get_time_ms() >= deadline_ms
        {
            return Err(SysError::EAGAIN);
        }

        let interrupts_enabled = interrupt::supervisor_interrupt_enabled();
        interrupt::disable_supervisor_interrupt();
        let should_retry = peek_pending_signal(wanted).is_some()
            || current_has_interrupting_signal()
            || deadline_ms.is_some_and(|deadline_ms| get_time_ms() >= deadline_ms);
        if should_retry {
            if interrupts_enabled {
                interrupt::enable_supervisor_interrupt();
            }
            continue;
        }
        let (task, task_cx_ptr) = block_current_task_no_schedule();
        if let Some(deadline_ms) = deadline_ms {
            add_timer(deadline_ms, task);
        }
        if interrupts_enabled {
            interrupt::enable_supervisor_interrupt();
        }
        schedule(task_cx_ptr);
    }
}
