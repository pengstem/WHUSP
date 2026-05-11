use crate::fs::clone_mount_namespace;
use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::user_ptr::{read_user_value, write_user_value};
use crate::task::{
    CloneArgs, CloneFlags, add_task, clone_current_thread, current_process, current_user_token,
};
use alloc::sync::Arc;
use core::mem::size_of;

const CLONE_ARGS_MIN_SIZE: usize = 64;
const CLONE_PIDFD: u64 = 0x0000_1000;
const CLONE_SIGHAND: u64 = 0x0000_0800;
const CLONE_THREAD: u64 = 0x0001_0000;
const CLONE_FS: u64 = 0x0000_0200;
const CLONE_NEWNS: u64 = 0x0002_0000;
const CLONE_INTO_CGROUP: u64 = 0x2000_0000_0;
const SIGNAL_INFO_SLOTS: u64 = crate::task::SIGNAL_INFO_SLOTS as u64;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LinuxCloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

fn clone_tls_and_ctid_args(raw_arg4: usize, raw_arg5: usize) -> (usize, usize) {
    #[cfg(target_arch = "loongarch64")]
    {
        (raw_arg5, raw_arg4)
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        (raw_arg4, raw_arg5)
    }
}

pub fn sys_clone(
    flags: usize,
    stack: usize,
    ptid: usize,
    raw_arg4: usize,
    raw_arg5: usize,
) -> SysResult {
    let (tls, ctid) = clone_tls_and_ctid_args(raw_arg4, raw_arg5);
    let Some(args) = CloneArgs::parse(flags, stack, ptid, tls, ctid) else {
        return Err(SysError::EINVAL);
    };
    if args.is_thread() {
        sys_clone_thread(args)
    } else {
        sys_clone_process(args)
    }
}

pub fn sys_clone3(args: *const LinuxCloneArgs, size: usize) -> SysResult {
    if size == 0 || size < CLONE_ARGS_MIN_SIZE {
        return Err(SysError::EINVAL);
    }
    // UNFINISHED: Linux accepts larger clone_args when the unknown tail is
    // zeroed. This kernel only understands the current structure size.
    if size > size_of::<LinuxCloneArgs>() {
        return Err(SysError::EFAULT);
    }
    let token = current_user_token();
    let args = read_user_value(token, args)?;
    validate_clone3_args(args, token)?;

    if args.flags & CLONE_PIDFD != 0 {
        // UNFINISHED: pidfd descriptors and pidfd_send_signal are not modeled
        // yet. Bad pidfd pointers are still reported as EFAULT for clone302.
        return Err(SysError::EINVAL);
    }

    let stack_top = if args.stack == 0 {
        0
    } else {
        args.stack
            .checked_add(args.stack_size)
            .ok_or(SysError::EINVAL)? as usize
    };
    let clone_flags = CloneFlags::from_bits_truncate(args.flags as u32);
    let clone_args = CloneArgs::from_parts(
        clone_flags,
        args.exit_signal as u32,
        stack_top,
        args.parent_tid as usize,
        args.tls as usize,
        args.child_tid as usize,
    )
    .ok_or(SysError::EINVAL)?;
    if clone_args.is_thread() {
        sys_clone_thread(clone_args)
    } else {
        sys_clone_process(clone_args)
    }
}

fn validate_clone3_args(args: LinuxCloneArgs, token: usize) -> SysResult<()> {
    if args.flags & CLONE_SIGHAND != 0 && args.flags & CloneFlags::CLONE_VM.bits() as u64 == 0 {
        return Err(SysError::EINVAL);
    }
    if args.flags & CLONE_THREAD != 0 && args.flags & CLONE_SIGHAND == 0 {
        return Err(SysError::EINVAL);
    }
    if args.flags & CLONE_FS != 0 && args.flags & CLONE_NEWNS != 0 {
        return Err(SysError::EINVAL);
    }
    if args.flags & CLONE_INTO_CGROUP != 0 || args.set_tid != 0 || args.set_tid_size != 0 {
        return Err(SysError::EINVAL);
    }
    if args.exit_signal >= SIGNAL_INFO_SLOTS {
        return Err(SysError::EINVAL);
    }
    if (args.stack == 0) != (args.stack_size == 0) {
        return Err(SysError::EINVAL);
    }
    if args.flags & CLONE_PIDFD != 0 {
        write_user_value(token, args.pidfd as *mut i32, &-1)?;
    }
    Ok(())
}

fn sys_clone_process(args: CloneArgs) -> SysResult {
    let current_process = current_process();
    let child_parent = if args.flags.contains(CloneFlags::CLONE_PARENT) {
        current_process.parent_process().ok_or(SysError::EINVAL)?
    } else {
        Arc::clone(&current_process)
    };
    let mount_namespace_id = if args.flags.contains(CloneFlags::CLONE_NEWNS) {
        clone_mount_namespace(current_process.mount_namespace_id())
    } else {
        current_process.mount_namespace_id()
    };
    let new_process = current_process
        .fork(child_parent, mount_namespace_id, args.exit_signal)
        .ok_or(SysError::ENOMEM)?;
    let new_pid = new_process.getpid();
    let child_token = new_process.configure_cloned_main_task(args);

    if args.flags.contains(CloneFlags::CLONE_PARENT_SETTID) {
        let parent_token = current_user_token();
        write_user_value(parent_token, args.ptid as *mut i32, &(new_pid as i32))?;
        if args.flags.contains(CloneFlags::CLONE_VM) {
            // CONTEXT: Process clone currently copies the address space even
            // when CLONE_VM is requested. Mirror parent_tid into the child copy
            // so CLONE_PARENT_SETTID remains visible to the cloned entry code.
            write_user_value(child_token, args.ptid as *mut i32, &(new_pid as i32))?;
        }
    }
    if args.flags.contains(CloneFlags::CLONE_CHILD_SETTID) {
        write_user_value(child_token, args.ctid as *mut i32, &(new_pid as i32))?;
    }
    Ok(new_pid as isize)
}

fn sys_clone_thread(args: CloneArgs) -> SysResult {
    let process = current_process();
    let cloned = clone_current_thread(args);
    let process_token = process.attach_task(Arc::clone(&cloned.task));

    if args.flags.contains(CloneFlags::CLONE_PARENT_SETTID) {
        write_user_value(
            process_token,
            args.ptid as *mut i32,
            &(cloned.linux_tid as i32),
        )?;
    }
    if args.flags.contains(CloneFlags::CLONE_CHILD_SETTID) {
        write_user_value(
            process_token,
            args.ctid as *mut i32,
            &(cloned.linux_tid as i32),
        )?;
    }
    add_task(cloned.task);
    Ok(cloned.linux_tid as isize)
}
