use super::id::{PidHandle, TaskUserRes};
use super::{
    KernelStack, ProcessControlBlock, SIGNAL_INFO_SLOTS, SigAltStack, SignalFlags, SignalInfo,
    TaskContext, kstack_alloc,
};
use crate::trap::TrapContext;
use crate::{
    mm::PhysPageNum,
    sync::{UPIntrFreeCell, UPIntrRefMut},
};
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

pub const DEFAULT_TIMER_SLACK_NS: usize = 50_000;
pub(crate) const SCHED_RR_INTERVAL_US: usize = 100_000;
const SCHED_FIFO: i32 = 1;
const SCHED_RR: i32 = 2;

#[derive(Copy, Clone, PartialEq)]
pub enum TaskStatus {
    Ready,
    Running,
    Blocked,
    Exited,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TaskCpuTimes {
    user_us: usize,
    system_us: usize,
    last_user_enter_us: Option<usize>,
    last_kernel_enter_us: Option<usize>,
}

#[derive(Clone, Copy)]
pub struct SeccompSockFilter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

pub struct TaskControlBlock {
    // immutable
    pub process: Weak<ProcessControlBlock>,
    pub kstack: KernelStack,
    // mutable
    pub inner: UPIntrFreeCell<TaskControlBlockInner>,
}

pub struct TaskControlBlockInner {
    pub res: Option<TaskUserRes>,
    pub tid: usize,
    pub trap_cx_ppn: PhysPageNum,
    pub task_cx: TaskContext,
    pub task_status: TaskStatus,
    // Linux-visible sleep state for cooperative wait loops that stay runnable.
    pub proc_sleeping: bool,
    pub exit_code: Option<i32>,
    // Main tasks derive their Linux TID from the process PID. Pthreads and
    // CLONE_VM helper tasks own a separate PidHandle so futex, tgkill, and
    // robust-list paths never expose the internal task-slot index as a TID.
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
    pub sched_run_start_us: Option<usize>,
    pub cpu_times: TaskCpuTimes,
    pub timer_slack_ns: usize,
    pub default_timer_slack_ns: usize,
    pub seccomp_mode: u8,
    pub seccomp_filter: Option<Vec<SeccompSockFilter>>,
    // Same-PCB helper used for CLONE_VM process compatibility paths; it should
    // exit like a child helper, not terminate the whole parent thread group.
    pub clone_vm_process_helper: bool,
    // Exposes a synthetic new-net namespace view for CLONE_VM helper tasks.
    // The helper shares the parent PCB, so namespace-visible state that must
    // differ for LTP probes belongs on the task, not the process.
    pub synthetic_newnet: bool,
    pub(crate) thread_keyring: Option<i32>,
}

impl TaskControlBlock {
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
        Self {
            process: Arc::downgrade(&process),
            kstack,
            inner: unsafe {
                UPIntrFreeCell::new(TaskControlBlockInner {
                    res: Some(res),
                    tid,
                    trap_cx_ppn,
                    task_cx: TaskContext::goto_trap_return(kstack_top),
                    task_status: TaskStatus::Ready,
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
                    seccomp_mode: 0,
                    seccomp_filter: None,
                    clone_vm_process_helper: false,
                    synthetic_newnet: false,
                    thread_keyring: None,
                })
            },
        }
    }

    pub fn inner_exclusive_access(&self) -> UPIntrRefMut<'_, TaskControlBlockInner> {
        self.inner.exclusive_access()
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

    pub(crate) fn mark_sched_run_start(&self, now_us: usize) {
        self.inner_exclusive_access().sched_run_start_us = Some(now_us);
    }

    pub(crate) fn take_sched_runtime_us(&self, now_us: usize) -> usize {
        self.inner_exclusive_access()
            .sched_run_start_us
            .take()
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
        if let Some(mut inner) = self.inner.try_exclusive_access() {
            inner.cpu_times.account_system_until(now_us);
        }
    }

    pub fn cpu_time_us(&self) -> usize {
        self.inner_exclusive_access().cpu_times.total_us()
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
    pub fn mark_user_entry(&mut self, now_us: usize) {
        self.last_user_enter_us = Some(now_us);
        self.last_kernel_enter_us = None;
    }

    pub fn mark_kernel_entry(&mut self, now_us: usize) {
        self.last_kernel_enter_us = Some(now_us);
        self.last_user_enter_us = None;
    }

    pub fn account_user_until(&mut self, now_us: usize) {
        if let Some(start_us) = self.last_user_enter_us.take() {
            self.user_us = self.user_us.saturating_add(now_us.saturating_sub(start_us));
        }
        self.last_kernel_enter_us = Some(now_us);
    }

    pub fn account_system_until(&mut self, now_us: usize) {
        if let Some(start_us) = self.last_kernel_enter_us.take() {
            self.system_us = self
                .system_us
                .saturating_add(now_us.saturating_sub(start_us));
        }
        self.last_kernel_enter_us = Some(now_us);
    }

    pub fn total_us(&self) -> usize {
        self.user_us.saturating_add(self.system_us)
    }
}
