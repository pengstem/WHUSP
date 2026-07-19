use crate::sync::UPIntrFreeCell;
use crate::task::{TaskContext, TaskControlBlock, block_current_task_no_schedule, wakeup_task};
use alloc::{collections::VecDeque, sync::Arc};

pub struct Condvar {
    pub inner: UPIntrFreeCell<CondvarInner>,
}

pub struct CondvarInner {
    pub wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl Condvar {
    pub fn new() -> Self {
        Self {
            inner: unsafe {
                UPIntrFreeCell::new(CondvarInner {
                    wait_queue: VecDeque::new(),
                })
            },
        }
    }

    pub fn signal(&self) -> bool {
        let task = self.inner.exclusive_access().wait_queue.pop_front();
        if let Some(task) = task {
            wakeup_task(task);
            true
        } else {
            false
        }
    }

    pub fn wait_no_sched(&self) -> *mut TaskContext {
        // Serialize the Blocked publication and queue insertion against
        // signal(). Otherwise a completion can observe an empty queue after
        // the task has committed to sleeping but before it becomes wakeable.
        let mut inner = self.inner.exclusive_access();
        let (task, task_cx_ptr) = block_current_task_no_schedule();
        inner.wait_queue.push_back(task);
        task_cx_ptr
    }
}
