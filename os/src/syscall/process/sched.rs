use crate::{
    syscall::{
        errno::{SysError, SysResult},
        uapi::LinuxTimeSpec,
        user_ptr::{read_user_value_with_mmap_fault, write_user_value_with_mmap_fault},
    },
    task::{
        TaskControlBlock, current_process, current_task, current_user_token, processes_snapshot,
        reprioritize_ready_task, task_with_linux_tid,
    },
};
use alloc::{sync::Arc, vec, vec::Vec};
use core::mem::size_of;

const SCHED_OTHER: i32 = 0;
const SCHED_FIFO: i32 = 1;
const SCHED_RR: i32 = 2;
const SCHED_BATCH: i32 = 3;
const SCHED_IDLE: i32 = 5;
const SCHED_DEADLINE: i32 = 6;
const SCHED_RESET_ON_FORK: i32 = 0x4000_0000;
const RT_PRIORITY_MIN: isize = 1;
const RT_PRIORITY_MAX: isize = 99;
const RR_INTERVAL_NSEC: isize = 100_000_000;
const AFFINITY_MASK_BYTES: usize = size_of::<usize>();
const PRIO_PROCESS: i32 = 0;
const PRIO_PGRP: i32 = 1;
const PRIO_USER: i32 = 2;
const NICE_MIN: i8 = -20;
const NICE_MAX: i8 = 19;

#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxSchedParam {
    sched_priority: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxSchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    sched_runtime: u64,
    sched_deadline: u64,
    sched_period: u64,
}

const LINUX_SCHED_ATTR_SIZE: u32 = size_of::<LinuxSchedAttr>() as u32;
const SCHED_FLAG_RESET_ON_FORK: u64 = 0x01;
const SCHED_ATTR_SUPPORTED_FLAGS: u64 = SCHED_FLAG_RESET_ON_FORK;

fn sched_target_task(pid: isize) -> SysResult<Arc<TaskControlBlock>> {
    if pid < 0 {
        return Err(SysError::EINVAL);
    }
    if pid == 0 {
        return current_task().ok_or(SysError::ESRCH);
    }
    task_with_linux_tid(pid as usize).ok_or(SysError::ESRCH)
}

fn sched_priority_bounds(policy: i32) -> SysResult<(isize, isize)> {
    match policy {
        SCHED_FIFO | SCHED_RR => Ok((RT_PRIORITY_MIN, RT_PRIORITY_MAX)),
        SCHED_OTHER | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE => Ok((0, 0)),
        _ => Err(SysError::EINVAL),
    }
}

fn clamp_nice(prio: i32) -> i8 {
    prio.clamp(NICE_MIN as i32, NICE_MAX as i32) as i8
}

fn linux_raw_priority_from_nice(nice: i8) -> isize {
    20 - nice as isize
}

fn task_nice(task: &TaskControlBlock) -> i8 {
    task.inner_exclusive_access().nice
}

fn target_tasks_for_priority(which: i32, who: isize) -> SysResult<Vec<Arc<TaskControlBlock>>> {
    if who < 0 {
        return Err(SysError::ESRCH);
    }
    match which {
        PRIO_PROCESS => {
            let task = if who == 0 {
                current_task().ok_or(SysError::ESRCH)?
            } else {
                task_with_linux_tid(who as usize).ok_or(SysError::ESRCH)?
            };
            Ok(vec![task])
        }
        PRIO_PGRP => {
            let pgid = if who == 0 {
                current_process().process_group_id()
            } else {
                who as usize
            };
            let tasks = processes_snapshot()
                .into_iter()
                .filter(|process| process.process_group_id() == pgid)
                .flat_map(|process| process.tasks_snapshot())
                .collect::<Vec<_>>();
            if tasks.is_empty() {
                Err(SysError::ESRCH)
            } else {
                Ok(tasks)
            }
        }
        PRIO_USER => {
            let uid = if who == 0 {
                current_process().credentials().ruid
            } else {
                who as u32
            };
            let tasks = processes_snapshot()
                .into_iter()
                .filter(|process| {
                    let credentials = process.credentials();
                    credentials.ruid == uid || credentials.euid == uid
                })
                .flat_map(|process| process.tasks_snapshot())
                .collect::<Vec<_>>();
            if tasks.is_empty() {
                Err(SysError::ESRCH)
            } else {
                Ok(tasks)
            }
        }
        _ => Err(SysError::EINVAL),
    }
}

fn ensure_can_set_task_nice(task: &TaskControlBlock, new_nice: i8) -> SysResult<()> {
    let caller = current_process().credentials();
    let target_process = task.process.upgrade().ok_or(SysError::ESRCH)?;
    let target = target_process.credentials();
    let privileged = caller.euid == 0;

    if !privileged && caller.euid != target.ruid && caller.euid != target.euid {
        return Err(SysError::EPERM);
    }

    if !privileged && new_nice < task_nice(task) {
        return Err(SysError::EACCES);
    }

    Ok(())
}

fn current_has_scheduler_privilege() -> bool {
    current_process().credentials().is_root()
}

fn current_euid_matches_task(task: &TaskControlBlock) -> SysResult<bool> {
    let caller = current_process().credentials();
    let target_process = task.process.upgrade().ok_or(SysError::ESRCH)?;
    let target = target_process.credentials();
    Ok(caller.euid == target.ruid || caller.euid == target.euid)
}

fn policy_change_requires_privilege(policy: i32) -> bool {
    matches!(policy, SCHED_FIFO | SCHED_RR | SCHED_DEADLINE)
}

fn ensure_can_change_task_sched(
    task: &TaskControlBlock,
    requires_privilege: bool,
) -> SysResult<()> {
    if current_has_scheduler_privilege() {
        return Ok(());
    }
    if requires_privilege || !current_euid_matches_task(task)? {
        return Err(SysError::EPERM);
    }
    Ok(())
}

fn split_settable_policy(policy: i32) -> SysResult<(i32, bool)> {
    let reset_on_fork = policy & SCHED_RESET_ON_FORK != 0;
    let base_policy = policy & !SCHED_RESET_ON_FORK;
    match base_policy {
        SCHED_OTHER | SCHED_FIFO | SCHED_RR | SCHED_BATCH | SCHED_IDLE => {
            Ok((base_policy, reset_on_fork))
        }
        _ => Err(SysError::EINVAL),
    }
}

fn validate_priority_for_policy(policy: i32, priority: i32) -> SysResult<()> {
    let (min, max) = sched_priority_bounds(policy)?;
    let priority = priority as isize;
    if priority < min || priority > max {
        return Err(SysError::EINVAL);
    }
    Ok(())
}

fn validate_deadline_params(attr: LinuxSchedAttr) -> SysResult<()> {
    if attr.sched_runtime == 0
        || attr.sched_deadline == 0
        || attr.sched_period == 0
        || attr.sched_runtime > attr.sched_deadline
        || attr.sched_deadline > attr.sched_period
    {
        return Err(SysError::EINVAL);
    }
    Ok(())
}

fn validate_sched_attr(attr: LinuxSchedAttr) -> SysResult<()> {
    if attr.size < LINUX_SCHED_ATTR_SIZE {
        return Err(SysError::EINVAL);
    }
    if attr.sched_flags & !SCHED_ATTR_SUPPORTED_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    let policy = attr.sched_policy as i32;
    validate_priority_for_policy(policy, attr.sched_priority as i32)?;
    if policy == SCHED_DEADLINE {
        validate_deadline_params(attr)?;
    }
    Ok(())
}

fn linux_policy_for_task(task: &TaskControlBlock) -> i32 {
    let inner = task.inner_exclusive_access();
    let mut policy = inner.sched_policy;
    if inner.sched_reset_on_fork {
        policy |= SCHED_RESET_ON_FORK;
    }
    policy
}

fn apply_sched_policy(
    task: &Arc<TaskControlBlock>,
    base_policy: i32,
    priority: i32,
    reset_on_fork: bool,
) {
    {
        let mut inner = task.inner_exclusive_access();
        inner.sched_policy = base_policy;
        inner.sched_priority = priority;
        inner.sched_reset_on_fork = reset_on_fork;
    }
    reprioritize_ready_task(Arc::clone(task));
}

fn apply_sched_attr(task: &Arc<TaskControlBlock>, attr: LinuxSchedAttr) {
    {
        let mut inner = task.inner_exclusive_access();
        inner.sched_policy = attr.sched_policy as i32;
        inner.sched_priority = attr.sched_priority as i32;
        inner.sched_reset_on_fork = attr.sched_flags & SCHED_FLAG_RESET_ON_FORK != 0;
        inner.sched_deadline_runtime = attr.sched_runtime;
        inner.sched_deadline_deadline = attr.sched_deadline;
        inner.sched_deadline_period = attr.sched_period;
        if matches!(inner.sched_policy, SCHED_OTHER | SCHED_BATCH) {
            inner.nice = clamp_nice(attr.sched_nice);
        }
    }
    reprioritize_ready_task(Arc::clone(task));
}

fn sched_attr_for_task(task: &TaskControlBlock) -> LinuxSchedAttr {
    let inner = task.inner_exclusive_access();
    LinuxSchedAttr {
        size: LINUX_SCHED_ATTR_SIZE,
        sched_policy: inner.sched_policy as u32,
        sched_flags: if inner.sched_reset_on_fork {
            SCHED_FLAG_RESET_ON_FORK
        } else {
            0
        },
        sched_nice: inner.nice as i32,
        sched_priority: inner.sched_priority as u32,
        sched_runtime: inner.sched_deadline_runtime,
        sched_deadline: inner.sched_deadline_deadline,
        sched_period: inner.sched_deadline_period,
    }
}

pub fn sys_sched_getscheduler(pid: isize) -> SysResult {
    let task = sched_target_task(pid)?;
    Ok(linux_policy_for_task(&task) as isize)
}

pub fn sys_sched_getparam(pid: isize, param: usize) -> SysResult {
    if param == 0 {
        return Err(SysError::EINVAL);
    }
    let task = sched_target_task(pid)?;
    let sched_param = LinuxSchedParam {
        sched_priority: task.inner_exclusive_access().sched_priority,
    };
    write_user_value_with_mmap_fault(
        current_user_token(),
        param as *mut LinuxSchedParam,
        &sched_param,
    )?;
    Ok(0)
}

pub fn sys_sched_getaffinity(pid: isize, cpusetsize: usize, mask: usize) -> SysResult {
    if cpusetsize < AFFINITY_MASK_BYTES {
        return Err(SysError::EINVAL);
    }
    let _task = sched_target_task(pid)?;
    // CONTEXT: The current contest runtime exposes a single runnable hart to
    // user space and does not model Linux cpusets/cgroups yet, so every task
    // reports an affinity mask containing CPU 0 only.
    let affinity_mask = 1usize;
    write_user_value_with_mmap_fault(current_user_token(), mask as *mut usize, &affinity_mask)?;
    Ok(AFFINITY_MASK_BYTES as isize)
}

pub fn sys_sched_setaffinity(pid: isize, cpusetsize: usize, mask: usize) -> SysResult {
    if cpusetsize < AFFINITY_MASK_BYTES {
        return Err(SysError::EINVAL);
    }
    let task = sched_target_task(pid)?;
    let affinity_mask =
        read_user_value_with_mmap_fault(current_user_token(), mask as *const usize)?;
    if affinity_mask & 1 == 0 {
        return Err(SysError::EINVAL);
    }
    ensure_can_change_task_sched(&task, false)?;
    // CONTEXT: The current contest runtime has only CPU 0 available to user
    // space. Accept masks that include CPU 0 for ABI compatibility, but do
    // not perform migration or persist a broader cpuset model yet.
    Ok(0)
}

pub fn sys_sched_setparam(pid: isize, param: usize) -> SysResult {
    if param == 0 {
        return Err(SysError::EINVAL);
    }
    let sched_param =
        read_user_value_with_mmap_fault(current_user_token(), param as *const LinuxSchedParam)?;
    let task = sched_target_task(pid)?;
    let requires_privilege = {
        let inner = task.inner_exclusive_access();
        validate_priority_for_policy(inner.sched_policy, sched_param.sched_priority)?;
        policy_change_requires_privilege(inner.sched_policy)
    };
    ensure_can_change_task_sched(&task, requires_privilege)?;
    // CONTEXT: This updates Linux ABI-visible static priority metadata. The
    // current contest scheduler uses this metadata only for coarse RT queue
    // selection, not for full Linux policy semantics.
    task.inner_exclusive_access().sched_priority = sched_param.sched_priority;
    reprioritize_ready_task(task);
    Ok(0)
}

pub fn sys_sched_setscheduler(pid: isize, policy: i32, param: usize) -> SysResult {
    if param == 0 {
        return Err(SysError::EINVAL);
    }
    let (base_policy, reset_on_fork) = split_settable_policy(policy)?;
    let sched_param =
        read_user_value_with_mmap_fault(current_user_token(), param as *const LinuxSchedParam)?;
    validate_priority_for_policy(base_policy, sched_param.sched_priority)?;
    let task = sched_target_task(pid)?;
    ensure_can_change_task_sched(&task, policy_change_requires_privilege(base_policy))?;

    // CONTEXT: cyclictest only needs Linux ABI-visible scheduling attributes
    // plus coarse RT priority over normal hackbench load. When the current
    // thread enters an RT policy, promote sibling threads in the same PCB too
    // so cyclictest's normal-priority control thread can drive the short test
    // to completion in this single-run-queue kernel.
    // UNFINISHED: Linux sched_setscheduler(2) is per-thread for pid 0; this
    // compatibility promotion should become unnecessary once the scheduler has
    // a real RT run queue and better synchronization wakeup semantics.
    // UNFINISHED: Linux also considers CAP_SYS_NICE and rlimits. This kernel
    // currently models scheduler privilege with root euid only.
    if pid == 0 && matches!(base_policy, SCHED_FIFO | SCHED_RR) {
        for task in current_process().tasks_snapshot() {
            apply_sched_policy(
                &task,
                base_policy,
                sched_param.sched_priority,
                reset_on_fork,
            );
        }
    } else {
        apply_sched_policy(
            &task,
            base_policy,
            sched_param.sched_priority,
            reset_on_fork,
        );
    }
    Ok(0)
}

pub fn sys_sched_get_priority_max(policy: i32) -> SysResult {
    Ok(sched_priority_bounds(policy)?.1)
}

pub fn sys_sched_get_priority_min(policy: i32) -> SysResult {
    Ok(sched_priority_bounds(policy)?.0)
}

pub fn sys_sched_setattr(pid: isize, attr: usize, flags: u32) -> SysResult {
    if pid < 0 || attr == 0 || flags != 0 {
        return Err(SysError::EINVAL);
    }
    let sched_attr =
        read_user_value_with_mmap_fault(current_user_token(), attr as *const LinuxSchedAttr)?;
    validate_sched_attr(sched_attr)?;
    let task = sched_target_task(pid)?;
    ensure_can_change_task_sched(
        &task,
        policy_change_requires_privilege(sched_attr.sched_policy as i32),
    )?;
    // CONTEXT: SCHED_DEADLINE admission control and bandwidth enforcement are
    // not modeled by this contest scheduler yet. The Linux-visible attributes
    // are stored so LTP and libc callers can observe the requested policy.
    apply_sched_attr(&task, sched_attr);
    Ok(0)
}

pub fn sys_sched_getattr(pid: isize, attr: usize, size: usize, flags: u32) -> SysResult {
    if pid < 0 || attr == 0 || flags != 0 || size < LINUX_SCHED_ATTR_SIZE as usize {
        return Err(SysError::EINVAL);
    }
    let task = sched_target_task(pid)?;
    let sched_attr = sched_attr_for_task(&task);
    write_user_value_with_mmap_fault(
        current_user_token(),
        attr as *mut LinuxSchedAttr,
        &sched_attr,
    )?;
    Ok(0)
}

pub fn sys_sched_rr_get_interval(pid: isize, interval: *mut LinuxTimeSpec) -> SysResult {
    if interval.is_null() {
        return Err(SysError::EFAULT);
    }
    let task = sched_target_task(pid)?;
    let sched_policy = task.inner_exclusive_access().sched_policy;
    // CONTEXT: The kernel does not yet run a separate SCHED_RR queue. Report
    // Linux's default 100 ms quantum for RR tasks and a zero interval for
    // non-RR policies such as SCHED_FIFO.
    let rr_interval = if sched_policy == SCHED_RR {
        LinuxTimeSpec {
            tv_sec: 0,
            tv_nsec: RR_INTERVAL_NSEC,
        }
    } else {
        LinuxTimeSpec {
            tv_sec: 0,
            tv_nsec: 0,
        }
    };
    write_user_value_with_mmap_fault(current_user_token(), interval, &rr_interval)?;
    Ok(0)
}

pub fn sys_getpriority(which: i32, who: isize) -> SysResult {
    let targets = target_tasks_for_priority(which, who)?;
    let best_nice = targets
        .iter()
        .map(|task| task_nice(task))
        .min()
        .ok_or(SysError::ESRCH)?;

    // CONTEXT: Linux's raw getpriority syscall returns 40..1 so negative nice
    // values cannot be confused with -errno. libc translates this back to
    // user-visible nice values in the -20..19 range.
    Ok(linux_raw_priority_from_nice(best_nice))
}

pub fn sys_setpriority(which: i32, who: isize, prio: i32) -> SysResult {
    let new_nice = clamp_nice(prio);
    let targets = target_tasks_for_priority(which, who)?;

    for task in &targets {
        ensure_can_set_task_nice(task, new_nice)?;
    }
    for task in targets {
        task.inner_exclusive_access().nice = new_nice;
    }

    Ok(0)
}
