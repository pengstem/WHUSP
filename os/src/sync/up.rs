use super::{SpinNoIrqLock, SpinNoIrqLockGuard};
use core::ops::{Deref, DerefMut};

/// Compatibility wrapper for kernel state historically protected only against
/// local interrupt re-entry.
///
/// The name is retained to keep this migration reviewable, but the implementation
/// is now an SMP-safe irq-masking spin lock. New shared state should use the
/// explicit SpinLock/SpinNoIrqLock types so its interrupt-context contract is
/// visible at the declaration.
pub struct UPIntrFreeCell<T> {
    inner: SpinNoIrqLock<T>,
}

impl<T> UPIntrFreeCell<T> {
    /// Existing call sites use `unsafe` because the former RefCell-based type
    /// relied on a UP-only external invariant. The new lock does not require
    /// that invariant; the signature remains compatible during migration.
    pub const unsafe fn new(value: T) -> Self {
        Self {
            inner: SpinNoIrqLock::new(value),
        }
    }

    pub fn exclusive_access(&self) -> UPIntrRefMut<'_, T> {
        UPIntrRefMut(self.inner.lock())
    }

    pub fn try_exclusive_access(&self) -> Option<UPIntrRefMut<'_, T>> {
        self.inner.try_lock().map(UPIntrRefMut)
    }

    pub fn exclusive_session<F, V>(&self, f: F) -> V
    where
        F: FnOnce(&mut T) -> V,
    {
        let mut inner = self.exclusive_access();
        f(inner.deref_mut())
    }
}

#[must_use = "dropping the guard releases the lock and restores local interrupts"]
pub struct UPIntrRefMut<'a, T>(SpinNoIrqLockGuard<'a, T>);

impl<T> Deref for UPIntrRefMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}

impl<T> DerefMut for UPIntrRefMut<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.deref_mut()
    }
}
