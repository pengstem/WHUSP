mod clone;
mod exec;
mod fd;
mod id;
mod initproc;
mod manager;
mod process;
mod process_lifecycle;
mod processor;
mod signal;
#[allow(clippy::module_inception)]
mod task;

use self::id::TaskUserRes;
use crate::arch::__switch;
use crate::sbi::shutdown;
use alloc::{sync::Arc, vec::Vec};
use lazy_static::*;
use log::info;
use manager::fetch_task;
pub use process::{ProcessControlBlock, ProcessCpuTimesSnapshot};
pub(crate) use process::{ProcessProcSnapshot, RLimit, RLimitResource};

pub use crate::arch::TaskContext;
pub use clone::{CloneArgs, CloneFlags, clone_current_thread};
pub(crate) use fd::{FD_LIMIT, FdFlags, FdTableEntry};
pub use id::{IDLE_PID, KernelStack, PidHandle, kstack_alloc, pid_alloc};
pub(crate) use manager::any_process_references_mount;
pub(crate) use manager::list_process_snapshots;
pub use manager::{add_task, pid2process, remove_from_pid2process, wakeup_task};
pub use processor::{
    current_kstack_top, current_process, current_task, current_trap_cx, current_trap_cx_user_va,
    current_user_token, run_tasks, schedule, take_current_task,
};
pub use signal::{CLD_EXITED, SIGCHLD, SIGNAL_INFO_SLOTS, SignalFlags, SignalInfo};
pub use task::{TaskControlBlock, TaskStatus};

fn with_current_process(process_fn: impl FnOnce(&ProcessControlBlock)) {
    if let Some(task) = current_task() {
        if let Some(process) = task.process.upgrade() {
            process_fn(&process);
        }
    }
}

pub fn account_current_user_time_until(now_us: usize) {
    with_current_process(|process| process.account_user_time_until(now_us));
}

pub fn account_current_system_time_until(now_us: usize) {
    with_current_process(|process| process.account_system_time_until(now_us));
}

pub fn account_current_system_time() {
    account_current_system_time_until(crate::timer::get_time_us());
}

pub fn mark_current_user_time_entry(now_us: usize) {
    with_current_process(|process| process.mark_user_time_entry(now_us));
}

pub fn mark_current_kernel_time_entry(now_us: usize) {
    with_current_process(|process| process.mark_kernel_time_entry(now_us));
}

pub fn suspend_current_and_run_next() {
    // There must be an application running.
    account_current_system_time();
    let task = take_current_task().unwrap();

    // ---- access current TCB exclusively
    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut TaskContext;
    // Change status to Ready
    task_inner.task_status = TaskStatus::Ready;
    drop(task_inner);
    // ---- release current TCB

    // push back to ready queue.
    add_task(task);
    // jump to scheduling cycle
    schedule(task_cx_ptr);
}

/// This function must be followed by a schedule
pub fn block_current_task() -> *mut TaskContext {
    let (_task, task_cx_ptr) = block_current_task_no_schedule();
    task_cx_ptr
}

/// Mark the current task blocked and remove it from the processor without
/// scheduling. The caller must either enqueue the task on a wait queue and then
/// call `schedule`, or otherwise make it reachable for a later wakeup.
///
/// # Safety (logical)
/// The returned `Arc` must remain alive (e.g. on a wait queue) until after
/// `schedule(task_cx_ptr)` completes, because the pointer targets memory
/// owned by the `TaskControlBlock`.
pub fn block_current_task_no_schedule() -> (Arc<TaskControlBlock>, *mut TaskContext) {
    account_current_system_time();
    let task = take_current_task().unwrap();
    let mut task_inner = task.inner_exclusive_access();
    task_inner.task_status = TaskStatus::Blocked;
    let task_cx_ptr = &mut task_inner.task_cx as *mut TaskContext;
    drop(task_inner);
    (task, task_cx_ptr)
}

pub fn block_current_and_run_next() {
    let task_cx_ptr = block_current_task();
    schedule(task_cx_ptr);
}

/// Exit the current 'Running' task and run the next task in task list.
pub fn exit_current_and_run_next(exit_code: i32) {
    account_current_system_time();
    let task = take_current_task().unwrap();
    let mut task_inner = task.inner_exclusive_access();
    let process = task.process.upgrade().unwrap();
    let tid = task_inner.res.as_ref().unwrap().tid;
    // record exit code
    task_inner.exit_code = Some(exit_code);
    task_inner.res = None;
    // UNFINISHED: Exited non-main threads stay in the process task table until
    // process teardown. Linux clear_child_tid/futex-based join cleanup is not
    // implemented here yet.
    drop(task_inner);
    drop(task);
    // however, if this is the main thread of current process
    // the process should terminate at once
    if tid == 0 {
        let pid = process.getpid();
        if pid == IDLE_PID || Arc::ptr_eq(&process, &INITPROC) {
            println!(
                "[kernel] init process exit with exit_code {} ...",
                exit_code
            );
            if exit_code != 0 {
                //crate::sbi::shutdown(255); //255 == -1 for err hint
                shutdown(true);
            } else {
                //crate::sbi::shutdown(0); //0 for success hint
                shutdown(false);
            }
        }
        remove_from_pid2process(pid);
        let mut process_inner = process.inner_exclusive_access();
        // mark this process as a zombie process
        process_inner.is_zombie = true;
        // record exit code of main process
        process_inner.exit_code = exit_code;
        let parent = process_inner.parent.as_ref().and_then(|p| p.upgrade());

        {
            // move all child processes under init process
            let mut initproc_inner = INITPROC.inner_exclusive_access();
            for child in process_inner.children.iter() {
                child.inner_exclusive_access().parent = Some(Arc::downgrade(&INITPROC));
                initproc_inner.children.push(child.clone());
            }
        }

        // deallocate user res (including tid/trap_cx/ustack) of all threads
        // it has to be done before we dealloc the whole memory_set
        // otherwise they will be deallocated twice
        let mut recycle_res = Vec::<TaskUserRes>::new();
        for task in process_inner.tasks.iter().filter(|t| t.is_some()) {
            let task = task.as_ref().unwrap();
            let mut task_inner = task.inner_exclusive_access();
            if let Some(res) = task_inner.res.take() {
                recycle_res.push(res);
            }
        }
        // dealloc_tid and dealloc_user_res require access to PCB inner, so we
        // need to collect those user res first, then release process_inner
        // for now to avoid deadlock/double borrow problem.
        drop(process_inner);
        recycle_res.clear();

        if let Some(parent) = parent {
            let parent_task = {
                let mut parent_inner = parent.inner_exclusive_access();
                parent_inner.signals |= SignalFlags::SIGCHLD;
                parent_inner.signal_infos[SIGCHLD as usize] = Some(SignalInfo::child_exit(
                    SIGCHLD as i32,
                    pid as i32,
                    exit_code,
                ));
                parent_inner
                    .tasks
                    .first()
                    .and_then(|task| task.as_ref().map(Arc::clone))
            };
            if let Some(parent_task) = parent_task {
                let is_blocked =
                    parent_task.inner_exclusive_access().task_status == TaskStatus::Blocked;
                if is_blocked {
                    wakeup_task(parent_task);
                }
            }
        }

        let mut process_inner = process.inner_exclusive_access();
        process_inner.children.clear();
        // deallocate other data in user space i.e. program code/data section
        process_inner.memory_set.recycle_data_pages();
        // drop file descriptors
        process_inner.fd_table.clear();
        // Remove all tasks except for the main thread itself.
        // This is because we are still using the kstack under the TCB
        // of the main thread. This TCB, including its kstack, will be
        // deallocated when the process is reaped via waitpid.
        while process_inner.tasks.len() > 1 {
            process_inner.tasks.pop();
        }
    }
    drop(process);
    // we do not have to save task context
    let mut _unused = TaskContext::zero_init();
    schedule(&mut _unused as *mut _);
}

lazy_static! {
    pub static ref INITPROC: Arc<ProcessControlBlock> = {
        let init = initproc::load().expect("kernel initproc /musl/busybox not found");
        info!("loading initproc from {}", init.path);
        ProcessControlBlock::new_with_args(init.data.as_slice(), init.argv, init.envp)
    };
}

pub fn add_initproc() {
    let _initproc = INITPROC.clone();
}

pub fn check_signals_of_current() -> Option<(i32, &'static str)> {
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    process_inner.signals.check_error()
}

pub fn current_add_signal(signal: SignalFlags) {
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    process_inner.signals |= signal;
}
