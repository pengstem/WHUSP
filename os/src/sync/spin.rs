use super::LocalIrqGuard;
use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A small test-and-test-and-set spin lock for short SMP critical sections.
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for SpinLock<T> {}
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        loop {
            while self.locked.load(Ordering::Relaxed) {
                spin_loop();
            }
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return SpinLockGuard {
                    lock: self,
                    _not_send: PhantomData,
                };
            }
        }
    }

    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| SpinLockGuard {
                lock: self,
                _not_send: PhantomData,
            })
    }
}

#[must_use = "dropping the guard releases the spin lock"]
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    _not_send: PhantomData<*mut ()>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// A spin lock that masks local interrupts before acquisition.
///
/// Drop releases the shared lock before restoring the local IRQ state. This
/// ordering prevents an interrupt handler from observing a lock still held by
/// the interrupted context.
pub struct SpinNoIrqLock<T> {
    inner: SpinLock<T>,
}

impl<T> SpinNoIrqLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: SpinLock::new(value),
        }
    }

    pub fn lock(&self) -> SpinNoIrqLockGuard<'_, T> {
        let irq = LocalIrqGuard::disable();
        let lock = self.inner.lock();
        SpinNoIrqLockGuard {
            lock: Some(lock),
            irq,
        }
    }

    pub fn try_lock(&self) -> Option<SpinNoIrqLockGuard<'_, T>> {
        let irq = LocalIrqGuard::disable();
        self.inner.try_lock().map(|lock| SpinNoIrqLockGuard {
            lock: Some(lock),
            irq,
        })
    }
}

#[must_use = "dropping the guard releases the spin lock and restores local interrupts"]
pub struct SpinNoIrqLockGuard<'a, T> {
    lock: Option<SpinLockGuard<'a, T>>,
    #[allow(dead_code)]
    irq: LocalIrqGuard,
}

impl<T> Deref for SpinNoIrqLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.lock.as_ref().expect("spin guard already dropped")
    }
}

impl<T> DerefMut for SpinNoIrqLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.lock.as_mut().expect("spin guard already dropped")
    }
}

impl<T> Drop for SpinNoIrqLockGuard<'_, T> {
    fn drop(&mut self) {
        drop(self.lock.take());
    }
}
