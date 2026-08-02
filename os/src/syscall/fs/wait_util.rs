use crate::task::{TaskControlBlock, current_task};
use crate::uapi::errno::{Errno, KResult};
use alloc::sync::Arc;

pub(super) struct ProcSleepGuard {
    task: Arc<TaskControlBlock>,
}

impl ProcSleepGuard {
    pub(super) fn new() -> KResult<Self> {
        let task = current_task().ok_or(Errno::ESRCH)?;
        task.inner_exclusive_access().proc_sleeping = true;
        Ok(Self { task })
    }
}

impl Drop for ProcSleepGuard {
    fn drop(&mut self) {
        self.task.inner_exclusive_access().proc_sleeping = false;
    }
}
