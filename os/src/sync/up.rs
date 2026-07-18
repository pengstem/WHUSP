use super::LocalIrqGuard;
use core::cell::{RefCell, RefMut};
use core::ops::{Deref, DerefMut};

/*
/// Wrap a static data structure inside it so that we are
/// able to access it without any `unsafe`.
///
/// We should only use it in uniprocessor.
///
/// In order to get mutable reference of inner data, call
/// `exclusive_access`.
pub struct UPSafeCell<T> {
    /// inner data
    inner: RefCell<T>,
}

unsafe impl<T> Sync for UPSafeCell<T> {}

impl<T> UPSafeCell<T> {
    /// User is responsible to guarantee that inner struct is only used in
    /// uniprocessor.
    pub unsafe fn new(value: T) -> Self {
        Self {
            inner: RefCell::new(value),
        }
    }
    /// Panic if the data has been borrowed.
    pub fn exclusive_access(&self) -> RefMut<'_, T> {
        self.inner.borrow_mut()
    }
}
*/

/// Legacy CPU-local cell used while scheduler-owned objects are migrated.
///
/// Local interrupt masking prevents re-entry on one CPU, but does not provide
/// mutual exclusion between CPUs. Do not place data reachable by multiple CPUs
/// behind this type; use SpinLock or SpinNoIrqLock instead.
pub struct UPIntrFreeCell<T> {
    /// inner data
    inner: RefCell<T>,
}

unsafe impl<T> Sync for UPIntrFreeCell<T> {}

pub struct UPIntrRefMut<'a, T> {
    inner: Option<RefMut<'a, T>>,
    _irq: LocalIrqGuard,
}

impl<T> UPIntrFreeCell<T> {
    pub unsafe fn new(value: T) -> Self {
        Self {
            inner: RefCell::new(value),
        }
    }

    /// Panic if the data has been borrowed.
    pub fn exclusive_access(&self) -> UPIntrRefMut<'_, T> {
        let irq = LocalIrqGuard::disable();
        UPIntrRefMut {
            inner: Some(self.inner.borrow_mut()),
            _irq: irq,
        }
    }

    pub fn try_exclusive_access(&self) -> Option<UPIntrRefMut<'_, T>> {
        let irq = LocalIrqGuard::disable();
        match self.inner.try_borrow_mut() {
            Ok(inner) => Some(UPIntrRefMut {
                inner: Some(inner),
                _irq: irq,
            }),
            Err(_) => None,
        }
    }

    pub fn exclusive_session<F, V>(&self, f: F) -> V
    where
        F: FnOnce(&mut T) -> V,
    {
        let mut inner = self.exclusive_access();
        f(inner.deref_mut())
    }
}

impl<'a, T> Drop for UPIntrRefMut<'a, T> {
    fn drop(&mut self) {
        self.inner = None;
    }
}

impl<'a, T> Deref for UPIntrRefMut<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.inner.as_ref().unwrap().deref()
    }
}
impl<'a, T> DerefMut for UPIntrRefMut<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.as_mut().unwrap().deref_mut()
    }
}
