use super::SpinNoIrqLock;
use crate::task::{
    TaskContext, TaskControlBlock, block_current_task_no_schedule,
    block_current_task_no_schedule_unless_interrupting_signal, schedule, wakeup_task,
};
use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitKind {
    Reader,
    Writer,
}

struct Waiter {
    kind: WaitKind,
    task: Arc<TaskControlBlock>,
    granted: AtomicBool,
}

struct SleepRwLockInner {
    active_readers: usize,
    writer_active: bool,
    #[cfg(feature = "sleep-rwlock-probe")]
    waiting_readers: usize,
    waiting_writers: usize,
    #[cfg(feature = "sleep-rwlock-probe")]
    max_waiters: usize,
    wait_queue: VecDeque<Arc<Waiter>>,
}

impl SleepRwLockInner {
    fn can_read_directly(&self) -> bool {
        !self.writer_active && self.waiting_writers == 0
    }

    fn can_write_directly(&self) -> bool {
        !self.writer_active && self.active_readers == 0 && self.wait_queue.is_empty()
    }

    fn enqueue(&mut self, waiter: Arc<Waiter>) {
        match waiter.kind {
            WaitKind::Reader => {
                #[cfg(feature = "sleep-rwlock-probe")]
                {
                    self.waiting_readers += 1;
                }
            }
            WaitKind::Writer => self.waiting_writers += 1,
        }
        self.wait_queue.push_back(waiter);
        #[cfg(feature = "sleep-rwlock-probe")]
        {
            self.max_waiters = self.max_waiters.max(self.wait_queue.len());
        }
    }

    fn remove_waiter(&mut self, waiter: &Arc<Waiter>) {
        let position = self
            .wait_queue
            .iter()
            .position(|queued| Arc::ptr_eq(queued, waiter))
            .expect("ungranted SleepRwLock waiter must remain queued");
        let removed = self
            .wait_queue
            .remove(position)
            .expect("SleepRwLock waiter position disappeared");
        debug_assert!(Arc::ptr_eq(&removed, waiter));
        match waiter.kind {
            WaitKind::Reader => {
                #[cfg(feature = "sleep-rwlock-probe")]
                {
                    self.waiting_readers -= 1;
                }
            }
            WaitKind::Writer => self.waiting_writers -= 1,
        }
    }

    /// Transfers ownership while the short internal state lock is held. The
    /// returned tasks must be woken only after that state lock is released.
    fn grant_front_phase(&mut self) -> Vec<Arc<TaskControlBlock>> {
        debug_assert!(!self.writer_active);
        debug_assert_eq!(self.active_readers, 0);
        let mut waking = Vec::new();
        let Some(front) = self.wait_queue.front() else {
            return waking;
        };
        match front.kind {
            WaitKind::Writer => {
                let waiter = self
                    .wait_queue
                    .pop_front()
                    .expect("SleepRwLock writer front disappeared");
                self.waiting_writers -= 1;
                self.writer_active = true;
                waiter.granted.store(true, Ordering::Release);
                waking.push(Arc::clone(&waiter.task));
            }
            WaitKind::Reader => {
                while self
                    .wait_queue
                    .front()
                    .is_some_and(|waiter| waiter.kind == WaitKind::Reader)
                {
                    let waiter = self
                        .wait_queue
                        .pop_front()
                        .expect("SleepRwLock reader front disappeared");
                    #[cfg(feature = "sleep-rwlock-probe")]
                    {
                        self.waiting_readers -= 1;
                    }
                    self.active_readers += 1;
                    waiter.granted.store(true, Ordering::Release);
                    waking.push(Arc::clone(&waiter.task));
                }
            }
        }
        waking
    }
}

/// A sleeping, FIFO phase-fair reader/writer lock.
///
/// New readers may join an active reader phase only while no writer is queued.
/// Ownership is handed to the queue front before wakeup: one writer, or the
/// contiguous reader batch at the head. The internal `SpinNoIrqLock` protects
/// only queue/state bookkeeping and is always released before scheduler wakeup.
pub struct SleepRwLock<T> {
    data: UnsafeCell<T>,
    inner: SpinNoIrqLock<SleepRwLockInner>,
}

pub struct SleepRwLockReadGuard<'a, T> {
    lock: &'a SleepRwLock<T>,
}

pub struct SleepRwLockWriteGuard<'a, T> {
    lock: &'a SleepRwLock<T>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SleepRwLockInterrupted;

#[cfg(feature = "sleep-rwlock-probe")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SleepRwLockStats {
    pub active_readers: usize,
    pub writer_active: bool,
    pub waiting_readers: usize,
    pub waiting_writers: usize,
    pub max_waiters: usize,
}

unsafe impl<T: Send> Send for SleepRwLock<T> {}
unsafe impl<T: Send + Sync> Sync for SleepRwLock<T> {}

impl<T> SleepRwLock<T> {
    pub fn new(data: T) -> Self {
        Self {
            data: UnsafeCell::new(data),
            inner: SpinNoIrqLock::new(SleepRwLockInner {
                active_readers: 0,
                writer_active: false,
                #[cfg(feature = "sleep-rwlock-probe")]
                waiting_readers: 0,
                waiting_writers: 0,
                #[cfg(feature = "sleep-rwlock-probe")]
                max_waiters: 0,
                wait_queue: VecDeque::new(),
            }),
        }
    }

    pub fn read(&self) -> SleepRwLockReadGuard<'_, T> {
        self.acquire(WaitKind::Reader, false)
            .expect("uninterruptible SleepRwLock read was interrupted");
        SleepRwLockReadGuard { lock: self }
    }

    pub fn write(&self) -> SleepRwLockWriteGuard<'_, T> {
        self.acquire(WaitKind::Writer, false)
            .expect("uninterruptible SleepRwLock write was interrupted");
        SleepRwLockWriteGuard { lock: self }
    }

    #[cfg(feature = "sleep-rwlock-probe")]
    pub(crate) fn write_interruptible(
        &self,
    ) -> Result<SleepRwLockWriteGuard<'_, T>, SleepRwLockInterrupted> {
        self.acquire(WaitKind::Writer, true)?;
        Ok(SleepRwLockWriteGuard { lock: self })
    }

    #[cfg(feature = "sleep-rwlock-probe")]
    pub(crate) fn try_read(&self) -> Option<SleepRwLockReadGuard<'_, T>> {
        let mut inner = self.inner.try_lock()?;
        if !inner.can_read_directly() {
            return None;
        }
        inner.active_readers += 1;
        Some(SleepRwLockReadGuard { lock: self })
    }

    #[cfg(feature = "sleep-rwlock-probe")]
    pub(crate) fn try_write(&self) -> Option<SleepRwLockWriteGuard<'_, T>> {
        let mut inner = self.inner.try_lock()?;
        if !inner.can_write_directly() {
            return None;
        }
        inner.writer_active = true;
        Some(SleepRwLockWriteGuard { lock: self })
    }

    #[cfg(feature = "sleep-rwlock-probe")]
    pub(crate) fn stats(&self) -> SleepRwLockStats {
        let inner = self.inner.lock();
        SleepRwLockStats {
            active_readers: inner.active_readers,
            writer_active: inner.writer_active,
            waiting_readers: inner.waiting_readers,
            waiting_writers: inner.waiting_writers,
            max_waiters: inner.max_waiters,
        }
    }

    #[cfg(feature = "sleep-rwlock-probe")]
    pub(crate) fn reset_max_waiters_for_probe(&self) {
        let mut inner = self.inner.lock();
        inner.max_waiters = inner.wait_queue.len();
    }

    fn acquire(&self, kind: WaitKind, interruptible: bool) -> Result<(), SleepRwLockInterrupted> {
        let mut inner = self.inner.lock();
        let direct = match kind {
            WaitKind::Reader => inner.can_read_directly(),
            WaitKind::Writer => inner.can_write_directly(),
        };
        if direct {
            match kind {
                WaitKind::Reader => inner.active_readers += 1,
                WaitKind::Writer => inner.writer_active = true,
            }
            return Ok(());
        }

        let Some((task, task_cx_ptr)) = block_for_lock(interruptible) else {
            return Err(SleepRwLockInterrupted);
        };
        let waiter = Arc::new(Waiter {
            kind,
            task,
            granted: AtomicBool::new(false),
        });
        inner.enqueue(Arc::clone(&waiter));
        drop(inner);
        schedule(task_cx_ptr);

        loop {
            let mut inner = self.inner.lock();
            if waiter.granted.load(Ordering::Acquire) {
                return Ok(());
            }

            let Some((task, task_cx_ptr)) = block_for_lock(interruptible) else {
                inner.remove_waiter(&waiter);
                let waking = if !inner.writer_active && inner.active_readers == 0 {
                    inner.grant_front_phase()
                } else {
                    Vec::new()
                };
                drop(inner);
                wake_all(waking);
                return Err(SleepRwLockInterrupted);
            };
            debug_assert!(Arc::ptr_eq(&task, &waiter.task));
            drop(task);
            drop(inner);
            schedule(task_cx_ptr);
        }
    }

    fn release_read(&self) {
        let waking = self.inner.with_lock(|inner| {
            assert!(inner.active_readers > 0);
            inner.active_readers -= 1;
            if inner.active_readers == 0 {
                inner.grant_front_phase()
            } else {
                Vec::new()
            }
        });
        wake_all(waking);
    }

    fn release_write(&self) {
        let waking = self.inner.with_lock(|inner| {
            assert!(inner.writer_active);
            inner.writer_active = false;
            inner.grant_front_phase()
        });
        wake_all(waking);
    }
}

fn block_for_lock(interruptible: bool) -> Option<(Arc<TaskControlBlock>, *mut TaskContext)> {
    if interruptible {
        block_current_task_no_schedule_unless_interrupting_signal()
    } else {
        Some(block_current_task_no_schedule())
    }
}

fn wake_all(tasks: Vec<Arc<TaskControlBlock>>) {
    for task in tasks {
        wakeup_task(task);
    }
}

impl<T> Deref for SleepRwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> Deref for SleepRwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SleepRwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SleepRwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.release_read();
    }
}

impl<T> Drop for SleepRwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.release_write();
    }
}
