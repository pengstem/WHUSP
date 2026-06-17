use crate::task::{ProcessControlBlock, TaskControlBlock};
use alloc::sync::Arc;

/// Per-syscall snapshot of the current task, process, and user address space.
///
/// User-copy helpers should prefer this context when a syscall can sleep or
/// mutate process state before touching user memory again.
pub(crate) struct SyscallContext {
    task: Arc<TaskControlBlock>,
    process: Arc<ProcessControlBlock>,
    user_token: usize,
}

impl SyscallContext {
    pub(crate) fn new(task: Arc<TaskControlBlock>, process: Arc<ProcessControlBlock>) -> Self {
        // Snapshot the caller token at syscall entry; user-copy helpers using
        // this context must not re-read a process token after an exec-style
        // image switch changes the PCB memory_set.
        let user_token = process.inner_exclusive_access().memory_set.token();
        Self {
            task,
            process,
            user_token,
        }
    }

    pub(crate) fn task(&self) -> &Arc<TaskControlBlock> {
        &self.task
    }

    pub(crate) fn process(&self) -> &Arc<ProcessControlBlock> {
        &self.process
    }

    pub(crate) fn user_token(&self) -> usize {
        self.user_token
    }
}
