use crate::{
    syscall::{
        errno::{SysError, SysResult},
        uapi::LinuxTimeSpec,
        user_ptr::{copy_to_user, read_user_value, write_user_value},
    },
    task::{TaskControlBlock, current_task, current_user_token, processes_snapshot},
};
use alloc::sync::Arc;
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

#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxSchedParam {
    sched_priority: i32,
}

fn task_with_linux_tid(tid: usize) -> Option<Arc<TaskControlBlock>> {
    for process in processes_snapshot() {
        if let Some(task) = process
            .tasks_snapshot()
            .into_iter()
            .find(|task| task.linux_tid() == tid)
        {
            return Some(task);
        }
    }
    None
}

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

fn linux_policy_for_task(task: &TaskControlBlock) -> i32 {
    let inner = task.inner_exclusive_access();
    let mut policy = inner.sched_policy;
    if inner.sched_reset_on_fork {
        policy |= SCHED_RESET_ON_FORK;
    }
    policy
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
    write_user_value(
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
    let affinity_mask = 1usize.to_ne_bytes();
    copy_to_user(current_user_token(), mask as *mut u8, &affinity_mask)?;
    Ok(AFFINITY_MASK_BYTES as isize)
}

pub fn sys_sched_setparam(pid: isize, param: usize) -> SysResult {
    if param == 0 {
        return Err(SysError::EINVAL);
    }
    let sched_param = read_user_value(current_user_token(), param as *const LinuxSchedParam)?;
    let task = sched_target_task(pid)?;
    let mut inner = task.inner_exclusive_access();
    validate_priority_for_policy(inner.sched_policy, sched_param.sched_priority)?;
    // CONTEXT: This updates only Linux ABI-visible static priority metadata.
    // The contest scheduler still runs a single non-RT run queue.
    inner.sched_priority = sched_param.sched_priority;
    Ok(0)
}

pub fn sys_sched_setscheduler(pid: isize, policy: i32, param: usize) -> SysResult {
    if param == 0 {
        return Err(SysError::EINVAL);
    }
    let (base_policy, reset_on_fork) = split_settable_policy(policy)?;
    let sched_param = read_user_value(current_user_token(), param as *const LinuxSchedParam)?;
    validate_priority_for_policy(base_policy, sched_param.sched_priority)?;
    let task = sched_target_task(pid)?;

    // CONTEXT: cyclictest only needs Linux ABI-visible scheduling attributes.
    // The current contest scheduler does not yet run a separate RT queue.
    // UNFINISHED: Linux permission checks use CAP_SYS_NICE, rlimits, and
    // per-thread credentials; this compatibility layer currently allows the
    // root-like contest workload to set RT policies.
    let mut inner = task.inner_exclusive_access();
    inner.sched_policy = base_policy;
    inner.sched_priority = sched_param.sched_priority;
    inner.sched_reset_on_fork = reset_on_fork;
    Ok(0)
}

pub fn sys_sched_get_priority_max(policy: i32) -> SysResult {
    Ok(sched_priority_bounds(policy)?.1)
}

pub fn sys_sched_get_priority_min(policy: i32) -> SysResult {
    Ok(sched_priority_bounds(policy)?.0)
}

pub fn sys_sched_rr_get_interval(pid: isize, interval: *mut LinuxTimeSpec) -> SysResult {
    if interval.is_null() {
        return Err(SysError::EFAULT);
    }
    let _task = sched_target_task(pid)?;
    // CONTEXT: The kernel does not yet run a separate SCHED_RR queue. Report
    // Linux's default 100 ms quantum for ABI compatibility.
    let rr_interval = LinuxTimeSpec {
        tv_sec: 0,
        tv_nsec: RR_INTERVAL_NSEC,
    };
    write_user_value(current_user_token(), interval, &rr_interval)?;
    Ok(0)
}
