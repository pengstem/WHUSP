use crate::sync::UPIntrFreeCell;
use crate::task::{TaskControlBlock, block_current_task_no_schedule, schedule, wakeup_task};
use alloc::collections::VecDeque;
use alloc::sync::Arc;

pub(crate) struct PageCacheLoadGate {
    inner: UPIntrFreeCell<PageCacheLoadGateInner>,
}

struct PageCacheLoadGateInner {
    complete: bool,
    waiters: VecDeque<Arc<TaskControlBlock>>,
}

impl PageCacheLoadGate {
    pub(crate) fn new() -> Self {
        Self {
            inner: unsafe {
                UPIntrFreeCell::new(PageCacheLoadGateInner {
                    complete: false,
                    waiters: VecDeque::new(),
                })
            },
        }
    }

    /// Waits for the owner without a completion-before-enqueue lost wake.
    pub(crate) fn wait(&self) {
        let mut inner = self.inner.exclusive_access();
        if inner.complete {
            return;
        }
        let (task, task_cx_ptr) = block_current_task_no_schedule();
        inner.waiters.push_back(task);
        drop(inner);
        schedule(task_cx_ptr);
    }

    /// Publishes completion and wakes every task that joined this load.
    pub(crate) fn complete(&self) {
        let waiters = self.inner.exclusive_session(|inner| {
            if inner.complete {
                return VecDeque::new();
            }
            inner.complete = true;
            core::mem::take(&mut inner.waiters)
        });
        for task in waiters {
            wakeup_task(task);
        }
    }
}
