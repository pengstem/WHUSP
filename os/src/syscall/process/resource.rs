use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::user_ptr::{read_user_value, write_user_value};
use crate::task::{current_process, current_user_token, pid2process, RLimit, RLimitResource};
use alloc::sync::Arc;

fn rlimit_target_process(pid: usize) -> SysResult<Arc<crate::task::ProcessControlBlock>> {
    if pid == 0 {
        Ok(current_process())
    } else {
        // UNFINISHED: Linux prlimit64 checks real/effective/saved UIDs and
        // CAP_SYS_RESOURCE before operating on another process. This kernel
        // does not model credentials yet, so a live PID is accepted.
        pid2process(pid).ok_or(SysError::ESRCH)
    }
}

fn validate_new_rlimit(current: RLimit, new_limit: RLimit) -> SysResult<()> {
    if new_limit.rlim_cur > new_limit.rlim_max {
        return Err(SysError::EINVAL);
    }
    if new_limit.rlim_max > current.rlim_max {
        // UNFINISHED: Raising a hard resource limit should be allowed for a
        // task with CAP_SYS_RESOURCE. Capabilities are not modeled yet.
        return Err(SysError::EPERM);
    }
    Ok(())
}

pub fn sys_prlimit64(
    pid: usize,
    resource: i32,
    new_limit: *const RLimit,
    old_limit: *mut RLimit,
) -> SysResult {
    let resource = RLimitResource::from_raw(resource).ok_or(SysError::EINVAL)?;
    let token = current_user_token();
    let new_limit = if new_limit.is_null() {
        None
    } else {
        Some(read_user_value(token, new_limit)?)
    };
    let process = rlimit_target_process(pid)?;
    let mut inner = process.inner_exclusive_access();
    let current = inner.resource_limits.get(resource);

    if let Some(new_limit) = new_limit {
        validate_new_rlimit(current, new_limit)?;
    }
    if !old_limit.is_null() {
        write_user_value(token, old_limit, &current)?;
    }
    if let Some(new_limit) = new_limit {
        inner.resource_limits.set(resource, new_limit);
    }
    Ok(0)
}

pub fn sys_getrlimit(resource: i32, old_limit: *mut RLimit) -> SysResult {
    if old_limit.is_null() {
        return Err(SysError::EFAULT);
    }
    sys_prlimit64(0, resource, core::ptr::null(), old_limit)
}

pub fn sys_setrlimit(resource: i32, new_limit: *const RLimit) -> SysResult {
    if new_limit.is_null() {
        return Err(SysError::EFAULT);
    }
    sys_prlimit64(0, resource, new_limit, core::ptr::null_mut())
}
