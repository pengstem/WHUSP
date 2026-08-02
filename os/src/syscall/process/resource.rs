use crate::syscall::SyscallContext;
use crate::syscall::user_ptr::{read_user_value_ctx, write_user_value_ctx};
use crate::task::{CAP_SYS_RESOURCE, ProcessControlBlock, RLimit, RLimitResource, pid2process};
use crate::uapi::errno::{Errno, KResult};
use alloc::sync::Arc;

fn rlimit_target_process_ctx(
    ctx: &SyscallContext,
    pid: usize,
) -> KResult<Arc<ProcessControlBlock>> {
    if pid == 0 {
        Ok(Arc::clone(ctx.process()))
    } else {
        // UNFINISHED: Linux prlimit64 checks real/effective/saved UIDs and
        // CAP_SYS_RESOURCE before operating on another process. This kernel
        // does not model credentials yet, so a live PID is accepted.
        pid2process(pid).ok_or(Errno::ESRCH)
    }
}

fn validate_new_rlimit(
    current: RLimit,
    new_limit: RLimit,
    can_raise_hard_limit: bool,
) -> KResult<()> {
    if new_limit.rlim_cur > new_limit.rlim_max {
        return Err(Errno::EINVAL);
    }
    if new_limit.rlim_max > current.rlim_max && !can_raise_hard_limit {
        // UNFINISHED: Linux checks CAP_SYS_RESOURCE in the caller's user
        // namespace. This kernel has one process-wide capability set, so root
        // with the stored CAP_SYS_RESOURCE bit is the current privileged model.
        return Err(Errno::EPERM);
    }
    Ok(())
}

pub fn sys_prlimit64_ctx(
    ctx: &SyscallContext,
    pid: usize,
    resource: i32,
    new_limit: *const RLimit,
    old_limit: *mut RLimit,
) -> KResult {
    let resource = RLimitResource::from_raw(resource).ok_or(Errno::EINVAL)?;
    let new_limit = if new_limit.is_null() {
        None
    } else {
        Some(read_user_value_ctx(ctx, new_limit)?)
    };
    if new_limit.is_some()
        && !matches!(
            resource,
            RLimitResource::FSize
                | RLimitResource::Stack
                | RLimitResource::Core
                | RLimitResource::NoFile
                | RLimitResource::MemLock
        )
    {
        return Err(Errno::ENOTSUP);
    }
    let credentials = ctx.process().credentials();
    let can_raise_hard_limit = credentials.euid == 0
        && credentials
            .capabilities
            .has_effective(CAP_SYS_RESOURCE)
            .unwrap_or(false);
    let process = rlimit_target_process_ctx(ctx, pid)?;
    let mut inner = process.inner_exclusive_access();
    let current = inner.resource_limits.get(resource);

    if let Some(new_limit) = new_limit {
        validate_new_rlimit(current, new_limit, can_raise_hard_limit)?;
    }
    if !old_limit.is_null() {
        write_user_value_ctx(ctx, old_limit, &current)?;
    }
    if let Some(new_limit) = new_limit {
        if !inner.resource_limits.set(resource, new_limit) {
            return Err(Errno::ENOTSUP);
        }
    }
    Ok(0)
}

pub fn sys_getrlimit_ctx(ctx: &SyscallContext, resource: i32, old_limit: *mut RLimit) -> KResult {
    if old_limit.is_null() {
        return Err(Errno::EFAULT);
    }
    sys_prlimit64_ctx(ctx, 0, resource, core::ptr::null(), old_limit)
}

pub fn sys_setrlimit_ctx(ctx: &SyscallContext, resource: i32, new_limit: *const RLimit) -> KResult {
    if new_limit.is_null() {
        return Err(Errno::EFAULT);
    }
    sys_prlimit64_ctx(ctx, 0, resource, new_limit, core::ptr::null_mut())
}
