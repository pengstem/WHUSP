use crate::{
    syscall::{
        errno::{SysError, SysResult},
        user_ptr::write_user_value,
    },
    task::{TaskControlBlock, current_task, current_user_token, processes_snapshot},
};
use alloc::sync::Arc;

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

pub fn sys_sched_getparam(pid: isize, param: usize) -> SysResult {
    if param == 0 {
        return Err(SysError::EINVAL);
    }
    let _task = sched_target_task(pid)?;
    // CONTEXT: The kernel does not yet expose Linux scheduling classes. All
    // runnable tasks are reported as SCHED_OTHER-compatible, whose static
    // priority is 0.
    let sched_param = LinuxSchedParam { sched_priority: 0 };
    write_user_value(
        current_user_token(),
        param as *mut LinuxSchedParam,
        &sched_param,
    )?;
    Ok(0)
}
