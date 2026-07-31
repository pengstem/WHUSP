use crate::sync::UPIntrFreeCell;
use crate::task::{TaskControlBlock, block_current_task_no_schedule, schedule, wakeup_task};
use alloc::collections::VecDeque;
use alloc::sync::Arc;

struct PageCacheWaitGate {
    inner: UPIntrFreeCell<PageCacheWaitGateInner>,
}

struct PageCacheWaitGateInner {
    complete: bool,
    waiters: VecDeque<Arc<TaskControlBlock>>,
}

impl PageCacheWaitGate {
    fn new() -> Self {
        Self {
            inner: unsafe {
                UPIntrFreeCell::new(PageCacheWaitGateInner {
                    complete: false,
                    waiters: VecDeque::new(),
                })
            },
        }
    }

    /// Waits for the owner without a completion-before-enqueue lost wake.
    fn wait(&self) {
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
    fn complete(&self) {
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

macro_rules! define_page_cache_gate {
    ($gate:ident) => {
        pub(crate) struct $gate {
            gate: PageCacheWaitGate,
        }

        impl $gate {
            pub(crate) fn new() -> Self {
                Self {
                    gate: PageCacheWaitGate::new(),
                }
            }

            pub(crate) fn wait(&self) {
                self.gate.wait();
            }

            pub(crate) fn complete(&self) {
                self.gate.complete();
            }
        }
    };
}

define_page_cache_gate!(PageCacheLoadGate);
define_page_cache_gate!(PageCacheGenerationGate);
