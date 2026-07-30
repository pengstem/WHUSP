//! Copy-on-publish snapshots for short, non-blocking read-mostly paths.

use crate::config::MAX_CPUS;
use alloc::boxed::Box;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

#[repr(align(64))]
struct ReaderSlot {
    active: AtomicUsize,
}

impl ReaderSlot {
    const fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
        }
    }

    fn enter(&self) -> ReadEpoch<'_> {
        self.active.fetch_add(1, Ordering::AcqRel);
        ReadEpoch {
            active: &self.active,
        }
    }
}

struct ReadEpoch<'a> {
    active: &'a AtomicUsize,
}

impl Drop for ReadEpoch<'_> {
    fn drop(&mut self) {
        let previous = self.active.fetch_sub(1, Ordering::Release);
        debug_assert!(previous > 0, "read-mostly reader epoch underflow");
    }
}

/// An immutable snapshot with per-CPU reader grace periods.
///
/// Readers may only borrow the current value inside [`Self::read`]. The
/// closure must not block or migrate to another CPU. Publishers must be
/// externally serialized; publication closes the read phase, waits for prior
/// readers, swaps ownership, and reclaims the old value before reopening it.
pub(crate) struct ReadMostlySnapshot<T: Send + Sync> {
    sequence: AtomicUsize,
    current: AtomicPtr<T>,
    readers: [ReaderSlot; MAX_CPUS],
}

impl<T: Send + Sync> ReadMostlySnapshot<T> {
    pub(crate) fn new(initial: T) -> Self {
        Self {
            sequence: AtomicUsize::new(0),
            current: AtomicPtr::new(Box::into_raw(Box::new(initial))),
            readers: [const { ReaderSlot::new() }; MAX_CPUS],
        }
    }

    pub(crate) fn read<R>(&self, read: impl FnOnce(&T) -> R) -> R {
        let reader = &self.readers[crate::cpu::current_id()];
        loop {
            let sequence = self.sequence.load(Ordering::Acquire);
            if sequence & 1 != 0 {
                spin_loop();
                continue;
            }

            let epoch = reader.enter();
            if self.sequence.load(Ordering::Acquire) != sequence {
                drop(epoch);
                continue;
            }

            let current = self.current.load(Ordering::Acquire);
            assert!(!current.is_null(), "read-mostly snapshot is missing");
            let value = read(unsafe { &*current });
            drop(epoch);
            return value;
        }
    }

    pub(crate) fn publish(&self, updated: T) {
        let start = self.sequence.load(Ordering::Acquire);
        assert_eq!(start & 1, 0, "concurrent read-mostly snapshot writer");
        let publishing = start
            .checked_add(1)
            .expect("read-mostly snapshot sequence exhausted");
        assert!(
            self.sequence
                .compare_exchange(start, publishing, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "concurrent read-mostly snapshot writer"
        );

        for reader in &self.readers {
            while reader.active.load(Ordering::Acquire) != 0 {
                crate::cpu::handle_remote_sync_ipi();
                spin_loop();
            }
        }

        let replacement = Box::into_raw(Box::new(updated));
        let previous = self.current.swap(replacement, Ordering::AcqRel);
        assert!(!previous.is_null(), "read-mostly snapshot is missing");
        unsafe {
            drop(Box::from_raw(previous));
        }
        self.sequence.store(
            start
                .checked_add(2)
                .expect("read-mostly snapshot sequence exhausted"),
            Ordering::Release,
        );
    }
}

impl<T: Send + Sync> Drop for ReadMostlySnapshot<T> {
    fn drop(&mut self) {
        let current = *self.current.get_mut();
        if !current.is_null() {
            unsafe {
                drop(Box::from_raw(current));
            }
        }
    }
}
