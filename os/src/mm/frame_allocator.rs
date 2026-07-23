use super::{PhysAddr, PhysPageNum};
use crate::config::{MAX_CPUS, memory_end};
use crate::perf;
use crate::sync::{SpinNoIrqLock, UPIntrFreeCell};
use alloc::vec::Vec;
use core::fmt::{self, Debug, Formatter};
#[cfg(feature = "perf-counters")]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicU32, Ordering};
use lazy_static::*;

const FRAME_CACHE_CAPACITY: usize = 64;
const FRAME_CACHE_BATCH: usize = 32;

#[cfg(feature = "perf-counters")]
static FRAME_CACHE_HITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static FRAME_CACHE_REFILLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static FRAME_CACHE_REFILL_PAGES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static FRAME_CACHE_LOCAL_FREES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static FRAME_CACHE_DRAINS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static FRAME_CACHE_DRAIN_PAGES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static FRAME_CACHE_GLOBAL_ALLOCS: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "perf-counters")]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FrameCacheStats {
    pub(crate) hits: usize,
    pub(crate) refills: usize,
    pub(crate) refill_pages: usize,
    pub(crate) local_frees: usize,
    pub(crate) drains: usize,
    pub(crate) drain_pages: usize,
    pub(crate) global_allocs: usize,
}

pub struct FrameTracker {
    pub ppn: PhysPageNum,
}

impl FrameTracker {
    pub fn new_zeroed(ppn: PhysPageNum) -> Self {
        let _profile_scope = perf::time_scope(perf::ProfilePoint::FrameZeroFill);
        ppn.get_bytes_array().fill(0);
        perf::record_frame_alloc(true);
        Self { ppn }
    }

    pub fn new_uninit(ppn: PhysPageNum) -> Self {
        perf::record_frame_alloc(false);
        Self { ppn }
    }

    pub fn from_retained(ppn: PhysPageNum) -> Option<Self> {
        frame_retain(ppn).then_some(Self { ppn })
    }
}

impl Debug for FrameTracker {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("FrameTracker:PPN={:#x}", self.ppn.0))
    }
}

impl Drop for FrameTracker {
    fn drop(&mut self) {
        frame_dealloc(self.ppn);
    }
}

/// Stable per-frame ownership metadata.
///
/// A zero count means the page is free and owned either by the global free
/// structure or by exactly one CPU cache. Claiming a page changes zero to one
/// only after removing it from that free owner. Atomic counts let retain/drop
/// avoid serializing unrelated pages through the global allocator lock.
struct FrameRefCounts {
    start: usize,
    counts: Vec<AtomicU32>,
}

impl FrameRefCounts {
    fn new(start: PhysPageNum, end: PhysPageNum) -> Self {
        let len = end.0.saturating_sub(start.0);
        let mut counts = Vec::with_capacity(len);
        counts.resize_with(len, || AtomicU32::new(0));
        Self {
            start: start.0,
            counts,
        }
    }

    fn slot(&self, ppn: PhysPageNum) -> Option<&AtomicU32> {
        ppn.0
            .checked_sub(self.start)
            .and_then(|index| self.counts.get(index))
    }

    fn claim_free(&self, ppn: PhysPageNum) {
        let slot = self
            .slot(ppn)
            .unwrap_or_else(|| panic!("frame PPN {ppn:?} is outside allocator metadata"));
        let previous = slot
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .unwrap_or_else(|count| panic!("free frame PPN {ppn:?} has reference count {count}"));
        assert_eq!(previous, 0);
    }

    fn release(&self, ppn: PhysPageNum) -> bool {
        let slot = self
            .slot(ppn)
            .unwrap_or_else(|| panic!("frame PPN {ppn:?} is outside allocator metadata"));
        loop {
            let count = slot.load(Ordering::Acquire);
            assert_ne!(count, 0, "frame PPN {ppn:?} has no reference count");
            if slot
                .compare_exchange_weak(count, count - 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return count == 1;
            }
        }
    }

    fn retain(&self, ppn: PhysPageNum) -> bool {
        let Some(slot) = self.slot(ppn) else {
            return false;
        };
        loop {
            let count = slot.load(Ordering::Acquire);
            if count == 0 {
                return false;
            }
            let next = count
                .checked_add(1)
                .expect("frame reference count overflow");
            if slot
                .compare_exchange_weak(count, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    fn ref_count(&self, ppn: PhysPageNum) -> Option<usize> {
        let count = self.slot(ppn)?.load(Ordering::Acquire);
        (count > 0).then_some(count as usize)
    }
}

struct FrameCache {
    pages: Vec<PhysPageNum>,
}

impl FrameCache {
    fn new() -> Self {
        Self {
            pages: Vec::with_capacity(FRAME_CACHE_CAPACITY),
        }
    }
}

trait FrameAllocator {
    fn new() -> Self;
    fn alloc(&mut self) -> Option<PhysPageNum>;
    fn alloc_more(&mut self, pages: usize) -> Option<Vec<PhysPageNum>>;
    fn recycle(&mut self, ppn: PhysPageNum);
}

pub struct StackFrameAllocator {
    start: usize,
    current: usize,
    end: usize,
    recycled: Vec<usize>,
}

impl StackFrameAllocator {
    pub fn init(&mut self, l: PhysPageNum, r: PhysPageNum) {
        self.start = l.0;
        self.current = l.0;
        self.end = r.0;
        // println!("last {} Physical Frames.", self.end - self.current);
    }

    pub fn stats(&self) -> (usize, usize) {
        let total = self.end.saturating_sub(self.start);
        let free = self
            .end
            .saturating_sub(self.current)
            .saturating_add(self.recycled.len());
        (total, free)
    }
}
impl FrameAllocator for StackFrameAllocator {
    fn new() -> Self {
        Self {
            start: 0,
            current: 0,
            end: 0,
            recycled: Vec::new(),
        }
    }
    fn alloc(&mut self) -> Option<PhysPageNum> {
        let ppn = if let Some(ppn) = self.recycled.pop() {
            ppn
        } else if self.current == self.end {
            return None;
        } else {
            self.current += 1;
            self.current - 1
        };
        Some(ppn.into())
    }
    fn alloc_more(&mut self, pages: usize) -> Option<Vec<PhysPageNum>> {
        if pages == 0 || self.current.checked_add(pages)? > self.end {
            None
        } else {
            self.current += pages;
            let arr: Vec<usize> = (1..pages + 1).collect();
            let v = arr
                .iter()
                .map(|x| {
                    let ppn = self.current - x;
                    ppn.into()
                })
                .collect();
            Some(v)
        }
    }
    fn recycle(&mut self, ppn: PhysPageNum) {
        let ppn = ppn.0;
        if ppn < self.start || ppn >= self.current {
            panic!("Frame ppn={ppn:#x} has not been allocated!");
        }
        self.recycled.push(ppn);
    }
}

type FrameAllocatorImpl = StackFrameAllocator;

// The global lock owns only the bump cursor and globally recycled pages.
// Per-CPU caches own pages removed from those structures, while stable atomic
// reference counts preserve COW/pinning semantics independently.
lazy_static! {
    static ref FRAME_ALLOCATOR: UPIntrFreeCell<FrameAllocatorImpl> =
        unsafe { UPIntrFreeCell::new(FrameAllocatorImpl::new()) };
    static ref FRAME_REF_COUNTS: FrameRefCounts = {
        unsafe extern "C" {
            safe fn ekernel();
        }
        FrameRefCounts::new(
            PhysAddr::from(ekernel as usize).ceil(),
            PhysAddr::from(memory_end()).floor(),
        )
    };
    static ref FRAME_CACHES: Vec<SpinNoIrqLock<FrameCache>> = (0..MAX_CPUS)
        .map(|_| SpinNoIrqLock::new(FrameCache::new()))
        .collect();
}

/// Initializes the physical frame allocator from the DTB-selected RAM range.
///
/// The first allocatable page starts after `ekernel`; earlier physical memory
/// contains the loaded kernel image and must never be recycled as user frames.
pub fn init_frame_allocator() {
    unsafe extern "C" {
        safe fn ekernel();
    }
    let start = PhysAddr::from(ekernel as usize).ceil();
    let end = PhysAddr::from(memory_end()).floor();
    lazy_static::initialize(&FRAME_REF_COUNTS);
    lazy_static::initialize(&FRAME_CACHES);
    assert_eq!(FRAME_REF_COUNTS.start, start.0);
    assert_eq!(FRAME_REF_COUNTS.counts.len(), end.0.saturating_sub(start.0));
    for cache in FRAME_CACHES.iter() {
        assert!(
            cache.lock().pages.is_empty(),
            "frame cache was populated before allocator initialization"
        );
    }
    FRAME_ALLOCATOR.exclusive_access().init(start, end);
}

pub fn frame_alloc() -> Option<FrameTracker> {
    let _profile_scope = perf::time_scope(perf::ProfilePoint::FrameAllocZeroed);
    alloc_frame_ppn().map(FrameTracker::new_zeroed)
}

pub fn frame_alloc_uninit() -> Option<FrameTracker> {
    let _profile_scope = perf::time_scope(perf::ProfilePoint::FrameAllocUninit);
    alloc_frame_ppn().map(FrameTracker::new_uninit)
}

/// Allocates a contiguous fresh page run for device DMA queues.
///
/// This path intentionally does not satisfy requests from recycled single
/// pages; callers such as VirtIO pass the first physical address to hardware.
pub fn frame_alloc_more(num: usize) -> Option<Vec<FrameTracker>> {
    let _profile_scope = perf::time_scope(perf::ProfilePoint::FrameAllocDma);
    let pages = FRAME_ALLOCATOR.exclusive_access().alloc_more(num)?;
    for ppn in pages.iter().copied() {
        FRAME_REF_COUNTS.claim_free(ppn);
    }
    Some(pages.into_iter().map(FrameTracker::new_zeroed).collect())
}

pub fn frame_dealloc(ppn: PhysPageNum) {
    if !FRAME_REF_COUNTS.release(ppn) {
        perf::record_frame_dealloc(false, true, 0, 0);
        return;
    }

    if let Some(cpu) = crate::cpu::try_current_id() {
        let mut drained = Vec::new();
        {
            let mut cache = FRAME_CACHES[cpu].lock();
            cache.pages.push(ppn);
            if cache.pages.len() > FRAME_CACHE_CAPACITY {
                let drain_start = cache.pages.len() - FRAME_CACHE_BATCH;
                drained = cache.pages.split_off(drain_start);
            }
        }
        record_frame_cache_local_free();
        if !drained.is_empty() {
            record_frame_cache_drain(drained.len());
            // Never nest the per-CPU and global allocator locks. This keeps
            // refill, drain, stats, and interrupt-context frees free of a
            // cache/global lock-order cycle.
            let mut allocator = FRAME_ALLOCATOR.exclusive_access();
            for drained_ppn in drained {
                allocator.recycle(drained_ppn);
            }
        }
        perf::record_frame_dealloc(true, false, 0, 0);
        return;
    }

    let mut allocator = FRAME_ALLOCATOR.exclusive_access();
    allocator.recycle(ppn);
    let recycled_len = allocator.recycled.len();
    perf::record_frame_dealloc(true, false, 0, recycled_len);
}

pub fn frame_retain(ppn: PhysPageNum) -> bool {
    FRAME_REF_COUNTS.retain(ppn)
}

pub fn frame_ref_count(ppn: PhysPageNum) -> Option<usize> {
    FRAME_REF_COUNTS.ref_count(ppn)
}

pub fn frame_stats() -> (usize, usize) {
    let cached = FRAME_CACHES
        .iter()
        .map(|cache| cache.lock().pages.len())
        .sum::<usize>();
    let (total, global_free) = FRAME_ALLOCATOR.exclusive_access().stats();
    (total, global_free.saturating_add(cached))
}

fn alloc_frame_ppn() -> Option<PhysPageNum> {
    if let Some(cpu) = crate::cpu::try_current_id() {
        if let Some(ppn) = FRAME_CACHES[cpu].lock().pages.pop() {
            FRAME_REF_COUNTS.claim_free(ppn);
            record_frame_cache_hit();
            return Some(ppn);
        }

        let mut refill = Vec::with_capacity(FRAME_CACHE_BATCH);
        {
            // Reserve a bounded batch under the global lock. Claiming one
            // page, publishing the remainder to the CPU cache, and zeroing
            // the returned page all happen after this guard is released.
            let mut allocator = FRAME_ALLOCATOR.exclusive_access();
            for _ in 0..FRAME_CACHE_BATCH {
                let Some(ppn) = allocator.alloc() else {
                    break;
                };
                refill.push(ppn);
            }
        }
        record_frame_cache_refill(refill.len());
        let ppn = refill.pop()?;
        FRAME_REF_COUNTS.claim_free(ppn);
        if !refill.is_empty() {
            FRAME_CACHES[cpu].lock().pages.extend(refill);
        }
        return Some(ppn);
    }

    let ppn = FRAME_ALLOCATOR.exclusive_access().alloc()?;
    record_frame_cache_global_alloc();
    FRAME_REF_COUNTS.claim_free(ppn);
    Some(ppn)
}

#[cfg(feature = "perf-counters")]
fn record_frame_cache_hit() {
    FRAME_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(feature = "perf-counters"))]
fn record_frame_cache_hit() {}

#[cfg(feature = "perf-counters")]
fn record_frame_cache_refill(pages: usize) {
    FRAME_CACHE_REFILLS.fetch_add(1, Ordering::Relaxed);
    FRAME_CACHE_REFILL_PAGES.fetch_add(pages, Ordering::Relaxed);
}

#[cfg(not(feature = "perf-counters"))]
fn record_frame_cache_refill(_pages: usize) {}

#[cfg(feature = "perf-counters")]
fn record_frame_cache_local_free() {
    FRAME_CACHE_LOCAL_FREES.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(feature = "perf-counters"))]
fn record_frame_cache_local_free() {}

#[cfg(feature = "perf-counters")]
fn record_frame_cache_drain(pages: usize) {
    FRAME_CACHE_DRAINS.fetch_add(1, Ordering::Relaxed);
    FRAME_CACHE_DRAIN_PAGES.fetch_add(pages, Ordering::Relaxed);
}

#[cfg(not(feature = "perf-counters"))]
fn record_frame_cache_drain(_pages: usize) {}

#[cfg(feature = "perf-counters")]
fn record_frame_cache_global_alloc() {
    FRAME_CACHE_GLOBAL_ALLOCS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(feature = "perf-counters"))]
fn record_frame_cache_global_alloc() {}

#[cfg(feature = "perf-counters")]
pub(crate) fn frame_cache_stats() -> FrameCacheStats {
    FrameCacheStats {
        hits: FRAME_CACHE_HITS.load(Ordering::Relaxed),
        refills: FRAME_CACHE_REFILLS.load(Ordering::Relaxed),
        refill_pages: FRAME_CACHE_REFILL_PAGES.load(Ordering::Relaxed),
        local_frees: FRAME_CACHE_LOCAL_FREES.load(Ordering::Relaxed),
        drains: FRAME_CACHE_DRAINS.load(Ordering::Relaxed),
        drain_pages: FRAME_CACHE_DRAIN_PAGES.load(Ordering::Relaxed),
        global_allocs: FRAME_CACHE_GLOBAL_ALLOCS.load(Ordering::Relaxed),
    }
}
