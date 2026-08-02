use super::id::{PidHandle, TaskUserRes};
use super::{
    KernelStack, ProcessControlBlock, SIGNAL_INFO_SLOTS, SigAltStack, SignalFlags, SignalInfo,
    TaskContext, kstack_alloc,
};
use crate::trap::TrapContext;
use crate::{
    mm::PhysPageNum,
    sync::{SpinNoIrqLock, SpinNoIrqLockGuard},
};
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

pub const DEFAULT_TIMER_SLACK_NS: usize = 50_000;
pub(crate) const SCHED_RR_INTERVAL_US: usize = 100_000;
const SCHED_FIFO: i32 = 1;
const SCHED_RR: i32 = 2;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskStatus {
    Ready,
    Running,
    Blocked,
    Exited,
}

#[derive(Clone, Copy, Debug, Default)]
struct TaskCpuTimes {
    user_us: usize,
    system_us: usize,
    last_user_enter_us: Option<usize>,
    last_kernel_enter_us: Option<usize>,
}

pub struct TaskControlBlock {
    // immutable
    pub process: Weak<ProcessControlBlock>,
    pub kstack: KernelStack,
    // Scheduler-owned hot data that must remain reachable without taking the
    // broad task inner lock.
    sched: TaskSched,
    // Linux keeps a TIF_SIGPENDING summary in thread flags so the normal trap
    // return path does not lock signal queues. This conservative bit mirrors
    // whether any task-local signal is queued; masked signals may cause a slow
    // check, but false negatives are forbidden.
    signal_pending: AtomicBool,
    // mutable
    pub inner: SpinNoIrqLock<TaskControlBlockInner>,
}

#[repr(C, align(64))]
struct TaskSched {
    remote_wake: TaskWakeNode,
}

struct TaskWakeNode {
    next: AtomicPtr<TaskControlBlock>,
    front: AtomicBool,
    linked: AtomicBool,
}

impl TaskSched {
    const fn new() -> Self {
        Self {
            remote_wake: TaskWakeNode {
                next: AtomicPtr::new(ptr::null_mut()),
                front: AtomicBool::new(false),
                linked: AtomicBool::new(false),
            },
        }
    }
}

pub struct TaskControlBlockInner {
    pub res: Option<TaskUserRes>,
    pub tid: usize,
    pub trap_cx_ppn: PhysPageNum,
    pub task_cx: TaskContext,
    pub task_status: TaskStatus,
    // Scheduler ownership is explicit so a task cannot be both running and
    // reachable from any per-CPU run queue.
    pub(crate) on_cpu: Option<crate::cpu::CpuId>,
    pub(crate) on_rq: bool,
    // Exact per-CPU run-queue ownership. It changes only while the source or
    // destination queue lock serializes removal/publication.
    pub(crate) queued_cpu: Option<crate::cpu::CpuId>,
    // Most recent CPU that successfully claimed this task. Sleeping wakeup
    // placement uses it only after intersecting affinity with the online mask.
    pub(crate) last_cpu: Option<crate::cpu::CpuId>,
    // A wakeup can race after a task publishes Blocked but before its CPU has
    // crossed the task-to-idle context switch. The old CPU owns the only legal
    // enqueue at that boundary, so remember the wakeup until switch completion.
    pub(crate) wake_pending: bool,
    pub(crate) wake_front: bool,
    /// Orthogonal job-control gate. The base status continues to describe
    /// whether the task was runnable or waiting when the process stopped.
    pub(crate) job_control_stopped: bool,
    /// This running task still owes the process-wide group-stop generation an
    /// acknowledgement at switch completion.
    pub(crate) job_control_stop_ack_generation: usize,
    // Linux-visible affinity mask used for placement, stealing, and migration.
    pub(crate) allowed_cpus: crate::cpu::CpuMask,
    // Linux-visible sleep state for cooperative wait loops that stay runnable.
    pub proc_sleeping: bool,
    pub exit_code: Option<i32>,
    // Main tasks derive their Linux TID from the process PID. Pthreads own a
    // separate PidHandle so futex, tgkill, and robust-list paths never expose
    // the internal task-slot index as a TID.
    pub linux_tid: Option<PidHandle>,
    // Linux clear_child_tid user address from set_tid_address()/clone().
    // Exit cleanup writes 0 through this task's address space and wakes one
    // futex waiter, so this must track the Linux-visible thread lifecycle.
    pub clear_child_tid: Option<usize>,
    // Per-thread robust-list head from set_robust_list(); robust futex cleanup
    // must pair this pointer with linux_tid(), not the internal task slot.
    pub robust_list_head: usize,
    pub pending_signals: SignalFlags,
    pub signal_infos: Vec<Option<SignalInfo>>,
    pub signal_mask: SignalFlags,
    pub sigsuspend_restore_mask: Option<SignalFlags>,
    pub sigaltstack: SigAltStack,
    pub sched_policy: i32,
    pub sched_priority: i32,
    pub sched_reset_on_fork: bool,
    pub sched_deadline_runtime: u64,
    pub sched_deadline_deadline: u64,
    pub sched_deadline_period: u64,
    pub nice: i8,
    pub sched_vruntime: u64,
    sched_run_start_us: Option<usize>,
    cpu_times: TaskCpuTimes,
    pub timer_slack_ns: usize,
    pub default_timer_slack_ns: usize,
}

impl TaskControlBlock {
    #[inline(always)]
    pub(crate) fn has_pending_signal_fast(&self) -> bool {
        self.signal_pending.load(Ordering::Acquire)
    }

    /// Publishes the exact queue summary while the caller still owns
    /// `self.inner`; signal enqueue and consume sites use that same lock, so a
    /// clear cannot overwrite a later enqueue with `false`.
    pub(crate) fn publish_signal_pending_locked(&self, pending: bool) {
        self.signal_pending.store(pending, Ordering::Release);
    }

    pub fn new(
        process: Arc<ProcessControlBlock>,
        ustack_base: usize,
        alloc_user_res: bool,
    ) -> Self {
        let res = TaskUserRes::new(Arc::clone(&process), ustack_base, alloc_user_res);
        Self::from_user_res(process, res)
    }

    pub fn new_with_supplied_stack(
        process: Arc<ProcessControlBlock>,
        ustack_base: usize,
        alloc_user_res: bool,
    ) -> Self {
        let res =
            TaskUserRes::new_with_supplied_stack(Arc::clone(&process), ustack_base, alloc_user_res);
        Self::from_user_res(process, res)
    }

    fn from_user_res(process: Arc<ProcessControlBlock>, res: TaskUserRes) -> Self {
        let tid = res.tid;
        let trap_cx_ppn = res.trap_cx_ppn();
        let kstack = kstack_alloc();
        let kstack_top = kstack.get_top();
        let job_control_stopped = process.is_job_control_stopped();
        Self {
            process: Arc::downgrade(&process),
            kstack,
            sched: TaskSched::new(),
            signal_pending: AtomicBool::new(false),
            inner: SpinNoIrqLock::new(TaskControlBlockInner {
                res: Some(res),
                tid,
                trap_cx_ppn,
                task_cx: TaskContext::goto_trap_return(kstack_top),
                task_status: TaskStatus::Ready,
                on_cpu: None,
                on_rq: false,
                queued_cpu: None,
                last_cpu: None,
                wake_pending: false,
                wake_front: false,
                job_control_stopped,
                job_control_stop_ack_generation: 0,
                allowed_cpus: crate::cpu::topology().possible_mask(),
                proc_sleeping: false,
                exit_code: None,
                linux_tid: None,
                clear_child_tid: None,
                robust_list_head: 0,
                pending_signals: SignalFlags::empty(),
                signal_infos: (0..SIGNAL_INFO_SLOTS).map(|_| None).collect(),
                signal_mask: SignalFlags::empty(),
                sigsuspend_restore_mask: None,
                sigaltstack: SigAltStack::disabled(),
                sched_policy: 0,
                sched_priority: 0,
                sched_reset_on_fork: false,
                sched_deadline_runtime: 0,
                sched_deadline_deadline: 0,
                sched_deadline_period: 0,
                nice: 0,
                sched_vruntime: 0,
                sched_run_start_us: None,
                cpu_times: TaskCpuTimes::default(),
                timer_slack_ns: DEFAULT_TIMER_SLACK_NS,
                default_timer_slack_ns: DEFAULT_TIMER_SLACK_NS,
            }),
        }
    }

    pub fn inner_exclusive_access(&self) -> SpinNoIrqLockGuard<'_, TaskControlBlockInner> {
        self.inner.lock()
    }

    /// Claims this task's embedded remote-wake node before list publication.
    ///
    /// The task state machine permits at most one remote-list reference for a
    /// task. Keep the atomic membership bit as a fail-stop guard so a future
    /// regression cannot overwrite `next` and lose an Arc raw reference.
    pub(crate) fn claim_remote_wake_node(&self, front: bool) {
        assert!(
            self.sched
                .remote_wake
                .linked
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "task already owns a linked remote-wake node"
        );
        self.sched.remote_wake.front.store(front, Ordering::Relaxed);
        self.sched
            .remote_wake
            .next
            .store(ptr::null_mut(), Ordering::Relaxed);
    }

    /// Updates the link while the node is producer-private or belongs to the
    /// target CPU's detached list.
    pub(crate) fn set_remote_wake_next(&self, next: *mut TaskControlBlock) {
        assert!(
            self.sched.remote_wake.linked.load(Ordering::Relaxed),
            "unlinked task cannot carry a remote-wake next pointer"
        );
        self.sched.remote_wake.next.store(next, Ordering::Relaxed);
    }

    pub(crate) fn remote_wake_next(&self) -> *mut TaskControlBlock {
        assert!(
            self.sched.remote_wake.linked.load(Ordering::Relaxed),
            "unlinked task cannot be traversed as a remote-wake node"
        );
        self.sched.remote_wake.next.load(Ordering::Relaxed)
    }

    /// Returns the detached node payload and makes the embedded node reusable.
    /// The caller must own the target CPU's detached list and reconstruct the
    /// list's Arc strong reference exactly once after this call.
    pub(crate) fn release_remote_wake_node(&self) -> (*mut TaskControlBlock, bool) {
        assert!(
            self.sched.remote_wake.linked.load(Ordering::Relaxed),
            "remote-wake consumer observed an unlinked task"
        );
        let next = self
            .sched
            .remote_wake
            .next
            .swap(ptr::null_mut(), Ordering::Relaxed);
        let front = self.sched.remote_wake.front.swap(false, Ordering::Relaxed);
        self.sched
            .remote_wake
            .linked
            .store(false, Ordering::Release);
        (next, front)
    }

    pub fn get_user_token(&self) -> usize {
        let process = self
            .process
            .upgrade()
            .expect("task process must outlive the task while it is runnable");
        let inner = process.inner_exclusive_access();
        inner.memory_set.token()
    }

    /// Returns the Linux-visible TID, not the internal task-table slot.
    pub fn linux_tid(&self) -> usize {
        let tid = self
            .inner_exclusive_access()
            .linux_tid
            .as_ref()
            .map(|handle| handle.0);
        tid.unwrap_or_else(|| {
            self.process
                .upgrade()
                .expect("main task process must exist while deriving Linux tid")
                .getpid()
        })
    }

    pub fn robust_list_head(&self) -> usize {
        self.inner_exclusive_access().robust_list_head
    }

    pub fn set_robust_list_head(&self, head: usize) {
        self.inner_exclusive_access().robust_list_head = head;
    }

    pub(crate) fn realtime_priority(&self) -> i32 {
        let inner = self.inner_exclusive_access();
        match inner.sched_policy {
            SCHED_FIFO | SCHED_RR if inner.sched_priority > 0 => inner.sched_priority,
            _ => 0,
        }
    }

    pub(crate) fn is_realtime_round_robin(&self) -> bool {
        let inner = self.inner_exclusive_access();
        inner.sched_policy == SCHED_RR && inner.sched_priority > 0
    }

    pub(crate) fn nice_value(&self) -> i8 {
        self.inner_exclusive_access().nice
    }

    pub(crate) fn floor_sched_vruntime(&self, min_vruntime: u64) -> u64 {
        let mut inner = self.inner_exclusive_access();
        if inner.sched_vruntime < min_vruntime {
            inner.sched_vruntime = min_vruntime;
        }
        inner.sched_vruntime
    }

    pub(crate) fn add_sched_vruntime(&self, delta: u64) -> u64 {
        let mut inner = self.inner_exclusive_access();
        inner.sched_vruntime = inner.sched_vruntime.saturating_add(delta);
        inner.sched_vruntime
    }

    pub(crate) fn migrate_sched_vruntime(&self, source_min: u64, target_min: u64) {
        let mut inner = self.inner_exclusive_access();
        let relative = inner.sched_vruntime.saturating_sub(source_min);
        inner.sched_vruntime = target_min.saturating_add(relative);
    }

    pub(crate) fn mark_sched_run_start(&self, now_us: usize) {
        self.inner_exclusive_access().sched_run_start_us = Some(now_us);
    }

    pub(crate) fn take_sched_runtime_us(&self, now_us: usize) -> usize {
        self.inner_exclusive_access()
            .sched_run_start_us
            .take()
            .map_or(0, |start_us| now_us.saturating_sub(start_us))
    }

    pub(crate) fn sched_runtime_us(&self, now_us: usize) -> usize {
        self.inner_exclusive_access()
            .sched_run_start_us
            .map_or(0, |start_us| now_us.saturating_sub(start_us))
    }

    pub fn mark_user_time_entry(&self, now_us: usize) {
        self.inner_exclusive_access()
            .cpu_times
            .mark_user_entry(now_us);
    }

    pub fn mark_kernel_time_entry(&self, now_us: usize) {
        self.inner_exclusive_access()
            .cpu_times
            .mark_kernel_entry(now_us);
    }

    pub fn account_user_time_until(&self, now_us: usize) {
        self.inner_exclusive_access()
            .cpu_times
            .account_user_until(now_us);
    }

    pub fn account_system_time_until(&self, now_us: usize) {
        self.inner_exclusive_access()
            .cpu_times
            .account_system_until(now_us);
    }

    pub fn try_account_system_time_until(&self, now_us: usize) {
        if let Some(mut inner) = self.inner.try_lock() {
            inner.cpu_times.account_system_until(now_us);
        }
    }

    pub fn cpu_time_us(&self) -> usize {
        self.inner_exclusive_access().cpu_times.total_us()
    }

    pub(crate) fn cpu_times_snapshot(&self) -> (usize, usize) {
        let inner = self.inner_exclusive_access();
        (inner.cpu_times.user_us, inner.cpu_times.system_us)
    }

    pub(crate) fn take_cpu_times_snapshot(&self) -> (usize, usize) {
        let mut inner = self.inner_exclusive_access();
        let times = core::mem::take(&mut inner.cpu_times);
        (times.user_us, times.system_us)
    }
}

impl TaskControlBlockInner {
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        self.trap_cx_ppn.get_mut()
    }

    pub fn clear_pending(&mut self, signum: u32) {
        if let Some(flag) = SignalFlags::from_signum(signum) {
            self.pending_signals.remove(flag);
        }
        if let Some(slot) = self.signal_infos.get_mut(signum as usize) {
            *slot = None;
        }
    }
}

impl TaskCpuTimes {
    fn mark_user_entry(&mut self, now_us: usize) {
        self.last_user_enter_us = Some(now_us);
        self.last_kernel_enter_us = None;
    }

    fn mark_kernel_entry(&mut self, now_us: usize) {
        self.last_kernel_enter_us = Some(now_us);
        self.last_user_enter_us = None;
    }

    fn account_user_until(&mut self, now_us: usize) {
        if let Some(start_us) = self.last_user_enter_us.take() {
            self.user_us = self.user_us.saturating_add(now_us.saturating_sub(start_us));
        }
        self.last_kernel_enter_us = Some(now_us);
    }

    fn account_system_until(&mut self, now_us: usize) {
        if let Some(start_us) = self.last_kernel_enter_us.take() {
            self.system_us = self
                .system_us
                .saturating_add(now_us.saturating_sub(start_us));
        }
        self.last_kernel_enter_us = Some(now_us);
    }

    fn total_us(&self) -> usize {
        self.user_us.saturating_add(self.system_us)
    }
}
