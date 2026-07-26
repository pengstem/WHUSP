#![allow(dead_code)]

use super::{FrameTracker, PhysPageNum};
use crate::config::PAGE_SIZE;
use crate::fs::MountId;
use crate::perf;
use crate::sync::UPIntrFreeCell;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use lazy_static::*;

// CONTEXT: This is a bounded transition toward watermark-driven reclaim. The
// clean Rust workload runs with an explicitly frozen 8 GiB guest, so retaining
// at most 256 MiB of clean file pages lets read(2), exec, and readonly private
// mmap share their working set without allowing unbounded cache growth.
pub(crate) const PAGE_CACHE_SOFT_MAX_PAGES: usize = 65_536;
const SHARED_MMAP_GENERATION: usize = usize::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct PageCacheId {
    pub(crate) mount_id: MountId,
    pub(crate) ino: u32,
}

impl PageCacheId {
    pub(crate) fn new(mount_id: MountId, ino: u32) -> Self {
        Self { mount_id, ino }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct PageCacheKey {
    pub(crate) id: PageCacheId,
    pub(crate) generation: usize,
    pub(crate) page_index: usize,
}

impl PageCacheKey {
    /// Builds a cache key only for page-aligned file offsets.
    ///
    /// The current mmap path caches full file pages; partial-page offsets fall
    /// back to private fault frames.
    pub(crate) fn from_file_offset(
        id: PageCacheId,
        generation: usize,
        file_offset: usize,
    ) -> Option<Self> {
        if file_offset % PAGE_SIZE != 0 {
            return None;
        }
        Some(Self {
            id,
            generation,
            page_index: file_offset / PAGE_SIZE,
        })
    }

    pub(crate) fn for_page(id: PageCacheId, generation: usize, page_index: usize) -> Self {
        Self {
            id,
            generation,
            page_index,
        }
    }

    /// Returns the byte offset represented by this file page key.
    pub(crate) fn file_offset(self) -> usize {
        self.page_index * PAGE_SIZE
    }
}

pub(crate) struct PageCachePage {
    pub(crate) frame: FrameTracker,
    pub(crate) key: PageCacheKey,
    // Size observed when this page was loaded; callers pass the mmap snapshot
    // that already bounded fault-time EOF reads before insertion.
    pub(crate) file_size_at_load: usize,
    // Dirty pages belong to MAP_SHARED writeback and are not soft-LRU victims.
    pub(crate) dirty: bool,
    // Active page-table mappings, not Arc references. Nonzero pins the frame.
    pub(crate) ref_count: usize,
    exec_icache_synced: bool,
    lru_stamp: usize,
}

impl PageCachePage {
    pub(crate) fn new(frame: FrameTracker, key: PageCacheKey, file_size_at_load: usize) -> Self {
        Self {
            frame,
            key,
            file_size_at_load,
            dirty: false,
            ref_count: 0,
            exec_icache_synced: false,
            lru_stamp: 0,
        }
    }

    pub(crate) fn ppn(&self) -> PhysPageNum {
        self.frame.ppn
    }

    fn ensure_exec_icache_synced(&mut self) {
        if self.exec_icache_synced {
            return;
        }
        crate::arch::mm::publish_pte_barrier();
        crate::arch::mm::instruction_barrier();
        self.exec_icache_synced = true;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct PageCacheLruEntry {
    stamp: usize,
    key: PageCacheKey,
}

pub(crate) struct PageCache {
    pages: BTreeMap<PageCacheKey, PageCachePage>,
    generations: BTreeMap<PageCacheId, PageCacheGeneration>,
    lru: BTreeSet<PageCacheLruEntry>,
    lru_clock: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct PageCacheGeneration {
    value: usize,
    active_mutations: usize,
}

/// Keeps one file-content mutation inside an unstable generation epoch.
/// Nested or concurrent writers share the epoch; only the final guard makes a
/// new stable generation visible to readers.
pub(crate) struct PageCacheMutationGuard {
    id: PageCacheId,
}

impl Drop for PageCacheMutationGuard {
    fn drop(&mut self) {
        PAGE_CACHE.exclusive_access().end_mutation(self.id);
    }
}

impl PageCache {
    pub(crate) fn new() -> Self {
        Self {
            pages: BTreeMap::new(),
            generations: BTreeMap::new(),
            lru: BTreeSet::new(),
            lru_clock: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.pages.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    pub(crate) fn contains(&self, key: PageCacheKey) -> bool {
        self.pages.contains_key(&key)
    }

    pub(crate) fn current_generation(&self, id: PageCacheId) -> usize {
        self.generations
            .get(&id)
            .map(|generation| generation.value)
            .unwrap_or(0)
    }

    pub(crate) fn current_stable_generation(&self, id: PageCacheId) -> Option<usize> {
        match self.generations.get(&id) {
            Some(generation) if generation.active_mutations != 0 => None,
            Some(generation) => Some(generation.value),
            None => Some(0),
        }
    }

    pub(crate) fn current_key_from_file_offset(
        &self,
        id: PageCacheId,
        file_offset: usize,
    ) -> Option<PageCacheKey> {
        PageCacheKey::from_file_offset(id, self.current_stable_generation(id)?, file_offset)
    }

    /// MAP_SHARED keeps one compatibility namespace across file-data epochs.
    ///
    /// The current kernel has no reverse-map invalidation for already mapped
    /// shared pages. Keeping their exact key stable avoids splitting one file
    /// page into multiple writable frames while clean private/exec mappings use
    /// versioned keys. A broader shared-dirty coherency rewrite is separate.
    pub(crate) fn mmap_key_from_file_offset(
        &self,
        id: PageCacheId,
        file_offset: usize,
        shared: bool,
    ) -> Option<(PageCacheKey, usize)> {
        let generation = self.current_stable_generation(id)?;
        let key = PageCacheKey::from_file_offset(
            id,
            if shared {
                SHARED_MMAP_GENERATION
            } else {
                generation
            },
            file_offset,
        )?;
        Some((key, generation))
    }

    pub(crate) fn is_current_key(&self, key: PageCacheKey) -> bool {
        self.current_stable_generation(key.id) == Some(key.generation)
    }

    pub(crate) fn is_usable_mmap_key(
        &self,
        key: PageCacheKey,
        shared: bool,
        observed_generation: usize,
    ) -> bool {
        if self.current_stable_generation(key.id) != Some(observed_generation) {
            return false;
        }
        if shared {
            key.generation == SHARED_MMAP_GENERATION
        } else {
            key.generation == observed_generation
        }
    }

    fn begin_mutation(&mut self, id: PageCacheId) -> (usize, usize) {
        let first_mutation = {
            let generation = self.generations.entry(id).or_default();
            let first_mutation = generation.active_mutations == 0;
            if first_mutation {
                generation.value = generation
                    .value
                    .checked_add(1)
                    .expect("page-cache inode generation exhausted");
            }
            generation.active_mutations = generation
                .active_mutations
                .checked_add(1)
                .expect("page-cache inode mutation nesting exhausted");
            first_mutation
        };
        if !first_mutation {
            return (0, 0);
        }
        perf::record_page_cache_generation_epoch_begin();

        // The generation is intentionally unstable while the first mutation
        // is active. Drop only unpinned clean pages; pinned old versions keep
        // their exact key until their final mapping reference retires.
        let start = PageCacheKey::for_page(id, 0, 0);
        let end = PageCacheKey::for_page(id, usize::MAX, usize::MAX);
        let mut scanned = 0usize;
        let victims: Vec<_> = self
            .pages
            .range(start..=end)
            .filter_map(|(key, page)| {
                scanned += 1;
                (page.ref_count == 0 && !page.dirty).then_some((*key, page.lru_stamp))
            })
            .collect();
        let removed = victims.len();
        for (key, stamp) in victims {
            self.pages.remove(&key);
            self.lru.remove(&PageCacheLruEntry { stamp, key });
        }
        (removed, scanned)
    }

    fn end_mutation(&mut self, id: PageCacheId) {
        let finished_epoch = {
            let generation = self
                .generations
                .get_mut(&id)
                .expect("page-cache mutation guard lost its generation");
            assert!(generation.active_mutations > 0);
            generation.active_mutations -= 1;
            if generation.active_mutations == 0 {
                generation.value = generation
                    .value
                    .checked_add(1)
                    .expect("page-cache inode generation exhausted");
                true
            } else {
                false
            }
        };
        if finished_epoch {
            perf::record_page_cache_generation_epoch_finish();
        }
    }

    fn touch(&mut self, key: PageCacheKey, old_stamp: Option<usize>) -> usize {
        if let Some(stamp) = old_stamp {
            self.lru.remove(&PageCacheLruEntry { stamp, key });
        }
        self.lru_clock = self.lru_clock.wrapping_add(1);
        let stamp = self.lru_clock;
        self.lru.insert(PageCacheLruEntry { stamp, key });
        stamp
    }

    fn evict_one_clean_unpinned(&mut self) -> bool {
        let victim = self.lru.iter().copied().find(|entry| {
            self.pages.get(&entry.key).is_some_and(|page| {
                page.lru_stamp == entry.stamp && page.ref_count == 0 && !page.dirty
            })
        });
        let Some(victim) = victim else {
            return false;
        };
        self.lru.remove(&victim);
        self.pages.remove(&victim.key).is_some()
    }

    fn trim_clean_unpinned_to_len(&mut self, max_len: usize) -> usize {
        let mut evicted = 0usize;
        while self.pages.len() > max_len {
            if !self.evict_one_clean_unpinned() {
                break;
            }
            evicted += 1;
        }
        evicted
    }

    /// Pins the exact version already owned by another mapping.
    ///
    /// Fork uses this for old generations as well as the current one; a stale
    /// key remains valid only while an existing mapping still owns its pin.
    pub(crate) fn pin_existing_exact(&mut self, key: PageCacheKey) -> Option<PhysPageNum> {
        let page = self.pages.get_mut(&key)?;
        page.ref_count += 1;
        Some(page.ppn())
    }

    pub(crate) fn get_and_inc_ref_for_mmap(
        &mut self,
        key: PageCacheKey,
        exec_fault: bool,
        shared: bool,
        observed_generation: usize,
    ) -> Option<PhysPageNum> {
        if !self.is_usable_mmap_key(key, shared, observed_generation) {
            return None;
        }
        // PERF: mmap exec faults are mostly page-cache hits. Fold the icache
        // sync check into the same tree lookup used to pin the cached frame.
        let page = self.pages.get_mut(&key)?;
        page.ref_count += 1;
        if exec_fault {
            page.ensure_exec_icache_synced();
        }
        Some(page.ppn())
    }

    /// Inserts a freshly loaded file page or reuses an existing one.
    ///
    /// The returned PPN is pinned for the caller's mapping in both cases.
    pub(crate) fn insert_loaded_page_and_inc_ref(
        &mut self,
        key: PageCacheKey,
        frame: FrameTracker,
        file_size_at_load: usize,
    ) -> Option<PhysPageNum> {
        if !self.is_current_key(key) {
            return None;
        }
        if let Some(page) = self.pages.get_mut(&key) {
            page.ref_count += 1;
            return Some(page.ppn());
        }
        let mut page = PageCachePage::new(frame, key, file_size_at_load);
        page.ref_count = 1;
        let ppn = page.ppn();
        page.lru_stamp = self.touch(key, None);
        self.pages.insert(key, page);
        Some(ppn)
    }

    pub(crate) fn insert_loaded_page_and_inc_ref_for_mmap(
        &mut self,
        key: PageCacheKey,
        frame: FrameTracker,
        file_size_at_load: usize,
        exec_fault: bool,
        shared: bool,
        observed_generation: usize,
    ) -> Option<PhysPageNum> {
        if !self.is_usable_mmap_key(key, shared, observed_generation) {
            return None;
        }
        if let Some(page) = self.pages.get_mut(&key) {
            page.ref_count += 1;
            if exec_fault {
                page.ensure_exec_icache_synced();
            }
            return Some(page.ppn());
        }
        let target_len = PAGE_CACHE_SOFT_MAX_PAGES.saturating_sub(1);
        self.trim_clean_unpinned_to_len(target_len);
        let mut page = PageCachePage::new(frame, key, file_size_at_load);
        page.ref_count = 1;
        if exec_fault {
            page.ensure_exec_icache_synced();
        }
        let ppn = page.ppn();
        page.lru_stamp = self.touch(key, None);
        self.pages.insert(key, page);
        Some(ppn)
    }

    /// Drops one mapping reference without evicting the cached page.
    pub(crate) fn dec_ref(&mut self, key: PageCacheKey) {
        let current_generation = self.current_generation(key.id);
        let remove_stale = if let Some(page) = self.pages.get_mut(&key) {
            page.ref_count = page.ref_count.saturating_sub(1);
            page.ref_count == 0 && !page.dirty && key.generation != current_generation
        } else {
            false
        };
        if remove_stale && let Some(page) = self.pages.remove(&key) {
            self.lru.remove(&PageCacheLruEntry {
                stamp: page.lru_stamp,
                key,
            });
        }
        if self.pages.len() > PAGE_CACHE_SOFT_MAX_PAGES {
            self.trim_clean_unpinned_to_len(PAGE_CACHE_SOFT_MAX_PAGES);
        }
    }

    /// Drops one mapping reference and removes the page when it is unreferenced.
    pub(crate) fn dec_ref_and_take_if_unused(
        &mut self,
        key: PageCacheKey,
    ) -> Option<PageCachePage> {
        let page = self.pages.get_mut(&key)?;
        page.ref_count = page.ref_count.saturating_sub(1);
        if page.ref_count == 0 {
            let page = self.pages.remove(&key)?;
            self.lru.remove(&PageCacheLruEntry {
                stamp: page.lru_stamp,
                key,
            });
            Some(page)
        } else {
            None
        }
    }

    pub(crate) fn copy_page_data(&self, key: PageCacheKey, len: usize) -> Option<Vec<u8>> {
        let page = self.pages.get(&key)?;
        let len = len.min(PAGE_SIZE);
        Some(page.ppn().get_bytes_array()[..len].to_vec())
    }

    /// Returns data from a page that was cached only for ordinary read(2).
    ///
    /// MAP_SHARED mmap pages keep a nonzero refcount while mapped and have
    /// separate dirty/writeback rules, so the ordinary read cache avoids using
    /// those pages until the broader page-cache coherency model is unified.
    pub(crate) fn copy_read_cache_page_data(
        &mut self,
        key: PageCacheKey,
        page_offset: usize,
        len: usize,
        dst: &mut [u8],
    ) -> Option<usize> {
        if !self.is_current_key(key) {
            return None;
        }
        let (old_stamp, len) = {
            let page = self.pages.get(&key)?;
            if page.dirty || page_offset >= PAGE_SIZE {
                return None;
            }
            let page_valid_len = page
                .file_size_at_load
                .saturating_sub(key.file_offset())
                .min(PAGE_SIZE);
            if page_offset >= page_valid_len {
                return Some(0);
            }
            let len = len
                .min(page_valid_len - page_offset)
                .min(PAGE_SIZE - page_offset)
                .min(dst.len());
            dst[..len]
                .copy_from_slice(&page.ppn().get_bytes_array()[page_offset..page_offset + len]);
            (page.lru_stamp, len)
        };
        let stamp = self.touch(key, Some(old_stamp));
        if let Some(page) = self.pages.get_mut(&key) {
            page.lru_stamp = stamp;
        }
        Some(len)
    }

    /// Inserts a clean unpinned page for ordinary read(2) reuse.
    pub(crate) fn insert_read_cache_page(
        &mut self,
        key: PageCacheKey,
        frame: FrameTracker,
        file_size_at_load: usize,
    ) -> (usize, bool) {
        if !self.is_current_key(key) {
            return (0, false);
        }
        if let Some(page) = self.pages.get(&key) {
            let old_stamp = page.lru_stamp;
            let stamp = self.touch(key, Some(old_stamp));
            if let Some(page) = self.pages.get_mut(&key) {
                page.lru_stamp = stamp;
            }
            return (0, false);
        }

        let target_len = PAGE_CACHE_SOFT_MAX_PAGES.saturating_sub(1);
        let evicted = self.trim_clean_unpinned_to_len(target_len);
        if self.pages.len() >= PAGE_CACHE_SOFT_MAX_PAGES {
            perf::record_page_cache_capacity_reject();
            return (evicted, false);
        }

        let mut page = PageCachePage::new(frame, key, file_size_at_load);
        page.lru_stamp = self.touch(key, None);
        self.pages.insert(key, page);
        (evicted, true)
    }

    /// Marks a shared mmap page dirty after the first write fault.
    pub(crate) fn mark_dirty(&mut self, key: PageCacheKey) -> bool {
        let Some(page) = self.pages.get_mut(&key) else {
            return false;
        };
        page.dirty = true;
        page.exec_icache_synced = false;
        true
    }

    pub(crate) fn copy_dirty_page_data(&self, key: PageCacheKey, len: usize) -> Option<Vec<u8>> {
        let page = self.pages.get(&key)?;
        if !page.dirty {
            return None;
        }
        let len = len.min(PAGE_SIZE);
        Some(page.ppn().get_bytes_array()[..len].to_vec())
    }

    /// Takes a dirty snapshot for writeback and clears the dirty bit.
    pub(crate) fn take_dirty_page_data(
        &mut self,
        key: PageCacheKey,
        len: usize,
    ) -> Option<Vec<u8>> {
        let page = self.pages.get_mut(&key)?;
        if !page.dirty {
            return None;
        }
        page.dirty = false;
        let len = len.min(PAGE_SIZE);
        Some(page.ppn().get_bytes_array()[..len].to_vec())
    }
}

pub(crate) fn begin_page_cache_mutation(id: PageCacheId) -> (PageCacheMutationGuard, usize, usize) {
    let (removed, scanned) = PAGE_CACHE.exclusive_access().begin_mutation(id);
    (PageCacheMutationGuard { id }, removed, scanned)
}

lazy_static! {
    pub(crate) static ref PAGE_CACHE: UPIntrFreeCell<PageCache> =
        unsafe { UPIntrFreeCell::new(PageCache::new()) };
}
