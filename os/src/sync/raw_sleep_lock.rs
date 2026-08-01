use super::SpinNoIrqLock;
use crate::task::{
    TaskContext, TaskControlBlock, block_current_task_no_schedule, schedule, wakeup_task,
};
use alloc::{collections::VecDeque, sync::Arc};

/// A payload-free sleeping lock for ownership protocols that cannot carry a
/// Rust guard across an FFI boundary.
///
/// New callers queue behind the current owner. Unlock performs direct FIFO
/// ownership handoff: the lock remains logically held while the oldest waiter
/// is woken, so a later caller cannot steal it before that waiter runs.
pub struct RawSleepLock {
    inner: SpinNoIrqLock<RawSleepLockInner>,
}

struct RawSleepLockInner {
    locked: bool,
    wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

unsafe impl Send for RawSleepLock {}
unsafe impl Sync for RawSleepLock {}

impl RawSleepLock {
    pub fn new() -> Self {
        Self {
            inner: SpinNoIrqLock::new(RawSleepLockInner {
                locked: false,
                wait_queue: VecDeque::new(),
            }),
        }
    }

    pub fn lock(&self) {
        let mut inner = self.inner.lock();
        if inner.locked {
            let (task, task_cx_ptr): (Arc<TaskControlBlock>, *mut TaskContext) =
                block_current_task_no_schedule();
            inner.wait_queue.push_back(task);
            drop(inner);
            schedule(task_cx_ptr);
        } else {
            inner.locked = true;
        }
    }

    pub fn try_lock(&self) -> bool {
        let Some(mut inner) = self.inner.try_lock() else {
            return false;
        };
        if inner.locked {
            false
        } else {
            inner.locked = true;
            true
        }
    }

    /// Releases one ownership acquired by [`Self::lock`] or
    /// [`Self::try_lock`].
    ///
    /// # Safety
    ///
    /// The caller must own this lock, must release it exactly once, and must
    /// not access any protected state after this call without reacquiring it.
    /// This explicit operation exists only for paired FFI lock/unlock
    /// callbacks; ordinary Rust state should use [`super::SleepMutex`].
    pub unsafe fn unlock(&self) {
        let waking_task = self.inner.with_lock(|inner| {
            assert!(inner.locked, "raw sleeping lock released without ownership");
            if let Some(task) = inner.wait_queue.pop_front() {
                Some(task)
            } else {
                inner.locked = false;
                None
            }
        });
        if let Some(task) = waking_task {
            wakeup_task(task);
        }
    }
}

impl Default for RawSleepLock {
    fn default() -> Self {
        Self::new()
    }
}
