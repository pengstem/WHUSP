mod clone;
mod contest_runner;
mod exec;
mod fd;
pub(crate) mod futex;
mod id;
mod initproc;
mod manager;
mod process;
mod process_lifecycle;
mod processor;
mod ptrace;
mod signal;
mod smp_probe;
#[allow(clippy::module_inception)]
mod task;

use self::id::TaskUserRes;
use crate::arch::__switch;
use crate::fs::{OpenFlags, PathContext, open_file_in, untrack_regular_file_executable};
use crate::shutdown::shutdown;
use crate::sync::UPIntrFreeCell;
use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::{
    close_detached_fd_entry_for_process_teardown, release_record_locks_for_process,
};
use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use lazy_static::*;
use log::info;
use manager::{
    charge_task_after_run, fetch_task, register_task_linux_tid, unregister_task_linux_tid,
};
pub(crate) use process::{
    Credentials, PROCESS_PKEY_COUNT, PathSnapshot, ProcessProcSnapshot, RLimit, RLimitResource,
};
pub use process::{ProcessControlBlock, ProcessCpuTimesSnapshot};
pub(crate) const CAP_IPC_LOCK: usize = process::CapabilitySets::CAP_IPC_LOCK;
pub(crate) const CAP_IPC_OWNER: usize = process::CapabilitySets::CAP_IPC_OWNER;
pub(crate) const CAP_SETPCAP: usize = process::CapabilitySets::CAP_SETPCAP;
pub(crate) const CAP_SYS_CHROOT: usize = process::CapabilitySets::CAP_SYS_CHROOT;
pub(crate) const CAP_SYS_PTRACE: usize = process::CapabilitySets::CAP_SYS_PTRACE;
pub(crate) const CAP_SYS_ADMIN: usize = process::CapabilitySets::CAP_SYS_ADMIN;
pub(crate) const CAP_SYS_RESOURCE: usize = process::CapabilitySets::CAP_SYS_RESOURCE;
pub(crate) const CAP_SYS_TIME: usize = process::CapabilitySets::CAP_SYS_TIME;
pub(crate) const CAP_SYS_TTY_CONFIG: usize = process::CapabilitySets::CAP_SYS_TTY_CONFIG;

pub use crate::arch::TaskContext;
pub use clone::{CloneArgs, CloneFlags, clone_current_thread};
pub(crate) use fd::{FD_LIMIT, FdFlags, FdTableEntry};
pub use id::{IDLE_PID, KernelStack, PidHandle, kstack_alloc, pid_alloc};
pub(crate) use manager::any_process_references_mount;
pub(crate) use manager::list_process_snapshots;
pub(crate) use manager::processes_snapshot;
use manager::remove_ready_tasks_of_process;
pub(crate) use manager::reprioritize_ready_task;
pub(crate) use manager::requeue_task_after_run;
pub(crate) use manager::task_with_linux_tid;
pub use manager::{add_task, pid2process, remove_from_pid2process, wakeup_task};
pub(crate) use manager::{wakeup_front_task, wakeup_timer_task};
pub use processor::{
    current_process, current_task, current_trap_cx, current_user_token, process_of_task,
    refresh_current_user_token, run_tasks, schedule, trap_cx_of_task,
    trap_return_context_after_accounting_for_task, try_current_kstack_bounds,
};
pub(crate) use processor::{current_processor_is_empty, processor_is_idle, processor_slot_ptr};
pub(crate) use ptrace::{
    ptrace_attach_process, ptrace_kill_process, ptrace_note_exec_current, ptrace_resume_process,
    ptrace_stop_task_if_needed, ptrace_syscall_enter_stop_for_task,
    ptrace_syscall_exit_stop_for_task, ptrace_take_wait_status, ptrace_traceme_current,
    ptrace_validate_tracee,
};
pub use signal::{
    CLD_CONTINUED, CLD_STOPPED, DefaultSignalAction, MINSIGSTKSZ, SA_RESTART, SI_TKILL, SIGCHLD,
    SIGCONT, SIGKILL, SIGNAL_INFO_SLOTS, SIGSTOP, SIGTRAP, SS_DISABLE, SS_ONSTACK, SigAltStack,
    SignalAction, SignalFlags, SignalInfo, default_signal_action, default_signal_error,
    default_signal_exit_code, signal_child_status, signal_wait_status,
};
#[cfg(target_arch = "riscv64")]
pub use signal::{SIGRT_1, SIGRTMIN};
pub(crate) use signal::{flags_to_linux_sigset, linux_sigset_to_flags};
pub(crate) use smp_probe::record_cpu_probe_scheduler_wake as record_smp_cpu_probe_scheduler_wake;
pub(crate) use smp_probe::record_yield_syscall as record_smp_probe_yield_syscall;
pub(crate) use smp_probe::start_cpu_probe as start_smp_cpu_probe;
pub(crate) use smp_probe::start_wait_io_probe as start_smp_wait_io_probe;
pub(crate) use task::SCHED_RR_INTERVAL_US;
pub use task::{DEFAULT_TIMER_SLACK_NS, SeccompSockFilter, TaskControlBlock, TaskStatus};

const CORE_DUMP_STATUS_BIT: i32 = 0x80;
const CORE_DUMP_MAX_BYTES: usize = 16 * 1024 * 1024;

fn with_current_task_and_process(
    task_fn: impl FnOnce(&TaskControlBlock),
    process_fn: impl FnOnce(&ProcessControlBlock),
) {
    let Some(task) = current_task() else {
        return;
    };
    task_fn(&task);
    if let Some(process) = task.process.upgrade() {
        process_fn(&process);
    }
}

pub fn account_task_user_time_until(
    task: &TaskControlBlock,
    process: &ProcessControlBlock,
    now_us: usize,
) {
    task.account_user_time_until(now_us);
    process.account_user_time_until(now_us);
}

pub fn account_current_system_time_until(now_us: usize) {
    with_current_task_and_process(
        |task| task.account_system_time_until(now_us),
        |process| process.account_system_time_until(now_us),
    );
}

fn try_account_current_system_time_until(now_us: usize) {
    with_current_task_and_process(
        |task| task.try_account_system_time_until(now_us),
        |process| process.try_account_system_time_until(now_us),
    );
}

pub fn account_current_system_time() {
    account_current_system_time_until(crate::timer::get_time_us());
}

fn try_account_current_system_time() {
    try_account_current_system_time_until(crate::timer::get_time_us());
}

pub fn mark_current_kernel_time_entry(now_us: usize) {
    with_current_task_and_process(
        |task| task.mark_kernel_time_entry(now_us),
        |process| process.mark_kernel_time_entry(now_us),
    );
}

pub fn timer_tick_should_preempt(current: &Arc<TaskControlBlock>) -> bool {
    manager::should_preempt_current_on_tick(current)
}

pub fn suspend_current_and_run_next() {
    // There must be an application running.
    account_current_system_time();
    let (_task, task_cx_ptr) = processor::prepare_current_switch(processor::SwitchReason::Yield);
    schedule(task_cx_ptr);
}

/// Mark the current task blocked and prepare a deferred processor release
/// without scheduling. The caller must enqueue the task on a wait queue and
/// then call `schedule`; the idle context clears `on_cpu` after the switch.
///
/// # Safety (logical)
/// The returned `Arc` is the wait-queue reference. The processor retains its
/// own reference through `__switch`, so the context pointer and kernel stack
/// remain valid until switch completion.
pub fn block_current_task_no_schedule() -> (Arc<TaskControlBlock>, *mut TaskContext) {
    // CONTEXT: `SleepMutex::lock()` can be reached from exit-time destructors
    // while nearby PCB cleanup is in progress. CPU accounting must not turn
    // that cleanup path into a RefCell panic; skipping one sample is preferable
    // to aborting the kernel while the task is about to block.
    try_account_current_system_time();
    processor::prepare_current_switch(processor::SwitchReason::Block)
}

/// Atomically publish Blocked only when no unmasked signal is pending.
///
/// Signal queueing takes the same task lock and wakes a task it observes as
/// Blocked, closing the check-then-sleep window for interruptible wait queues.
pub fn block_current_task_no_schedule_unless_unmasked_signal()
-> Option<(Arc<TaskControlBlock>, *mut TaskContext)> {
    try_account_current_system_time();
    processor::prepare_current_block_unless_unmasked_signal()
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
    let mut exited_linux_tids = Vec::new();
    {
        let mut process_inner = process.inner_exclusive_access();
        for (tid, task_slot) in process_inner.tasks.iter_mut().enumerate() {
            if tid == current_tid {
                continue;
            }
            let Some(task) = task_slot.as_ref().map(Arc::clone) else {
                continue;
            };
            exited_linux_tids.push(task.linux_tid());
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

    for linux_tid in exited_linux_tids {
        unregister_task_linux_tid(linux_tid);
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

fn terminate_sibling_threads_for_exec(
    process: &Arc<ProcessControlBlock>,
    current_tid: usize,
    process_token: usize,
    process_id: usize,
) {
    // Exec teardown must run before the new image is committed. Sibling tasks
    // still need the old token for robust-list and clear-child-tid cleanup.
    terminate_sibling_threads(process, current_tid, process_token, process_id, 0);
    remove_ready_tasks_of_process(process_id);
    futex::remove_process_futex_waiters(process_id);
}

fn rebind_non_leader_for_exec(
    process: &Arc<ProcessControlBlock>,
    current: &Arc<TaskControlBlock>,
    current_tid: usize,
    process_token: usize,
    process_id: usize,
) -> SysResult<()> {
    // A non-leader exec keeps the process PID but moves the caller into slot 0.
    // Do not preserve the caller's old Linux TID handle after rebinding; the
    // post-exec main thread is visible as the process leader.
    let mut clear_child_tids = Vec::new();
    let mut recycle_res = Vec::<TaskUserRes>::new();
    let mut robust_tasks = Vec::new();
    let mut exited_threads = Vec::new();
    let mut released_thread_keyrings = Vec::new();
    let mut exited_linux_tids = Vec::new();
    let mut old_current_linux_tid = None;
    {
        let mut process_inner = process.inner_exclusive_access();
        let current_slot_matches = process_inner
            .tasks
            .get(current_tid)
            .and_then(|slot| slot.as_ref())
            .is_some_and(|task| Arc::ptr_eq(task, current));
        if !current_slot_matches {
            return Err(SysError::ESRCH);
        }
        let leader = process_inner
            .tasks
            .first()
            .and_then(|slot| slot.as_ref())
            .map(Arc::clone)
            .ok_or(SysError::ESRCH)?;
        if leader.inner_exclusive_access().res.is_none() {
            return Err(SysError::ESRCH);
        }

        let mut leader_res = None;
        for (tid, task_slot) in process_inner.tasks.iter_mut().enumerate() {
            let Some(task) = task_slot.as_ref().map(Arc::clone) else {
                continue;
            };
            if Arc::ptr_eq(&task, current) {
                old_current_linux_tid = Some(task.linux_tid());
                *task_slot = None;
                continue;
            }

            exited_linux_tids.push(task.linux_tid());
            let mut task_inner = task.inner_exclusive_access();
            task_inner.task_status = TaskStatus::Exited;
            task_inner.exit_code = Some(0);
            if let Some(clear_child_tid) = task_inner.clear_child_tid.take() {
                clear_child_tids.push(clear_child_tid);
            }
            if let Some(keyring) = task_inner.thread_keyring.take() {
                released_thread_keyrings.push(keyring);
            }
            robust_tasks.push(Arc::clone(&task));
            if tid == 0 {
                leader_res = task_inner.res.take();
            } else if let Some(res) = task_inner.res.take() {
                recycle_res.push(res);
            }
            drop(task_inner);
            exited_threads.push(task);
            *task_slot = None;
        }

        let leader_res = leader_res.ok_or(SysError::ESRCH)?;
        let mut current_inner = current.inner_exclusive_access();
        if let Some(res) = current_inner.res.replace(leader_res) {
            recycle_res.push(res);
        }
        current_inner.tid = 0;
        current_inner.linux_tid = None;
        current_inner.clear_child_tid = None;
        drop(current_inner);

        process_inner.tasks[0] = Some(Arc::clone(current));
        while process_inner.tasks.len() > 1
            && process_inner.tasks.last().is_some_and(Option::is_none)
        {
            process_inner.tasks.pop();
        }
    }

    for linux_tid in exited_linux_tids {
        unregister_task_linux_tid(linux_tid);
    }
    if let Some(linux_tid) = old_current_linux_tid {
        unregister_task_linux_tid(linux_tid);
    }
    register_task_linux_tid(current);
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
    remove_ready_tasks_of_process(process_id);
    futex::remove_process_futex_waiters(process_id);
    Ok(())
}

pub(crate) fn prepare_exec_thread_group(
    process: &Arc<ProcessControlBlock>,
    current: Arc<TaskControlBlock>,
    process_token: usize,
    process_id: usize,
) -> SysResult<Arc<TaskControlBlock>> {
    // Linux execve() is thread-group destructive: sibling threads disappear
    // and a non-leader caller becomes the new leader for the preserved PID.
    // Return the task whose TaskUserRes and TrapContext will be rebuilt.
    let current_tid = current.inner_exclusive_access().tid;
    let thread_count = process.inner_exclusive_access().thread_count();
    if thread_count <= 1 {
        return if current_tid == 0 {
            Ok(current)
        } else {
            Err(SysError::ESRCH)
        };
    }
    if current_tid == 0 {
        terminate_sibling_threads_for_exec(process, current_tid, process_token, process_id);
    } else {
        rebind_non_leader_for_exec(process, &current, current_tid, process_token, process_id)?;
    }
    Ok(current)
}

pub(crate) fn queue_signal_to_task(
    task: Arc<TaskControlBlock>,
    signal: SignalFlags,
    info: SignalInfo,
) {
    if signal.is_empty() {
        return;
    }
    record_job_control_wait_state(&task, signal);
    {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.pending_signals |= signal;
        if let Some(slot) = task_inner.signal_infos.get_mut(info.signo as usize) {
            *slot = Some(info);
        }
    }
    if signal_should_wake_target(&task, signal) {
        wakeup_task(task);
    }
}

fn signal_should_wake_target(task: &Arc<TaskControlBlock>, signal: SignalFlags) -> bool {
    let mut non_job_control = signal;
    non_job_control.remove(SignalFlags::SIGSTOP);
    non_job_control.remove(SignalFlags::SIGCONT);
    if !non_job_control.is_empty() {
        return true;
    }
    let Some(process) = task.process.upgrade() else {
        return false;
    };
    let process_inner = process.inner_exclusive_access();
    // CONTEXT: Full Linux job control would wake a task that is actually
    // stopped by SIGCONT. This kernel only records stop/continue waitid
    // events, so waking a normal futex/pipe sleeper here turns checkpoints
    // into spurious EINTR. A user SIGCONT handler still needs delivery.
    process_inner.signal_actions[SIGCONT as usize].has_user_handler()
}

fn record_job_control_wait_state(task: &Arc<TaskControlBlock>, signal: SignalFlags) {
    let Some(process) = task.process.upgrade() else {
        return;
    };
    let mut changed = false;
    {
        let mut process_inner = process.inner_exclusive_access();
        if signal.contains(SignalFlags::SIGSTOP) {
            // UNFINISHED: This records the Linux-visible waitid stop event but
            // does not yet implement full job-control task suspension.
            process_inner.wait_stop_status = Some(SIGSTOP as i32);
            changed = true;
        }
        if signal.contains(SignalFlags::SIGCONT) {
            // UNFINISHED: This records the waitid continued event; full
            // process-group job control and terminal stop semantics are not modeled.
            process_inner.wait_continued = true;
            changed = true;
        }
    }
    if changed {
        wake_parent_waiters(&process);
    }
}

fn wake_parent_waiters(process: &Arc<ProcessControlBlock>) {
    let Some(parent) = process.parent_process() else {
        return;
    };
    for parent_task in parent.tasks_snapshot() {
        let is_blocked = parent_task.inner_exclusive_access().task_status == TaskStatus::Blocked;
        if is_blocked {
            wakeup_task(parent_task);
        }
    }
}

pub(crate) fn current_process_group_id() -> Option<usize> {
    current_task()
        .and_then(|task| task.process.upgrade())
        .map(|process| process.process_group_id())
}

pub(crate) fn send_tty_signal_to_process_group(pgid: usize, signal: SignalFlags) {
    if signal.is_empty() {
        return;
    }
    let signum = signal.bits().trailing_zeros() as i32;
    let info = SignalInfo::user(signum, 0);
    for process in processes_snapshot()
        .into_iter()
        .filter(|process| process.process_group_id() == pgid)
    {
        queue_signal_to_process_for_tty(&process, signal, info);
    }
}

fn queue_signal_to_process_for_tty(
    process: &Arc<ProcessControlBlock>,
    signal: SignalFlags,
    info: SignalInfo,
) {
    let tasks = process.tasks_snapshot();
    let target = tasks
        .iter()
        .find(|task| {
            let task_inner = task.inner_exclusive_access();
            !(task_inner.signal_mask & signal).contains(signal)
        })
        .cloned()
        .or_else(|| tasks.first().cloned());
    if let Some(task) = target {
        queue_signal_to_task(task, signal, info);
    }
    if signal.check_error().is_some() {
        for task in tasks {
            wakeup_task(task);
        }
    }
}

fn nearest_child_reaper(parent: Option<Arc<ProcessControlBlock>>) -> Arc<ProcessControlBlock> {
    let mut cursor = parent;
    while let Some(process) = cursor {
        let (is_live_subreaper, next_parent) = {
            let inner = process.inner_exclusive_access();
            (
                inner.is_child_subreaper && !inner.is_zombie,
                inner.parent.as_ref().and_then(|parent| parent.upgrade()),
            )
        };
        if is_live_subreaper {
            return process;
        }
        cursor = next_parent;
    }
    INITPROC.clone()
}

fn signal_status_has_core_dump(exit_code: i32) -> bool {
    exit_code < 0 && ((-exit_code) & CORE_DUMP_STATUS_BIT) != 0
}

fn write_core_dump(context: PathContext, path: String, bytes: Vec<u8>) {
    let Ok(file) = open_file_in(
        context,
        path.as_str(),
        OpenFlags::CREATE | OpenFlags::WRONLY | OpenFlags::TRUNC,
    ) else {
        return;
    };
    let _ = file.write_at(0, bytes.as_slice());
}

fn exit_current(exit_code: i32, group_exit: bool) {
    let current = current_task().expect("exit_current requires a current task");
    account_current_system_time();
    let process = current
        .process
        .upgrade()
        .expect("current task process must outlive the task");
    let process_token = process.inner_exclusive_access().get_user_token();
    let process_id = process.getpid();
    let tid = current.inner_exclusive_access().tid;
    let process_exit = group_exit || tid == 0;
    let group_exit_exclusion =
        process_exit.then(|| process.begin_group_exit_exclusion(current.as_ref()));
    let (tid, linux_tid, clear_child_tid, thread_keyring) = {
        let mut task_inner = current.inner_exclusive_access();
        let linux_tid = task_inner
            .linux_tid
            .as_ref()
            .map(|handle| handle.0)
            .unwrap_or(process_id);
        (
            task_inner.tid,
            linux_tid,
            task_inner.clear_child_tid.take(),
            task_inner.thread_keyring.take(),
        )
    };
    unregister_task_linux_tid(linux_tid);
    // Robust-list owner-death and CLONE_CHILD_CLEARTID writes still need the
    // exiting thread's old user address space; complete them before dropping
    // TaskUserRes or removing the task from its process slot.
    futex::exit_robust_list(&current, process_token, process_id);
    if let Some(clear_child_tid) = clear_child_tid {
        futex::clear_child_tid_and_wake(process_token, process_id, clear_child_tid);
    }
    if let Some(keyring) = thread_keyring {
        crate::syscall::keyring::release_keyring_tree(keyring);
    }
    current.inner_exclusive_access().res = None;

    if tid != 0 {
        let mut process_inner = process.inner_exclusive_access();
        if tid < process_inner.tasks.len() {
            process_inner.tasks[tid] = None;
        }
    }
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
        let core_context =
            signal_status_has_core_dump(exit_code).then(|| process.path_snapshot().context);
        let (
            parent,
            children,
            fd_entries,
            retired_areas,
            flushes,
            executable_node,
            exit_signal,
            process_keyring,
            core_dump,
        ) = {
            let mut process_inner = process.inner_exclusive_access();
            // mark this process as a zombie process
            process_inner.is_zombie = true;
            // record exit code of main process
            process_inner.exit_code = exit_code;
            let resident_kb = process_inner.memory_set.resident_bytes() / 1024;
            process_inner.cpu_times.record_resident_kb(resident_kb);
            let parent = process_inner.parent.as_ref().and_then(|p| p.upgrade());
            let exit_signal = process_inner.exit_signal;
            let children = core::mem::take(&mut process_inner.children);
            let core_dump = core_context.map(|context| {
                (
                    context,
                    crate::fs::core_pattern_for_pid(pid),
                    process_inner
                        .memory_set
                        .core_dump_bytes(CORE_DUMP_MAX_BYTES),
                )
            });
            // deallocate other data in user space i.e. program code/data section
            let (flushes, retired_areas) = process_inner.memory_set.recycle_data_pages();
            let executable_node = process_inner.executable_node.take();
            let process_keyring = process_inner.process_keyring.take();
            // Take fd entries out while the current task is still installed.
            // Close cleanup can re-enter VFS and notification paths, so run it
            // after dropping the PCB lock.
            let fd_entries = process_inner.take_all_fd_entries();
            // Keep only the main task in the zombie process for waitpid reaping.
            // Non-main exiting tasks are parked in EXITED_TASKS until their kernel
            // stacks are no longer active across the next schedule boundary.
            while process_inner.tasks.len() > 1 {
                process_inner.tasks.pop();
            }
            (
                parent,
                children,
                fd_entries,
                retired_areas,
                flushes,
                executable_node,
                exit_signal,
                process_keyring,
                core_dump,
            )
        };

        // MapArea owns file-backed ELF/mmap references. Its destructors can
        // enter sleepable VFS locks, so they must run after the PCB spin lock
        // has been released.
        drop(retired_areas);
        if let Some((context, path, bytes)) = core_dump {
            write_core_dump(context, path, bytes);
        }
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
        for entry in fd_entries {
            close_detached_fd_entry_for_process_teardown(entry);
        }
        process.release_vfork_parent();

        // Move orphaned children under the nearest live subreaper, or init.
        let reaper = nearest_child_reaper(parent.clone());
        let mut reaper_inner = reaper.inner_exclusive_access();
        for child in children {
            child.inner_exclusive_access().parent = Some(Arc::downgrade(&reaper));
            reaper_inner.children.push(child);
        }
        drop(reaper_inner);

        if let Some(parent) = parent {
            let parent_tasks = parent.tasks_snapshot();
            let sigchld_ignored = exit_signal == SIGCHLD
                && parent.inner_exclusive_access().signal_actions[SIGCHLD as usize].is_ignore();
            if sigchld_ignored {
                parent
                    .inner_exclusive_access()
                    .children
                    .retain(|child| child.getpid() != pid);
                remove_from_pid2process(pid);
            } else {
                if let Some(parent_task) = parent_tasks.first()
                    && let Some(signal) = SignalFlags::from_signum(exit_signal)
                    && !signal.is_empty()
                {
                    queue_signal_to_task(
                        Arc::clone(parent_task),
                        signal,
                        SignalInfo::child_exit(exit_signal as i32, pid as i32, exit_code),
                    );
                }
                // Signal delivery and wait wakeups are separate contracts. The
                // exit signal targets the parent leader, while every blocked parent
                // task may be sleeping in wait4()/waitid() and needs a wake hint.
                for parent_task in parent_tasks {
                    let is_blocked =
                        parent_task.inner_exclusive_access().task_status == TaskStatus::Blocked;
                    if is_blocked {
                        wakeup_task(parent_task);
                    }
                }
            }
        }
    }
    drop(group_exit_exclusion);
    let task = current_task().expect("exit_current requires the current task to be scheduled");
    let mut task_inner = task.inner_exclusive_access();
    task_inner.exit_code = Some(exit_code);
    drop(task_inner);
    let (switch_task, _task_cx_ptr) =
        processor::prepare_current_switch(processor::SwitchReason::Exit);
    // Processor::current now keeps the exiting task and its kernel stack alive
    // through __switch; switch completion transfers that Arc to EXITED_TASKS.
    drop(switch_task);
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
        ProcessControlBlock::new_with_args(
            init.data.as_slice(),
            init.path,
            init.executable_node,
            init.argv,
            init.envp,
        )
    };
}

pub fn add_initproc() {
    // Build the sharded futex table on the boot CPU before secondary
    // schedulers can race through its lazy initializer on their first wait.
    futex::init();
    let _initproc = INITPROC.clone();
}

pub fn check_signals_of_task(
    task: &Arc<TaskControlBlock>,
    process: &Arc<ProcessControlBlock>,
) -> Option<(i32, &'static str)> {
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
    let (action, core_limit) = {
        let process_inner = process.inner_exclusive_access();
        (
            process_inner.signal_actions[signum],
            process_inner
                .resource_limits
                .get(RLimitResource::Core)
                .rlim_cur,
        )
    };
    // CONTEXT: Linux's default disposition for SIGCHLD is ignore. PID 1 is
    // also protected from ordinary default-disposition signals unless it has
    // installed a user handler; LTP heartbeat children can otherwise kill the
    // kernel-owned init shell with a stray SIGUSR1.
    if action.is_ignore()
        || (default_signal_action(signum) == Some(DefaultSignalAction::Ignore)
            && !action.has_user_handler())
        || (Arc::ptr_eq(process, &INITPROC) && !action.has_user_handler())
    {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.clear_pending(signum as u32);
        return None;
    }
    if action.has_user_handler() {
        return None;
    }
    let (exit_code, message) = default_signal_error(signum)?;
    Some((
        default_signal_exit_code(signum, core_limit).unwrap_or(exit_code),
        message,
    ))
}

pub fn check_signals_of_current() -> Option<(i32, &'static str)> {
    let task = current_task()?;
    let process = task.process.upgrade()?;
    check_signals_of_task(&task, &process)
}

fn task_has_deliverable_signal_matching(
    task: &Arc<TaskControlBlock>,
    process: &Arc<ProcessControlBlock>,
    predicate: impl Fn(SignalAction) -> bool,
) -> bool {
    let pending = {
        let task_inner = task.inner_exclusive_access();
        SignalFlags::from_bits_retain(
            task_inner.pending_signals.bits() & !task_inner.signal_mask.bits(),
        )
    };
    if pending.is_empty() {
        return false;
    }
    crate::perf::record_signal_action_table_lock_call();
    let process_inner = process.inner_exclusive_access();
    for signum in 1..SIGNAL_INFO_SLOTS {
        let Some(signal) = SignalFlags::from_signum(signum as u32) else {
            continue;
        };
        if !pending.contains(signal) {
            continue;
        }
        let action = process_inner.signal_actions[signum];
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

fn current_has_deliverable_signal_matching(predicate: impl Fn(SignalAction) -> bool) -> bool {
    let Some(task) = current_task() else {
        return false;
    };
    let Some(process) = task.process.upgrade() else {
        return false;
    };
    task_has_deliverable_signal_matching(&task, &process, predicate)
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
    task_has_interrupting_signal_matching(&task, &process, |_, _| true)
}

fn task_has_interrupting_signal_matching(
    task: &Arc<TaskControlBlock>,
    process: &Arc<ProcessControlBlock>,
    user_handler_interrupts: impl Fn(SignalFlags, SignalAction) -> bool,
) -> bool {
    let pending = {
        let inner = task.inner_exclusive_access();
        SignalFlags::from_bits_retain(inner.pending_signals.bits() & !inner.signal_mask.bits())
    };
    if pending.is_empty() {
        return false;
    }
    crate::perf::record_signal_action_table_lock_call();
    let process_inner = process.inner_exclusive_access();
    for signum in 1..SIGNAL_INFO_SLOTS {
        let Some(signal) = SignalFlags::from_signum(signum as u32) else {
            continue;
        };
        if !pending.contains(signal) {
            continue;
        }
        let action = process_inner.signal_actions[signum];
        if action.is_ignore() {
            continue;
        }
        if action.has_user_handler() {
            if crate::arch::signal::can_deliver_user_signal(signum)
                && user_handler_interrupts(signal, action)
            {
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

pub(crate) fn task_has_wait_interrupt_signal(
    task: &Arc<TaskControlBlock>,
    process: &Arc<ProcessControlBlock>,
) -> bool {
    let interrupted = task_has_interrupting_signal_matching(task, process, |signal, action| {
        // CONTEXT: LTP uses SIGUSR1 as a musl signal(2) heartbeat, which sets
        // SA_RESTART. Let wait* keep sleeping for that compatibility signal,
        // but return to trap delivery for timeout handlers and fatal defaults.
        !(signal == SignalFlags::SIGUSR1 && action.flags & SA_RESTART != 0)
    });
    if interrupted {
        clear_restartable_wait_heartbeat(task, process);
    }
    interrupted
}

fn clear_restartable_wait_heartbeat(
    task: &Arc<TaskControlBlock>,
    process: &Arc<ProcessControlBlock>,
) {
    let signum = SignalFlags::SIGUSR1.bits().trailing_zeros() as usize;
    let action = process.inner_exclusive_access().signal_actions[signum];
    if !action.has_user_handler() || action.flags & SA_RESTART == 0 {
        return;
    }
    let mut task_inner = task.inner_exclusive_access();
    let unmasked = SignalFlags::from_bits_retain(
        task_inner.pending_signals.bits() & !task_inner.signal_mask.bits(),
    );
    if unmasked.contains(SignalFlags::SIGUSR1) {
        task_inner.clear_pending(signum as u32);
    }
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
