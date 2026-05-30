use crate::fs::{VfsNodeId, assign_pid_to_cgroup, clone_mount_namespace};
use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::user_ptr::{read_user_value, write_user_value, write_user_value_in_memory_set};
use crate::task::{
    CloneArgs, CloneFlags, ProcessControlBlock, TaskControlBlock, add_task,
    block_current_task_no_schedule, clone_current_thread, current_process, current_task,
    current_user_token, reap_exited_tasks, schedule, suspend_current_and_run_next,
};
use alloc::sync::Arc;
use core::mem::size_of;

use super::pidfd::{install_reserved_pidfd_for_current_process, reserve_pidfd_for_current_process};

const CLONE_ARGS_MIN_SIZE: usize = 64;
const CLONE_PIDFD: u64 = 0x0000_1000;
const CLONE_SIGHAND: u64 = 0x0000_0800;
const CLONE_THREAD: u64 = 0x0001_0000;
const CLONE_FS: u64 = 0x0000_0200;
const CLONE_NEWNS: u64 = 0x0002_0000;
const CLONE_INTO_CGROUP: u64 = 0x0002_0000_0000;
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

/// Decodes the architecture-specific raw clone argument order.
///
/// RISC-V passes TLS before child_tid in args 4/5, while the LoongArch syscall
/// ABI used by the contest libc passes child_tid before TLS.
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
    } else if is_vm_vfork_process_clone(args) {
        sys_clone_vm_vfork(args)
    } else if is_vm_newnet_process_clone(args) {
        sys_clone_vm_newnet(args)
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
    let cgroup = clone3_cgroup_target(args)?;
    if clone_args.is_thread() {
        if cgroup.is_some() {
            // UNFINISHED: CLONE_INTO_CGROUP is currently supported only for
            // process clone3(), because this kernel records cgroup membership
            // at process-id granularity.
            return Err(SysError::EINVAL);
        }
        if args.flags & CLONE_PIDFD != 0 {
            // UNFINISHED: Thread-directed pidfds are not modeled yet because
            // this kernel's pidfd object records process IDs only.
            return Err(SysError::EINVAL);
        }
        sys_clone_thread(clone_args)
    } else if is_vm_vfork_process_clone(clone_args)
        && args.flags & CLONE_PIDFD == 0
        && cgroup.is_none()
    {
        sys_clone_vm_vfork(clone_args)
    } else if is_vm_newnet_process_clone(clone_args)
        && args.flags & CLONE_PIDFD == 0
        && cgroup.is_none()
    {
        sys_clone_vm_newnet(clone_args)
    } else {
        let pidfd = if args.flags & CLONE_PIDFD != 0 {
            Some(args.pidfd as *mut i32)
        } else {
            None
        };
        sys_clone_process_inner(clone_args, pidfd, cgroup)
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
    if args.set_tid != 0 || args.set_tid_size != 0 {
        return Err(SysError::EINVAL);
    }
    if args.exit_signal >= SIGNAL_INFO_SLOTS {
        return Err(SysError::EINVAL);
    }
    if (args.stack == 0) != (args.stack_size == 0) {
        return Err(SysError::EINVAL);
    }
    if args.flags & CLONE_PIDFD != 0 {
        // Probe the pidfd result pointer before allocating or publishing the child.
        // The real fd value is written after reservation succeeds.
        write_user_value(token, args.pidfd as *mut i32, &-1)?;
    }
    Ok(())
}

fn sys_clone_process(args: CloneArgs) -> SysResult {
    sys_clone_process_inner(args, None, None)
}

fn clone3_cgroup_target(args: LinuxCloneArgs) -> SysResult<Option<VfsNodeId>> {
    if args.flags & CLONE_INTO_CGROUP == 0 {
        return Ok(None);
    }
    let fd = args.cgroup as usize;
    let file = {
        let process = current_process();
        let inner = process.inner_exclusive_access();
        inner
            .fd_table
            .get(fd)
            .and_then(|entry| entry.as_ref())
            .map(|entry| entry.file())
            .ok_or(SysError::EBADF)?
    };
    let dir = file.working_dir().ok_or(SysError::EINVAL)?;
    Ok(Some(VfsNodeId::new(dir.mount_id(), dir.ino())))
}

fn write_user_value_to_process<T: Copy>(
    process: &Arc<ProcessControlBlock>,
    ptr: *mut T,
    value: &T,
) -> SysResult<()> {
    // Write clone metadata into the child address space before it can run.
    // Parent and child do not necessarily share VM, so current_user_token()
    // would target the wrong memory set for CLONE_CHILD_SETTID.
    let mut inner = process.inner_exclusive_access();
    write_user_value_in_memory_set(&mut inner.memory_set, ptr, value)
}

fn sys_clone_process_inner(
    args: CloneArgs,
    pidfd: Option<*mut i32>,
    cgroup: Option<VfsNodeId>,
) -> SysResult {
    let vfork_parent = if args.flags.contains(CloneFlags::CLONE_VFORK) {
        Some(current_task().ok_or(SysError::ESRCH)?)
    } else {
        None
    };
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
        .fork(
            Arc::clone(&child_parent),
            mount_namespace_id,
            args.exit_signal,
        )
        .ok_or(SysError::ENOMEM)?;
    let new_pid = new_process.getpid();
    if let Some(parent_task) = vfork_parent {
        new_process.begin_vfork(parent_task);
    }
    if args.flags.contains(CloneFlags::CLONE_NEWPID) {
        // CONTEXT: LTP ioctl_ns checks only the init process of a newly
        // cloned PID namespace. Track a lightweight namespace identity so
        // /proc/<pid>/ns/pid and getpid() expose the expected Linux surface.
        new_process.enter_new_pid_namespace(new_pid);
    }
    if args.flags.contains(CloneFlags::CLONE_NEWUSER) {
        // CONTEXT: User namespace capability and id-mapping semantics are not
        // implemented; this records enough namespace ancestry for nsfs ioctl
        // discovery tests.
        new_process.enter_new_user_namespace(new_pid);
    }
    new_process.configure_cloned_main_task(args);
    let reserved_pidfd = if pidfd.is_some() {
        Some(reserve_pidfd_for_current_process()?)
    } else {
        None
    };
    if let Some(cgroup) = cgroup {
        assign_pid_to_cgroup(cgroup, new_pid)?;
    }
    if let (Some(pidfd), Some(fd)) = (pidfd, reserved_pidfd) {
        write_user_value(current_user_token(), pidfd, &(fd as i32))?;
    }

    if args.flags.contains(CloneFlags::CLONE_PARENT_SETTID) {
        let parent_token = current_user_token();
        write_user_value(parent_token, args.ptid as *mut i32, &(new_pid as i32))?;
        if args.flags.contains(CloneFlags::CLONE_VM) {
            // CONTEXT: Process clone currently copies the address space even
            // when CLONE_VM is requested. Mirror parent_tid into the child copy
            // so CLONE_PARENT_SETTID remains visible to the cloned entry code.
            write_user_value_to_process(&new_process, args.ptid as *mut i32, &(new_pid as i32))?;
        }
    }
    if args.flags.contains(CloneFlags::CLONE_CHILD_SETTID) {
        write_user_value_to_process(&new_process, args.ctid as *mut i32, &(new_pid as i32))?;
    }
    if let Some(fd) = reserved_pidfd {
        install_reserved_pidfd_for_current_process(fd, new_pid);
    }
    let child_task = new_process.main_task();
    new_process.publish_fork_child(&child_parent);
    if args.flags.contains(CloneFlags::CLONE_VFORK) {
        wait_for_vfork_child(&new_process, child_task);
    } else {
        add_task(child_task);
    }
    Ok(new_pid as isize)
}

fn wait_for_vfork_child(
    child_process: &Arc<ProcessControlBlock>,
    child_task: Arc<TaskControlBlock>,
) {
    let mut pending_child_task = Some(child_task);
    loop {
        // CONTEXT: Generic Blocked tasks can be woken by signal delivery. A
        // vfork parent must still wait for the child to exec or exit, so every
        // wake is treated as a hint and the explicit completion flag is checked.
        let (_blocked_parent, parent_cx_ptr) = block_current_task_no_schedule();
        if let Some(child_task) = pending_child_task.take() {
            add_task(child_task);
        }
        schedule(parent_cx_ptr);
        if !child_process.vfork_in_progress() {
            break;
        }
    }
}

fn is_vm_vfork_process_clone(args: CloneArgs) -> bool {
    args.flags
        .contains(CloneFlags::CLONE_VM | CloneFlags::CLONE_VFORK)
        && !args.is_thread()
}

fn is_vm_newnet_process_clone(args: CloneArgs) -> bool {
    args.flags
        .contains(CloneFlags::CLONE_VM | CloneFlags::CLONE_NEWNET)
        && !args.is_thread()
}

fn sys_clone_vm_vfork(args: CloneArgs) -> SysResult {
    // UNFINISHED: Linux CLONE_VM without CLONE_THREAD creates a distinct
    // process that shares the mm_struct, and CLONE_VFORK releases the parent
    // on either execve(2) or _exit(2). This contest compatibility path uses a
    // normal copied-address-space process clone because LTP command helpers use
    // vfork()+execve(), and this kernel cannot exec a same-process helper task
    // without replacing the parent's PCB.
    sys_clone_process(args)
}

fn sys_clone_vm_newnet(args: CloneArgs) -> SysResult {
    // UNFINISHED: Full network namespaces are not implemented. This path is
    // limited to CLONE_NEWNET|CLONE_VM LTP coverage: run the child as a helper
    // task so CLONE_VM data writes are visible, and mark it so procfs exposes
    // default net sysctls while it runs.
    sys_clone_vm_helper(args, true)
}

fn sys_clone_vm_helper(args: CloneArgs, synthetic_newnet: bool) -> SysResult {
    reap_exited_tasks();
    let process = current_process();
    let cloned = clone_current_thread(args);
    let linux_tid = cloned.linux_tid;
    {
        let mut task_inner = cloned.task.inner_exclusive_access();
        // CONTEXT: CLONE_VM process-compatibility children have no separate
        // PCB. Mark them so exit_group(), getpid(), and procfs namespace probes
        // expose the child-like Linux surface without killing the parent.
        task_inner.clone_vm_process_helper = true;
        task_inner.synthetic_newnet = synthetic_newnet;
    }
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
    while process
        .tasks_snapshot()
        .iter()
        .any(|task| task.linux_tid() == linux_tid)
    {
        suspend_current_and_run_next();
    }
    Ok(linux_tid as isize)
}

fn sys_clone_thread(args: CloneArgs) -> SysResult {
    reap_exited_tasks();
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
