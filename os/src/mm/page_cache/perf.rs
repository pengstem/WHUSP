use core::sync::atomic::{AtomicUsize, Ordering};

static PAGE_CACHE_CLEAN_EVICTIONS: AtomicUsize = AtomicUsize::new(0);
static PAGE_CACHE_GENERATION_EPOCHS_BEGUN: AtomicUsize = AtomicUsize::new(0);
static PAGE_CACHE_GENERATION_EPOCHS_FINISHED: AtomicUsize = AtomicUsize::new(0);
static PAGE_CACHE_GENERATION_RETRIES: AtomicUsize = AtomicUsize::new(0);
static PAGE_CACHE_STALE_FILL_DROPS: AtomicUsize = AtomicUsize::new(0);
static PAGE_CACHE_STALE_INSTALL_RETRIES: AtomicUsize = AtomicUsize::new(0);
static PAGE_CACHE_CAPACITY_REJECTS: AtomicUsize = AtomicUsize::new(0);
static MMAP_CLEAN_PAGE_CACHE_HITS: AtomicUsize = AtomicUsize::new(0);
static MMAP_CLEAN_PAGE_CACHE_MISSES: AtomicUsize = AtomicUsize::new(0);
static MMAP_CLEAN_PAGE_CACHE_FILLS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PageCachePerfSnapshot {
    pub(crate) clean_evictions: usize,
    pub(crate) generation_epochs_begun: usize,
    pub(crate) generation_epochs_finished: usize,
    pub(crate) generation_retries: usize,
    pub(crate) stale_fill_drops: usize,
    pub(crate) stale_install_retries: usize,
    pub(crate) capacity_rejects: usize,
    pub(crate) clean_mmap_hits: usize,
    pub(crate) clean_mmap_misses: usize,
    pub(crate) clean_mmap_fills: usize,
}

pub(crate) fn record_clean_eviction(pages: usize) {
    PAGE_CACHE_CLEAN_EVICTIONS.fetch_add(pages, Ordering::Relaxed);
}

pub(crate) fn record_generation_epoch_begin() {
    PAGE_CACHE_GENERATION_EPOCHS_BEGUN.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_generation_epoch_finish() {
    PAGE_CACHE_GENERATION_EPOCHS_FINISHED.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_generation_retry() {
    PAGE_CACHE_GENERATION_RETRIES.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_stale_fill_drop(pages: usize) {
    PAGE_CACHE_STALE_FILL_DROPS.fetch_add(pages, Ordering::Relaxed);
}

pub(crate) fn record_stale_install_retry() {
    PAGE_CACHE_STALE_INSTALL_RETRIES.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_capacity_reject() {
    PAGE_CACHE_CAPACITY_REJECTS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_clean_mmap(hit: bool) {
    if hit {
        MMAP_CLEAN_PAGE_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
    } else {
        MMAP_CLEAN_PAGE_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn record_clean_mmap_fill() {
    MMAP_CLEAN_PAGE_CACHE_FILLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn snapshot() -> PageCachePerfSnapshot {
    PageCachePerfSnapshot {
        clean_evictions: PAGE_CACHE_CLEAN_EVICTIONS.load(Ordering::Relaxed),
        generation_epochs_begun: PAGE_CACHE_GENERATION_EPOCHS_BEGUN.load(Ordering::Relaxed),
        generation_epochs_finished: PAGE_CACHE_GENERATION_EPOCHS_FINISHED.load(Ordering::Relaxed),
        generation_retries: PAGE_CACHE_GENERATION_RETRIES.load(Ordering::Relaxed),
        stale_fill_drops: PAGE_CACHE_STALE_FILL_DROPS.load(Ordering::Relaxed),
        stale_install_retries: PAGE_CACHE_STALE_INSTALL_RETRIES.load(Ordering::Relaxed),
        capacity_rejects: PAGE_CACHE_CAPACITY_REJECTS.load(Ordering::Relaxed),
        clean_mmap_hits: MMAP_CLEAN_PAGE_CACHE_HITS.load(Ordering::Relaxed),
        clean_mmap_misses: MMAP_CLEAN_PAGE_CACHE_MISSES.load(Ordering::Relaxed),
        clean_mmap_fills: MMAP_CLEAN_PAGE_CACHE_FILLS.load(Ordering::Relaxed),
    }
}
