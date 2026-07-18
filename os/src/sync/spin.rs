use super::LocalIrqGuard;
use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
#[cfg(debug_assertions)]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicBool, Ordering};

/// A small test-and-test-and-set spin lock for short SMP critical sections.
pub struct SpinLock<T> {
    locked: AtomicBool,
    #[cfg(debug_assertions)]
    owner: AtomicUsize,
    value: UnsafeCell<T>,
}

#[cfg(debug_assertions)]
const LOCK_OWNER_NONE: usize = usize::MAX;
#[cfg(debug_assertions)]
const LOCK_OWNER_EARLY: usize = usize::MAX - 1;

unsafe impl<T: Send> Send for SpinLock<T> {}
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            #[cfg(debug_assertions)]
            owner: AtomicUsize::new(LOCK_OWNER_NONE),
            value: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        self.assert_not_owned_by_current();
        loop {
            while self.locked.load(Ordering::Relaxed) {
                spin_loop();
            }
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                self.note_acquired();
                return SpinLockGuard {
                    lock: self,
                    _not_send: PhantomData,
                };
            }
        }
    }

    /// Acquires the lock and returns the number of busy/failed spin polls.
    /// Normal callers use `lock()` and add no statistics to the hot path.
    pub fn lock_counted(&self) -> (SpinLockGuard<'_, T>, usize) {
        self.assert_not_owned_by_current();
        let mut spins = 0usize;
        loop {
            while self.locked.load(Ordering::Relaxed) {
                spins = spins.saturating_add(1);
                spin_loop();
            }
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                self.note_acquired();
                return (
                    SpinLockGuard {
                        lock: self,
                        _not_send: PhantomData,
                    },
                    spins,
                );
            }
            spins = spins.saturating_add(1);
        }
    }

    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        // Recursive try-lock is a normal nonblocking miss. Several accounting
        // paths deliberately use it while an outer object guard may be held.
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| {
                self.note_acquired();
                SpinLockGuard {
                    lock: self,
                    _not_send: PhantomData,
                }
            })
    }

    #[inline]
    fn assert_not_owned_by_current(&self) {
        #[cfg(debug_assertions)]
        if let Some(current) = crate::cpu::try_current_id().map(|cpu| cpu + 1) {
            assert_ne!(
                self.owner.load(Ordering::Relaxed),
                current,
                "recursive spin-lock acquisition"
            );
        }
    }

    #[inline]
    fn note_acquired(&self) {
        #[cfg(debug_assertions)]
        {
            let owner = crate::cpu::try_current_id()
                .map(|cpu| cpu + 1)
                .unwrap_or(LOCK_OWNER_EARLY);
            let previous = self.owner.swap(owner, Ordering::Relaxed);
            assert_eq!(
                previous, LOCK_OWNER_NONE,
                "recursive/corrupt spin-lock owner"
            );
        }
    }

    #[inline]
    fn note_releasing(&self) {
        #[cfg(debug_assertions)]
        {
            let owner = self.owner.load(Ordering::Relaxed);
            if owner != LOCK_OWNER_EARLY {
                let current = crate::cpu::try_current_id()
                    .map(|cpu| cpu + 1)
                    .expect("tracked spin lock dropped without CPU-local identity");
                assert_eq!(owner, current, "spin lock dropped on a different CPU");
            }
            self.owner.store(LOCK_OWNER_NONE, Ordering::Relaxed);
        }
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
        self.lock.note_releasing();
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
