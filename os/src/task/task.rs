use super::id::{PidHandle, TaskUserRes};
use super::{
    KernelStack, ProcessControlBlock, SIGNAL_INFO_SLOTS, SignalFlags, SignalInfo, TaskContext,
    kstack_alloc,
};
use crate::trap::TrapContext;
use crate::{
    mm::PhysPageNum,
    sync::{UPIntrFreeCell, UPIntrRefMut},
};
use alloc::sync::{Arc, Weak};

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
        let process = self.process.upgrade().unwrap();
        let inner = process.inner_exclusive_access();
        inner.memory_set.token()
    }

    pub fn linux_tid(&self) -> usize {
        let tid = self
            .inner_exclusive_access()
            .linux_tid
            .as_ref()
            .map(|handle| handle.0);
        tid.unwrap_or_else(|| self.process.upgrade().unwrap().getpid())
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
    pub exit_code: Option<i32>,
    pub linux_tid: Option<PidHandle>,
    pub clear_child_tid: Option<usize>,
    pub robust_list_head: usize,
    pub pending_signals: SignalFlags,
    pub signal_infos: [Option<SignalInfo>; SIGNAL_INFO_SLOTS],
    pub signal_mask: SignalFlags,
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

// TODO: why separate it???
impl TaskControlBlock {
    pub fn new(
        process: Arc<ProcessControlBlock>,
        ustack_base: usize,
        alloc_user_res: bool,
    ) -> Self {
        let res = TaskUserRes::new(Arc::clone(&process), ustack_base, alloc_user_res);
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
                    exit_code: None,
                    linux_tid: None,
                    clear_child_tid: None,
                    robust_list_head: 0,
                    pending_signals: SignalFlags::empty(),
                    signal_infos: [None; SIGNAL_INFO_SLOTS],
                    signal_mask: SignalFlags::empty(),
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
