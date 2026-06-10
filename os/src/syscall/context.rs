use crate::task::{ProcessControlBlock, TaskControlBlock};
use alloc::sync::Arc;

pub(crate) struct SyscallContext {
    #[allow(dead_code)]
    task: Arc<TaskControlBlock>,
    process: Arc<ProcessControlBlock>,
    user_token: usize,
}

impl SyscallContext {
    pub(crate) fn new(task: Arc<TaskControlBlock>, process: Arc<ProcessControlBlock>) -> Self {
        let user_token = process.inner_exclusive_access().memory_set.token();
        Self {
            task,
            process,
            user_token,
        }
    }

    #[allow(dead_code)]
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
