use super::{InodeState, invalidate_direct_stat_cache};
use crate::config::PAGE_SIZE;
use crate::fs::{FileTimestamp, mount::MountId, vfs::VfsNodeId};
use crate::sync::{SleepMutex, SleepMutexGuard};
#[cfg(feature = "perf-counters")]
use crate::timer;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;

const DIRTY_INODE_SHARDS: usize = 16;

static TOTAL_DIRTY_PAGES: AtomicUsize = AtomicUsize::new(0);
static DIRTY_REGULAR_FILE_COUNT: AtomicUsize = AtomicUsize::new(0);
static DIRTY_PRESSURE_CURSOR: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "perf-counters")]
static DIRTY_OVERLAY_LOCK_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static DIRTY_OVERLAY_LOCK_CONTENDED: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static DIRTY_OVERLAY_LOCK_WAIT_TICKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static DIRTY_OVERLAY_LOCK_HOLD_TICKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static DIRTY_OVERLAY_GLOBAL_SCAN_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static DIRTY_OVERLAY_GLOBAL_SCAN_FILES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static DIRTY_OVERLAY_GLOBAL_SCAN_PAGES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static DIRTY_OVERLAY_LOCKED_ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static DIRTY_OVERLAY_LOCKED_COPY_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static DIRTY_OVERLAY_PRESSURE_CANDIDATES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static DIRTY_OVERLAY_PRESSURE_BATCH_INODES_MAX: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static DIRTY_OVERLAY_PRESSURE_BATCH_PAGES_MAX: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static DIRTY_OVERLAY_PRESSURE_BUDGET_STOPS: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "perf-counters")]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DirtyOverlayStats {
    pub(crate) lock_calls: usize,
    pub(crate) lock_contended: usize,
    pub(crate) lock_wait_ticks: usize,
    pub(crate) lock_hold_ticks: usize,
    pub(crate) global_scan_calls: usize,
    pub(crate) global_scan_files: usize,
    pub(crate) global_scan_pages: usize,
    pub(crate) locked_alloc_bytes: usize,
    pub(crate) locked_copy_bytes: usize,
    pub(crate) pressure_candidates: usize,
    pub(crate) pressure_batch_inodes_max: usize,
    pub(crate) pressure_batch_pages_max: usize,
    pub(crate) pressure_budget_stops: usize,
}

struct DirtyInodeQueue {
    states: VecDeque<Arc<InodeState>>,
}

impl DirtyInodeQueue {
    fn new() -> Self {
        Self {
            states: VecDeque::new(),
        }
    }
}

lazy_static! {
    static ref DIRTY_INODE_QUEUES: Vec<SleepMutex<DirtyInodeQueue>> = (0..DIRTY_INODE_SHARDS)
        .map(|_| SleepMutex::new(DirtyInodeQueue::new()))
        .collect();
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
    pub(crate) logical_size: usize,
    pub(crate) mtime: FileTimestamp,
    pub(crate) ctime: FileTimestamp,
    pub(crate) pages: BTreeMap<usize, DirtyPage>,
}

impl DirtyFileCache {
    pub(crate) fn new() -> Self {
        let timestamp = FileTimestamp::now();
        Self {
            logical_size: 0,
            mtime: timestamp,
            ctime: timestamp,
            pages: BTreeMap::new(),
        }
    }
}

pub(crate) struct DirtyFileGuard<'a> {
    inner: SleepMutexGuard<'a, DirtyFileCache>,
    #[cfg(feature = "perf-counters")]
    hold_start: usize,
}

impl Deref for DirtyFileGuard<'_> {
    type Target = DirtyFileCache;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for DirtyFileGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Drop for DirtyFileGuard<'_> {
    fn drop(&mut self) {
        #[cfg(feature = "perf-counters")]
        DIRTY_OVERLAY_LOCK_HOLD_TICKS.fetch_add(
            timer::get_time().wrapping_sub(self.hold_start),
            Ordering::Relaxed,
        );
    }
}

pub(crate) fn lock_dirty_file(state: &InodeState) -> DirtyFileGuard<'_> {
    #[cfg(feature = "perf-counters")]
    {
        DIRTY_OVERLAY_LOCK_CALLS.fetch_add(1, Ordering::Relaxed);
        let wait_start = timer::get_time();
        let (inner, contended) = match state.dirty.try_lock() {
            Some(inner) => (inner, false),
            None => (state.dirty.lock(), true),
        };
        if contended {
            DIRTY_OVERLAY_LOCK_CONTENDED.fetch_add(1, Ordering::Relaxed);
        }
        DIRTY_OVERLAY_LOCK_WAIT_TICKS.fetch_add(
            timer::get_time().wrapping_sub(wait_start),
            Ordering::Relaxed,
        );
        return DirtyFileGuard {
            inner,
            hold_start: timer::get_time(),
        };
    }
    #[cfg(not(feature = "perf-counters"))]
    DirtyFileGuard {
        inner: state.dirty.lock(),
    }
}

fn queue_index(node: VfsNodeId) -> usize {
    (node.mount_id.0.wrapping_mul(0x9e37_79b1) ^ node.ino as usize) % DIRTY_INODE_SHARDS
}

pub(crate) fn register_dirty_inode(state: Arc<InodeState>) {
    assert!(
        state
            .on_dirty_list
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok(),
        "dirty inode registered twice"
    );
    let mut queue = DIRTY_INODE_QUEUES[queue_index(state.node())].lock();
    assert!(
        !queue
            .states
            .iter()
            .any(|queued| Arc::ptr_eq(queued, &state)),
        "dirty inode queue contains duplicate state"
    );
    queue.states.push_back(state);
    DIRTY_REGULAR_FILE_COUNT.fetch_add(1, Ordering::Relaxed);
    invalidate_direct_stat_cache();
}

pub(crate) fn take_dirty_inode(state: &Arc<InodeState>) -> Option<Arc<InodeState>> {
    if state
        .on_dirty_list
        .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }
    let mut queue = DIRTY_INODE_QUEUES[queue_index(state.node())].lock();
    let Some(index) = queue
        .states
        .iter()
        .position(|queued| Arc::ptr_eq(queued, state))
    else {
        state.on_dirty_list.store(true, Ordering::Release);
        panic!("dirty inode membership flag lost its queue entry");
    };
    let removed = queue
        .states
        .remove(index)
        .expect("dirty inode queue index disappeared");
    let previous = DIRTY_REGULAR_FILE_COUNT.fetch_sub(1, Ordering::Relaxed);
    assert_ne!(previous, 0, "dirty inode count underflow");
    invalidate_direct_stat_cache();
    Some(removed)
}

pub(crate) fn any_regular_file_dirty() -> bool {
    TOTAL_DIRTY_PAGES.load(Ordering::Acquire) != 0
}

pub(crate) fn total_dirty_pages() -> usize {
    TOTAL_DIRTY_PAGES.load(Ordering::Acquire)
}

#[cfg(feature = "perf-counters")]
pub(crate) fn dirty_regular_file_count() -> usize {
    DIRTY_REGULAR_FILE_COUNT.load(Ordering::Acquire)
}

pub(crate) fn dirty_page_count(state: &InodeState) -> usize {
    state.dirty_page_count.load(Ordering::Acquire)
}

pub(crate) fn set_dirty_page_count(state: &InodeState, pages: usize) {
    state.dirty_page_count.store(pages, Ordering::Release);
}

pub(crate) fn try_reserve_dirty_pages(pages: usize, limit: usize) -> Option<usize> {
    if pages == 0 {
        return Some(total_dirty_pages());
    }
    let mut current = TOTAL_DIRTY_PAGES.load(Ordering::Acquire);
    loop {
        let next = current.checked_add(pages)?;
        if next > limit {
            return None;
        }
        match TOTAL_DIRTY_PAGES.compare_exchange_weak(
            current,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Some(next),
            Err(observed) => current = observed,
        }
    }
}

pub(crate) fn release_dirty_pages(pages: usize) {
    if pages == 0 {
        return;
    }
    let previous = TOTAL_DIRTY_PAGES.fetch_sub(pages, Ordering::AcqRel);
    assert!(previous >= pages, "dirty page total underflow");
}

pub(crate) fn restore_dirty_pages(pages: usize) -> usize {
    TOTAL_DIRTY_PAGES.fetch_add(pages, Ordering::AcqRel) + pages
}

pub(crate) fn dirty_inode_states_on_mount(mount_id: MountId) -> Vec<Arc<InodeState>> {
    let mut states = Vec::new();
    for queue in DIRTY_INODE_QUEUES.iter() {
        let queue = queue.lock();
        states.extend(
            queue
                .states
                .iter()
                .filter(|state| state.node().mount_id == mount_id)
                .cloned(),
        );
    }
    states
}

pub(crate) fn dirty_pressure_candidates(limit: usize) -> Vec<Arc<InodeState>> {
    let mut states = Vec::new();
    if limit == 0 {
        return states;
    }
    let start = DIRTY_PRESSURE_CURSOR.fetch_add(1, Ordering::Relaxed) % DIRTY_INODE_SHARDS;
    for offset in 0..DIRTY_INODE_SHARDS {
        let queue = DIRTY_INODE_QUEUES[(start + offset) % DIRTY_INODE_SHARDS].lock();
        for state in queue.states.iter() {
            states.push(Arc::clone(state));
            if states.len() == limit {
                return states;
            }
        }
    }
    states
}

#[cfg(feature = "perf-counters")]
pub(crate) fn record_dirty_overlay_locked_alloc(bytes: usize) {
    DIRTY_OVERLAY_LOCKED_ALLOC_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
pub(crate) fn record_dirty_overlay_locked_alloc(_bytes: usize) {}

#[cfg(feature = "perf-counters")]
pub(crate) fn record_dirty_overlay_locked_copy(bytes: usize) {
    DIRTY_OVERLAY_LOCKED_COPY_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
pub(crate) fn record_dirty_overlay_locked_copy(_bytes: usize) {}

#[cfg(feature = "perf-counters")]
pub(crate) fn record_dirty_overlay_pressure_batch(
    candidates: usize,
    inodes: usize,
    pages: usize,
    budget_stop: bool,
) {
    DIRTY_OVERLAY_PRESSURE_CANDIDATES.fetch_add(candidates, Ordering::Relaxed);
    DIRTY_OVERLAY_PRESSURE_BATCH_INODES_MAX.fetch_max(inodes, Ordering::Relaxed);
    DIRTY_OVERLAY_PRESSURE_BATCH_PAGES_MAX.fetch_max(pages, Ordering::Relaxed);
    if budget_stop {
        DIRTY_OVERLAY_PRESSURE_BUDGET_STOPS.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
pub(crate) fn record_dirty_overlay_pressure_batch(
    _candidates: usize,
    _inodes: usize,
    _pages: usize,
    _budget_stop: bool,
) {
}

#[cfg(feature = "perf-counters")]
pub(crate) fn dirty_overlay_stats_snapshot() -> DirtyOverlayStats {
    DirtyOverlayStats {
        lock_calls: DIRTY_OVERLAY_LOCK_CALLS.load(Ordering::Relaxed),
        lock_contended: DIRTY_OVERLAY_LOCK_CONTENDED.load(Ordering::Relaxed),
        lock_wait_ticks: DIRTY_OVERLAY_LOCK_WAIT_TICKS.load(Ordering::Relaxed),
        lock_hold_ticks: DIRTY_OVERLAY_LOCK_HOLD_TICKS.load(Ordering::Relaxed),
        global_scan_calls: DIRTY_OVERLAY_GLOBAL_SCAN_CALLS.load(Ordering::Relaxed),
        global_scan_files: DIRTY_OVERLAY_GLOBAL_SCAN_FILES.load(Ordering::Relaxed),
        global_scan_pages: DIRTY_OVERLAY_GLOBAL_SCAN_PAGES.load(Ordering::Relaxed),
        locked_alloc_bytes: DIRTY_OVERLAY_LOCKED_ALLOC_BYTES.load(Ordering::Relaxed),
        locked_copy_bytes: DIRTY_OVERLAY_LOCKED_COPY_BYTES.load(Ordering::Relaxed),
        pressure_candidates: DIRTY_OVERLAY_PRESSURE_CANDIDATES.load(Ordering::Relaxed),
        pressure_batch_inodes_max: DIRTY_OVERLAY_PRESSURE_BATCH_INODES_MAX.load(Ordering::Relaxed),
        pressure_batch_pages_max: DIRTY_OVERLAY_PRESSURE_BATCH_PAGES_MAX.load(Ordering::Relaxed),
        pressure_budget_stops: DIRTY_OVERLAY_PRESSURE_BUDGET_STOPS.load(Ordering::Relaxed),
    }
}
