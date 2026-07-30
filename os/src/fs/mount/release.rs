use super::super::inode_state::InodeState;
use crate::sync::UPIntrFreeCell;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

pub(super) struct PendingReleaseQueue {
    entries: UPIntrFreeCell<Vec<PendingInodeRelease>>,
    nonempty: AtomicBool,
}

pub(super) struct PendingInodeRelease {
    pub(super) ino: u32,
    pub(super) state: Arc<InodeState>,
    pub(super) attempts: usize,
}

impl PendingReleaseQueue {
    pub(super) fn new() -> Self {
        Self {
            entries: unsafe { UPIntrFreeCell::new(Vec::new()) },
            nonempty: AtomicBool::new(false),
        }
    }

    pub(super) fn push(&self, entry: PendingInodeRelease) {
        let mut entries = self.entries.exclusive_access();
        entries.push(entry);
        self.nonempty.store(true, Ordering::Release);
    }

    pub(super) fn take(&self) -> Vec<PendingInodeRelease> {
        if !self.nonempty.load(Ordering::Acquire) {
            return Vec::new();
        }
        let mut entries = self.entries.exclusive_access();
        let pending = core::mem::take(&mut *entries);
        self.nonempty.store(false, Ordering::Release);
        pending
    }

    pub(super) fn put_back(&self, entries: Vec<PendingInodeRelease>) {
        if !entries.is_empty() {
            let mut pending = self.entries.exclusive_access();
            pending.extend(entries);
            self.nonempty.store(true, Ordering::Release);
        }
    }

    pub(super) fn has_entries(&self) -> bool {
        self.nonempty.load(Ordering::Acquire)
    }
}
