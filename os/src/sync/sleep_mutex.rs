use super::RawSleepLock;
use core::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
};

pub struct SleepMutex<T> {
    data: UnsafeCell<T>,
    lock: RawSleepLock,
}

pub struct SleepMutexGuard<'a, T> {
    mutex: &'a SleepMutex<T>,
}

unsafe impl<T: Send> Send for SleepMutex<T> {}
unsafe impl<T: Send> Sync for SleepMutex<T> {}

impl<T> SleepMutex<T> {
    pub fn new(data: T) -> Self {
        Self {
            data: UnsafeCell::new(data),
            lock: RawSleepLock::new(),
        }
    }

    pub fn lock(&self) -> SleepMutexGuard<'_, T> {
        self.lock.lock();
        SleepMutexGuard { mutex: self }
    }

    pub fn try_lock(&self) -> Option<SleepMutexGuard<'_, T>> {
        if self.lock.try_lock() {
            Some(SleepMutexGuard { mutex: self })
        } else {
            None
        }
    }
}

impl<T> Drop for SleepMutexGuard<'_, T> {
    fn drop(&mut self) {
        // SAFETY: construction of this guard follows exactly one successful
        // lock acquisition and Rust ownership drops the guard exactly once.
        unsafe { self.mutex.lock.unlock() };
    }
}

impl<T> Deref for SleepMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for SleepMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.data.get() }
    }
}
