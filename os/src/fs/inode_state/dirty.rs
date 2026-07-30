use super::{InodeState, invalidate_direct_stat_cache};
use crate::config::PAGE_SIZE;
use crate::fs::{FileTimestamp, vfs::VfsNodeId};
use crate::sync::SleepMutex;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;

// CONTEXT: Mirrors DIRTY_REGULAR_FILES.len() so the read hot path can skip the
// global DIRTY_REGULAR_FILES SleepMutex entirely when no regular file has
// buffered dirty pages (e.g. iozone's read phase after fsync). Maintained only
// under the map lock at every mutation site, so a value of 0 reliably means
// "no dirty data anywhere" -- it is never a false negative for an active write.
static DIRTY_REGULAR_FILE_COUNT: AtomicUsize = AtomicUsize::new(0);

lazy_static! {
    pub(crate) static ref DIRTY_REGULAR_FILES: SleepMutex<BTreeMap<VfsNodeId, DirtyFileCache>> =
        SleepMutex::new(BTreeMap::new());
}

#[derive(Debug)]
pub(crate) struct DirtyPage {
    pub(crate) data: Vec<u8>,
    dirty_ranges: Vec<(usize, usize)>,
}

impl DirtyPage {
    pub(crate) fn empty() -> Self {
        Self {
            data: vec![0u8; PAGE_SIZE],
            dirty_ranges: Vec::new(),
        }
    }

    pub(crate) fn full(mut data: Vec<u8>) -> Self {
        if data.len() != PAGE_SIZE {
            data.resize(PAGE_SIZE, 0);
        }
        Self {
            data,
            dirty_ranges: vec![(0, PAGE_SIZE)],
        }
    }

    pub(crate) fn mark_dirty(&mut self, start: usize, end: usize) {
        debug_assert!(start <= end && end <= PAGE_SIZE);
        if start == end {
            return;
        }
        let mut merged_start = start;
        let mut merged_end = end;
        let mut index = 0usize;
        while index < self.dirty_ranges.len() {
            let (range_start, range_end) = self.dirty_ranges[index];
            if range_end < merged_start {
                index += 1;
                continue;
            }
            if range_start > merged_end {
                break;
            }
            merged_start = merged_start.min(range_start);
            merged_end = merged_end.max(range_end);
            self.dirty_ranges.remove(index);
        }
        self.dirty_ranges.insert(index, (merged_start, merged_end));
    }

    pub(crate) fn dirty_ranges(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.dirty_ranges.iter().copied()
    }
}

pub(crate) struct DirtyFileCache {
    pub(crate) inode_state: Arc<InodeState>,
    pub(crate) logical_size: usize,
    pub(crate) mtime: FileTimestamp,
    pub(crate) ctime: FileTimestamp,
    pub(crate) pages: BTreeMap<usize, DirtyPage>,
}

impl DirtyFileCache {
    pub(crate) fn new(
        inode_state: Arc<InodeState>,
        logical_size: usize,
        timestamp: FileTimestamp,
    ) -> Self {
        Self {
            inode_state,
            logical_size,
            mtime: timestamp,
            ctime: timestamp,
            pages: BTreeMap::new(),
        }
    }
}

pub(crate) fn any_regular_file_dirty() -> bool {
    DIRTY_REGULAR_FILE_COUNT.load(Ordering::Relaxed) != 0
}

pub(crate) fn sync_dirty_regular_file_count(map: &BTreeMap<VfsNodeId, DirtyFileCache>) {
    invalidate_direct_stat_cache();
    DIRTY_REGULAR_FILE_COUNT.store(map.len(), Ordering::Relaxed);
}
