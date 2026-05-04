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
use crate::sync::UPIntrFreeCell;
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
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
pub(crate) use manager::processes_snapshot;
pub(crate) use manager::remove_ready_tasks_of_process;
pub use manager::{add_task, pid2process, remove_from_pid2process, wakeup_task};
pub use processor::{
    current_kstack_top, current_process, current_task, current_trap_cx, current_trap_cx_user_va,
    current_user_token, run_tasks, schedule, take_current_task,
};
pub use signal::{
    CLD_EXITED, SIGCHLD, SIGKILL, SIGNAL_INFO_SLOTS, SIGSTOP, SignalAction, SignalFlags, SignalInfo,
};
pub(crate) use signal::{flags_to_linux_sigset, linux_sigset_to_flags};
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

fn try_account_current_system_time_until(now_us: usize) {
    with_current_process(|process| process.try_account_system_time_until(now_us));
}

pub fn account_current_system_time() {
    account_current_system_time_until(crate::timer::get_time_us());
}

fn try_account_current_system_time() {
    try_account_current_system_time_until(crate::timer::get_time_us());
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
    // CONTEXT: `SleepMutex::lock()` can be reached from exit-time destructors
    // while nearby PCB cleanup is in progress. CPU accounting must not turn
    // that cleanup path into a RefCell panic; skipping one sample is preferable
    // to aborting the kernel while the task is about to block.
    try_account_current_system_time();
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

static EXITED_DIRTY: AtomicBool = AtomicBool::new(false);

// CONTEXT: EXITED_TASKS defers Arc<TaskControlBlock> drops past the
// __switch boundary so kernel stacks remain mapped until the next
// scheduling tick completes.
lazy_static! {
    static ref EXITED_TASKS: UPIntrFreeCell<Vec<Arc<TaskControlBlock>>> =
        unsafe { UPIntrFreeCell::new(Vec::new()) };
}

fn queue_exited_task(task: Arc<TaskControlBlock>) {
    EXITED_TASKS.exclusive_access().push(task);
    EXITED_DIRTY.store(true, Ordering::Release);
}

pub(crate) fn reap_exited_tasks() {
    if !EXITED_DIRTY.swap(false, Ordering::Acquire) {
        return;
    }
    let exited_tasks = {
        let mut tasks = EXITED_TASKS.exclusive_access();
        core::mem::take(&mut *tasks)
    };
    drop(exited_tasks);
}

fn terminate_sibling_threads(
    process: &Arc<ProcessControlBlock>,
    current_tid: usize,
    process_token: usize,
    process_id: usize,
    exit_code: i32,
) {
    let mut clear_child_tids = Vec::new();
    let mut recycle_res = Vec::<TaskUserRes>::new();
    let mut robust_tasks = Vec::new();
    let mut exited_threads = Vec::new();
    {
        let mut process_inner = process.inner_exclusive_access();
        for (tid, task_slot) in process_inner.tasks.iter_mut().enumerate() {
            if tid == current_tid {
                continue;
            }
            let Some(task) = task_slot.as_ref().map(Arc::clone) else {
                continue;
            };
            let mut task_inner = task.inner_exclusive_access();
            task_inner.task_status = TaskStatus::Exited;
            task_inner.exit_code = Some(exit_code);
            if let Some(clear_child_tid) = task_inner.clear_child_tid.take() {
                clear_child_tids.push(clear_child_tid);
            }
            robust_tasks.push(Arc::clone(&task));
            if let Some(res) = task_inner.res.take() {
                recycle_res.push(res);
            }
            drop(task_inner);
            if tid != 0 {
                exited_threads.push(task);
                *task_slot = None;
            }
        }
    }

    for task in robust_tasks {
        crate::syscall::exit_robust_list(&task, process_token, process_id);
    }
    for clear_child_tid in clear_child_tids {
        crate::syscall::clear_child_tid_and_wake(process_token, process_id, clear_child_tid);
    }
    recycle_res.clear();
    for task in exited_threads {
        queue_exited_task(task);
    }
}

pub(crate) fn queue_signal_to_task(
    task: Arc<TaskControlBlock>,
    signal: SignalFlags,
    info: SignalInfo,
) {
    if signal.is_empty() {
        return;
    }
    {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.pending_signals |= signal;
        if let Some(slot) = task_inner.signal_infos.get_mut(info.signo as usize) {
            *slot = Some(info);
        }
    }
    wakeup_task(task);
}

fn exit_current(exit_code: i32, group_exit: bool) {
    account_current_system_time();
    let current = current_task().unwrap();
    let process = current.process.upgrade().unwrap();
    let process_token = process.inner_exclusive_access().get_user_token();
    let process_id = process.getpid();
    let (tid, clear_child_tid) = {
        let mut task_inner = current.inner_exclusive_access();
        (task_inner.tid, task_inner.clear_child_tid.take())
    };
    crate::syscall::exit_robust_list(&current, process_token, process_id);
    if let Some(clear_child_tid) = clear_child_tid {
        crate::syscall::clear_child_tid_and_wake(process_token, process_id, clear_child_tid);
    }
    current.inner_exclusive_access().res = None;

    let process_exit = group_exit || tid == 0;
    let exited_thread = if tid == 0 {
        None
    } else {
        let mut process_inner = process.inner_exclusive_access();
        if tid < process_inner.tasks.len() {
            process_inner.tasks[tid] = None;
        }
        Some(Arc::clone(&current))
    };
    if process_exit {
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
        terminate_sibling_threads(&process, tid, process_token, process_id, exit_code);
        remove_ready_tasks_of_process(pid);
        crate::syscall::remove_process_futex_waiters(pid);
        remove_from_pid2process(pid);
        let (parent, children, fd_table) = {
            let mut process_inner = process.inner_exclusive_access();
            // mark this process as a zombie process
            process_inner.is_zombie = true;
            // record exit code of main process
            process_inner.exit_code = exit_code;
            let parent = process_inner.parent.as_ref().and_then(|p| p.upgrade());
            let children = core::mem::take(&mut process_inner.children);
            // deallocate other data in user space i.e. program code/data section
            process_inner.memory_set.recycle_data_pages();
            // Take the fd table out while the current task is still installed.
            // Dropping VFS file objects can take SleepMutex-backed mount locks.
            let fd_table = core::mem::take(&mut process_inner.fd_table);
            // Keep only the main task in the zombie process for waitpid reaping.
            // Non-main exiting tasks are parked in EXITED_TASKS until their kernel
            // stacks are no longer active across the next schedule boundary.
            while process_inner.tasks.len() > 1 {
                process_inner.tasks.pop();
            }
            (parent, children, fd_table)
        };

        // move all child processes under init process
        let mut initproc_inner = INITPROC.inner_exclusive_access();
        for child in children {
            child.inner_exclusive_access().parent = Some(Arc::downgrade(&INITPROC));
            initproc_inner.children.push(child);
        }
        drop(initproc_inner);

        drop(fd_table);

        if let Some(parent) = parent {
            let parent_task = {
                let parent_inner = parent.inner_exclusive_access();
                parent_inner
                    .tasks
                    .first()
                    .and_then(|task| task.as_ref().map(Arc::clone))
            };
            if let Some(parent_task) = parent_task {
                queue_signal_to_task(
                    Arc::clone(&parent_task),
                    SignalFlags::SIGCHLD,
                    SignalInfo::child_exit(SIGCHLD as i32, pid as i32, exit_code),
                );
                let is_blocked =
                    parent_task.inner_exclusive_access().task_status == TaskStatus::Blocked;
                if is_blocked {
                    wakeup_task(parent_task);
                }
            }
        }
    }
    let task = take_current_task().unwrap();
    let mut task_inner = task.inner_exclusive_access();
    task_inner.exit_code = Some(exit_code);
    task_inner.task_status = TaskStatus::Exited;
    drop(task_inner);
    if let Some(task) = exited_thread {
        queue_exited_task(task);
    }
    drop(current);
    drop(task);
    drop(process);
    // we do not have to save task context
    let mut _unused = TaskContext::zero_init();
    schedule(&mut _unused as *mut _);
}

/// Exit the current 'Running' task and run the next task in task list.
pub fn exit_current_and_run_next(exit_code: i32) {
    exit_current(exit_code, false);
}

pub fn exit_current_group_and_run_next(exit_code: i32) {
    exit_current(exit_code, true);
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
    let task = current_task()?;
    let process = task.process.upgrade()?;
    let pending = {
        let task_inner = task.inner_exclusive_access();
        // CONTEXT: bitflags `!` truncates to named flags unless the flag type
        // declares external bits, while this kernel keeps Linux real-time
        // signals through `from_bits_retain`.
        SignalFlags::from_bits_retain(
            task_inner.pending_signals.bits() & !task_inner.signal_mask.bits(),
        )
    };
    let signum = pending.bits().trailing_zeros() as usize;
    if signum >= SIGNAL_INFO_SLOTS {
        return None;
    }
    let action = process.inner_exclusive_access().signal_actions[signum];
    // CONTEXT: Linux's default disposition for SIGCHLD is ignore. Shells and
    // runtest still reap children with wait/waitid or explicit sigtimedwait.
    if action.is_ignore() || (signum == SIGCHLD as usize && !action.has_user_handler()) {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.clear_pending(signum as u32);
        return None;
    }
    if action.has_user_handler() {
        return None;
    }
    pending.check_error()
}

pub fn current_add_signal(signal: SignalFlags) {
    if signal.is_empty() {
        return;
    }
    if let Some(task) = current_task() {
        let signum = signal.bits().trailing_zeros() as i32;
        queue_signal_to_task(task, signal, SignalInfo::user(signum, 0));
    }
}
