use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::user_ptr::write_user_value;
use crate::task::{
    add_task, clone_current_thread, current_process, current_user_token, CloneArgs, CloneFlags,
};
use alloc::sync::Arc;

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

fn sys_clone_process(args: CloneArgs) -> SysResult {
    let current_process = current_process();
    let child_parent = if args.flags.contains(CloneFlags::CLONE_PARENT) {
        current_process.parent_process().ok_or(SysError::EINVAL)?
    } else {
        Arc::clone(&current_process)
    };
    let new_process = current_process.fork(child_parent);
    let new_pid = new_process.getpid();
    let child_token = new_process.configure_cloned_main_task(args);

    if args.flags.contains(CloneFlags::CLONE_PARENT_SETTID) {
        let parent_token = current_user_token();
        write_user_value(parent_token, args.ptid as *mut i32, &(new_pid as i32))?;
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
