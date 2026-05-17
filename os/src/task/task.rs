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

impl TaskControlBlock {
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
    pub linux_tid: Option<PidHandle>,
    pub clear_child_tid: Option<usize>,
    pub robust_list_head: usize,
    pub pending_signals: SignalFlags,
    pub signal_infos: [Option<SignalInfo>; SIGNAL_INFO_SLOTS],
    pub signal_mask: SignalFlags,
    pub sigsuspend_restore_mask: Option<SignalFlags>,
    pub sigaltstack: SigAltStack,
    pub sched_policy: i32,
    pub sched_priority: i32,
    pub sched_reset_on_fork: bool,
    pub timer_slack_ns: usize,
    pub default_timer_slack_ns: usize,
    pub seccomp_mode: u8,
    pub seccomp_filter: Option<Vec<SeccompSockFilter>>,
    pub clone_vm_process_helper: bool,
    pub synthetic_newnet: bool,
    pub(crate) thread_keyring: Option<i32>,
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

    #[allow(unused)]
    fn get_status(&self) -> TaskStatus {
        self.task_status
    }
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
                    signal_infos: [None; SIGNAL_INFO_SLOTS],
                    signal_mask: SignalFlags::empty(),
                    sigsuspend_restore_mask: None,
                    sigaltstack: SigAltStack::disabled(),
                    sched_policy: 0,
                    sched_priority: 0,
                    sched_reset_on_fork: false,
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
}

#[derive(Copy, Clone, PartialEq)]
pub enum TaskStatus {
    Ready,
    Running,
    Blocked,
    Exited,
}
