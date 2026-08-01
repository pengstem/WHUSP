use super::LocalIrqGuard;
use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
        self.lock_inner(false)
    }

    fn lock_while_irqs_masked(&self) -> SpinLockGuard<'_, T> {
        self.lock_inner(true)
    }

    fn lock_inner(&self, poll_tlb: bool) -> SpinLockGuard<'_, T> {
        self.assert_not_owned_by_current();
        loop {
            while self.locked.load(Ordering::Relaxed) {
                if poll_tlb {
                    poll_tlb_while_spinning();
                }
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
            if poll_tlb {
                poll_tlb_while_spinning();
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

const SPIN_RWLOCK_WRITER: usize = 1usize << (usize::BITS - 1);
const SPIN_RWLOCK_WRITER_PENDING: usize = SPIN_RWLOCK_WRITER >> 1;
const SPIN_RWLOCK_READERS: usize = SPIN_RWLOCK_WRITER_PENDING - 1;

/// Reader-writer spin lock for short, non-sleeping cache/state lookups.
///
/// Callers must not cover filesystem/backend work, allocation, or any other
/// operation that can sleep. Writers are intentionally rare; this primitive is
/// optimized for immutable dentry shard hits.
pub struct SpinRwLock<T> {
    state: AtomicUsize,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for SpinRwLock<T> {}
unsafe impl<T: Send + Sync> Sync for SpinRwLock<T> {}

impl<T> SpinRwLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicUsize::new(0),
            value: UnsafeCell::new(value),
        }
    }

    pub fn read(&self) -> SpinRwLockReadGuard<'_, T> {
        loop {
            let state = self.state.load(Ordering::Relaxed);
            if state & (SPIN_RWLOCK_WRITER | SPIN_RWLOCK_WRITER_PENDING) != 0 {
                spin_loop();
                continue;
            }
            assert!(state & SPIN_RWLOCK_READERS != SPIN_RWLOCK_READERS);
            if self
                .state
                .compare_exchange_weak(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return SpinRwLockReadGuard {
                    lock: self,
                    _not_send: PhantomData,
                };
            }
        }
    }

    pub fn write(&self) -> SpinRwLockWriteGuard<'_, T> {
        loop {
            self.state
                .fetch_or(SPIN_RWLOCK_WRITER_PENDING, Ordering::AcqRel);
            loop {
                let state = self.state.load(Ordering::Relaxed);
                if state == SPIN_RWLOCK_WRITER_PENDING
                    && self
                        .state
                        .compare_exchange_weak(
                            SPIN_RWLOCK_WRITER_PENDING,
                            SPIN_RWLOCK_WRITER,
                            Ordering::Acquire,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                {
                    return SpinRwLockWriteGuard {
                        lock: self,
                        _not_send: PhantomData,
                    };
                }
                if state == 0 {
                    break;
                }
                spin_loop();
            }
        }
    }
}

#[must_use = "dropping the guard releases the spin read lock"]
pub struct SpinRwLockReadGuard<'a, T> {
    lock: &'a SpinRwLock<T>,
    _not_send: PhantomData<*mut ()>,
}

impl<T> Deref for SpinRwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> Drop for SpinRwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        let previous = self.lock.state.fetch_sub(1, Ordering::Release);
        assert!(previous > 0 && previous & SPIN_RWLOCK_WRITER == 0);
    }
}

#[must_use = "dropping the guard releases the spin write lock"]
pub struct SpinRwLockWriteGuard<'a, T> {
    lock: &'a SpinRwLock<T>,
    _not_send: PhantomData<*mut ()>,
}

impl<T> Deref for SpinRwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinRwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinRwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        let previous = self.lock.state.swap(0, Ordering::Release);
        // A contending writer advertises WRITER_PENDING while this writer is
        // active. Clearing both bits is safe: waiters observe zero, reassert
        // pending in their outer loop, and exactly one claims the write side.
        assert!(previous & SPIN_RWLOCK_WRITER != 0);
        assert_eq!(previous & SPIN_RWLOCK_READERS, 0);
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
        // LoongArch implements synchronous remote TLB invalidation with an
        // S-mode IPI. If this CPU is waiting for a lock while interrupts are
        // masked, the lock owner may itself be waiting for our acknowledgement.
        // Poll only the lock-free TLB request path here; scheduler/device IPIs
        // remain deferred until the saved interrupt state is restored.
        let lock = self.inner.lock_while_irqs_masked();
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

    pub fn with_lock<F, V>(&self, f: F) -> V
    where
        F: FnOnce(&mut T) -> V,
    {
        let mut guard = self.lock();
        f(&mut guard)
    }
}

/// Payload-free IRQ-safe spin lock for paired FFI lock/unlock callbacks.
///
/// This is restricted to short, non-sleeping bookkeeping sections. The
/// explicit unlock exists because a C callback pair cannot carry a Rust guard;
/// ordinary Rust code should use [`SpinNoIrqLock`]. Local interrupts remain
/// disabled from successful acquisition through the matching unlock, so the
/// owner cannot be preempted or migrate while the raw ownership is live.
pub struct RawSpinNoIrqLock {
    locked: AtomicBool,
    irq_was_enabled: AtomicBool,
    #[cfg(debug_assertions)]
    owner: AtomicUsize,
}

unsafe impl Send for RawSpinNoIrqLock {}
unsafe impl Sync for RawSpinNoIrqLock {}

impl RawSpinNoIrqLock {
    pub const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            irq_was_enabled: AtomicBool::new(false),
            #[cfg(debug_assertions)]
            owner: AtomicUsize::new(LOCK_OWNER_NONE),
        }
    }

    pub fn lock(&self) {
        let was_enabled = crate::arch::interrupt::supervisor_interrupt_enabled();
        crate::arch::interrupt::disable_supervisor_interrupt();
        self.assert_not_owned_by_current();
        loop {
            while self.locked.load(Ordering::Relaxed) {
                poll_tlb_while_spinning();
                spin_loop();
            }
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                self.note_acquired();
                self.irq_was_enabled.store(was_enabled, Ordering::Relaxed);
                return;
            }
            poll_tlb_while_spinning();
        }
    }

    #[cfg(feature = "perf-counters")]
    pub fn try_lock(&self) -> bool {
        let was_enabled = crate::arch::interrupt::supervisor_interrupt_enabled();
        crate::arch::interrupt::disable_supervisor_interrupt();
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            self.note_acquired();
            self.irq_was_enabled.store(was_enabled, Ordering::Relaxed);
            true
        } else {
            if was_enabled {
                crate::arch::interrupt::enable_supervisor_interrupt();
            }
            false
        }
    }

    /// Releases one acquisition by the current CPU.
    ///
    /// # Safety
    ///
    /// The current CPU must own this lock exactly once, and the protected C
    /// state must no longer be accessed after this call.
    pub unsafe fn unlock(&self) {
        self.note_releasing();
        let restore_irqs = self.irq_was_enabled.load(Ordering::Relaxed);
        self.locked.store(false, Ordering::Release);
        if restore_irqs {
            crate::arch::interrupt::enable_supervisor_interrupt();
        }
    }

    #[inline]
    fn assert_not_owned_by_current(&self) {
        #[cfg(debug_assertions)]
        if let Some(current) = crate::cpu::try_current_id().map(|cpu| cpu + 1) {
            assert_ne!(
                self.owner.load(Ordering::Relaxed),
                current,
                "recursive raw IRQ-safe spin-lock acquisition"
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
            assert_eq!(previous, LOCK_OWNER_NONE, "corrupt raw spin-lock owner");
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
                    .expect("raw spin lock released without CPU-local identity");
                assert_eq!(owner, current, "raw spin lock released on another CPU");
            }
            self.owner.store(LOCK_OWNER_NONE, Ordering::Relaxed);
        }
    }
}

impl Default for RawSpinNoIrqLock {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn poll_tlb_while_spinning() {
    if crate::cpu::try_current_id().is_some() {
        crate::cpu::handle_remote_sync_ipi();
    }
    #[cfg(target_arch = "loongarch64")]
    {
        if crate::cpu::try_current_id().is_some() {
            crate::arch::smp::handle_tlb_ipi();
        }
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
