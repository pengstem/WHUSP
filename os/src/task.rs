mod clone;
mod contest_runner;
mod exec;
mod fd;
pub(crate) mod futex;
mod id;
mod initproc;
mod ltp_whitelist;
mod manager;
mod process;
mod process_lifecycle;
mod processor;
mod signal;
#[allow(clippy::module_inception)]
mod task;

use self::id::TaskUserRes;
use crate::arch::__switch;
use crate::fs::untrack_regular_file_executable;
use crate::sbi::shutdown;
use crate::sync::UPIntrFreeCell;
use crate::syscall::{release_flock_locks_for_closed_fd_table, release_record_locks_for_process};
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use lazy_static::*;
use log::info;
use manager::fetch_task;
pub(crate) use process::{Credentials, PathSnapshot, ProcessProcSnapshot, RLimit, RLimitResource};
pub use process::{ProcessControlBlock, ProcessCpuTimesSnapshot};
pub(crate) const CAP_IPC_LOCK: usize = process::CapabilitySets::CAP_IPC_LOCK;
pub(crate) const CAP_SETPCAP: usize = process::CapabilitySets::CAP_SETPCAP;
pub(crate) const CAP_SYS_CHROOT: usize = process::CapabilitySets::CAP_SYS_CHROOT;
pub(crate) const CAP_SYS_ADMIN: usize = process::CapabilitySets::CAP_SYS_ADMIN;
pub(crate) const CAP_SYS_RESOURCE: usize = process::CapabilitySets::CAP_SYS_RESOURCE;

pub use crate::arch::TaskContext;
pub use clone::{CloneArgs, CloneFlags, clone_current_thread};
pub(crate) use fd::{FD_LIMIT, FdFlags, FdTableEntry};
pub use id::{IDLE_PID, KernelStack, PidHandle, kstack_alloc, pid_alloc};
pub(crate) use manager::any_process_references_mount;
pub(crate) use manager::list_process_snapshots;
pub(crate) use manager::processes_snapshot;
pub(crate) use manager::remove_ready_tasks_of_process;
pub use manager::{add_task, pid2process, remove_from_pid2process, wakeup_task};
#[cfg(target_arch = "riscv64")]
pub use processor::current_trap_cx_user_va;
pub use processor::{
    current_kstack_top, current_process, current_task, current_trap_cx, current_user_token,
    run_tasks, schedule, take_current_task,
};
pub use signal::{
    CLD_EXITED, DefaultSignalAction, MINSIGSTKSZ, SA_RESTART, SIGCHLD, SIGKILL, SIGNAL_INFO_SLOTS,
    SIGSTOP, SS_DISABLE, SS_ONSTACK, SigAltStack, SignalAction, SignalFlags, SignalInfo,
    default_signal_action, default_signal_error,
};
#[cfg(target_arch = "riscv64")]
pub use signal::{SI_TKILL, SIGRT_1, SIGRTMIN};
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
    let mut released_thread_keyrings = Vec::new();
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
            if let Some(keyring) = task_inner.thread_keyring.take() {
                released_thread_keyrings.push(keyring);
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
        futex::exit_robust_list(&task, process_token, process_id);
    }
    for clear_child_tid in clear_child_tids {
        futex::clear_child_tid_and_wake(process_token, process_id, clear_child_tid);
    }
    for keyring in released_thread_keyrings {
        crate::syscall::keyring::release_keyring_tree(keyring);
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
    let (tid, clear_child_tid, thread_keyring) = {
        let mut task_inner = current.inner_exclusive_access();
        (
            task_inner.tid,
            task_inner.clear_child_tid.take(),
            task_inner.thread_keyring.take(),
        )
    };
    futex::exit_robust_list(&current, process_token, process_id);
    if let Some(clear_child_tid) = clear_child_tid {
        futex::clear_child_tid_and_wake(process_token, process_id, clear_child_tid);
    }
    if let Some(keyring) = thread_keyring {
        crate::syscall::keyring::release_keyring_tree(keyring);
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
        futex::remove_process_futex_waiters(pid);
        let (parent, children, fd_table, flushes, executable_node, exit_signal, process_keyring) = {
            let mut process_inner = process.inner_exclusive_access();
            // mark this process as a zombie process
            process_inner.is_zombie = true;
            // record exit code of main process
            process_inner.exit_code = exit_code;
            let parent = process_inner.parent.as_ref().and_then(|p| p.upgrade());
            let exit_signal = process_inner.exit_signal;
            let children = core::mem::take(&mut process_inner.children);
            // deallocate other data in user space i.e. program code/data section
            let flushes = process_inner.memory_set.recycle_data_pages();
            let executable_node = process_inner.executable_node.take();
            let process_keyring = process_inner.process_keyring.take();
            // Take the fd table out while the current task is still installed.
            // Dropping VFS file objects can take SleepMutex-backed mount locks.
            let fd_table = core::mem::take(&mut process_inner.fd_table);
            // Keep only the main task in the zombie process for waitpid reaping.
            // Non-main exiting tasks are parked in EXITED_TASKS until their kernel
            // stacks are no longer active across the next schedule boundary.
            while process_inner.tasks.len() > 1 {
                process_inner.tasks.pop();
            }
            (
                parent,
                children,
                fd_table,
                flushes,
                executable_node,
                exit_signal,
                process_keyring,
            )
        };

        if let Some(keyring) = process_keyring {
            crate::syscall::keyring::release_keyring_tree(keyring);
        }
        for flush in flushes {
            flush.write_back();
        }
        if let Some(node) = executable_node {
            untrack_regular_file_executable(node);
        }
        release_record_locks_for_process(pid);
        release_flock_locks_for_closed_fd_table(&fd_table);

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
                if let Some(signal) = SignalFlags::from_signum(exit_signal) {
                    if !signal.is_empty() {
                        queue_signal_to_task(
                            Arc::clone(&parent_task),
                            signal,
                            SignalInfo::child_exit(exit_signal as i32, pid as i32, exit_code),
                        );
                    }
                }
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
    // CONTEXT: Linux's default disposition for SIGCHLD is ignore. PID 1 is
    // also protected from ordinary default-disposition signals unless it has
    // installed a user handler; LTP heartbeat children can otherwise kill the
    // kernel-owned init shell with a stray SIGUSR1.
    if action.is_ignore()
        || (default_signal_action(signum) == Some(DefaultSignalAction::Ignore)
            && !action.has_user_handler())
        || (Arc::ptr_eq(&process, &INITPROC) && !action.has_user_handler())
    {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.clear_pending(signum as u32);
        return None;
    }
    if action.has_user_handler() {
        return None;
    }
    default_signal_error(signum)
}

fn current_has_deliverable_signal_matching(predicate: impl Fn(SignalAction) -> bool) -> bool {
    let Some(task) = current_task() else {
        return false;
    };
    let Some(process) = task.process.upgrade() else {
        return false;
    };
    let pending = {
        let task_inner = task.inner_exclusive_access();
        SignalFlags::from_bits_retain(
            task_inner.pending_signals.bits() & !task_inner.signal_mask.bits(),
        )
    };
    for signum in 1..SIGNAL_INFO_SLOTS {
        let Some(signal) = SignalFlags::from_signum(signum as u32) else {
            continue;
        };
        if !pending.contains(signal) {
            continue;
        }
        let action = process.inner_exclusive_access().signal_actions[signum];
        if action.is_ignore() || !action.has_user_handler() {
            continue;
        }
        if !predicate(action) {
            continue;
        }
        if !crate::arch::signal::can_deliver_user_signal(signum) {
            continue;
        }
        return true;
    }
    false
}

pub fn current_has_deliverable_signal() -> bool {
    current_has_deliverable_signal_matching(|_| true)
}

pub fn current_has_interrupting_signal() -> bool {
    let Some(task) = current_task() else {
        return false;
    };
    let Some(process) = task.process.upgrade() else {
        return false;
    };
    let pending = {
        let inner = task.inner_exclusive_access();
        SignalFlags::from_bits_retain(inner.pending_signals.bits() & !inner.signal_mask.bits())
    };
    for signum in 1..SIGNAL_INFO_SLOTS {
        let Some(signal) = SignalFlags::from_signum(signum as u32) else {
            continue;
        };
        if !pending.contains(signal) {
            continue;
        }
        let action = process.inner_exclusive_access().signal_actions[signum];
        if action.is_ignore() {
            continue;
        }
        if action.has_user_handler() {
            if crate::arch::signal::can_deliver_user_signal(signum) {
                return true;
            }
            continue;
        }
        if default_signal_error(signum).is_some() {
            return true;
        }
    }
    false
}

pub fn current_has_unmasked_signal() -> bool {
    current_task().is_some_and(|task| {
        let inner = task.inner_exclusive_access();
        !(inner.pending_signals & !inner.signal_mask).is_empty()
    })
}

pub fn current_has_nonrestartable_signal() -> bool {
    current_has_deliverable_signal_matching(|action| action.flags & SA_RESTART == 0)
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
