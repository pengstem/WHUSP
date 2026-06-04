#![allow(dead_code)]

use super::{FrameTracker, PhysPageNum};
use crate::config::PAGE_SIZE;
use crate::fs::MountId;
use crate::sync::UPIntrFreeCell;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use lazy_static::*;

// Soft cap for ordinary read(2) cache pages. MAP_SHARED mmap pages are pinned
// and dirty-tracked separately, so this cap must not evict them.
pub(crate) const PAGE_CACHE_SOFT_MAX_PAGES: usize = 4096;

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
    pub(crate) page_index: usize,
}

impl PageCacheKey {
    /// Builds a cache key only for page-aligned file offsets.
    ///
    /// The current mmap path caches full file pages; partial-page offsets fall
    /// back to private fault frames.
    pub(crate) fn from_file_offset(id: PageCacheId, file_offset: usize) -> Option<Self> {
        if file_offset % PAGE_SIZE != 0 {
            return None;
        }
        Some(Self {
            id,
            page_index: file_offset / PAGE_SIZE,
        })
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct PageCacheLruEntry {
    stamp: usize,
    key: PageCacheKey,
}

pub(crate) struct PageCache {
    pages: BTreeMap<PageCacheKey, PageCachePage>,
    lru: BTreeSet<PageCacheLruEntry>,
    lru_clock: usize,
}

impl PageCache {
    pub(crate) fn new() -> Self {
        Self {
            pages: BTreeMap::new(),
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

    pub(crate) fn ensure_exec_icache_synced(&mut self, key: PageCacheKey) -> bool {
        let Some(page) = self.pages.get_mut(&key) else {
            return false;
        };
        if page.exec_icache_synced {
            return true;
        }
        crate::arch::mm::publish_pte_barrier();
        crate::arch::mm::instruction_barrier();
        page.exec_icache_synced = true;
        true
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

    /// Returns a cached frame and pins it for one additional mapping.
    pub(crate) fn get_and_inc_ref(&mut self, key: PageCacheKey) -> Option<PhysPageNum> {
        let (old_stamp, ppn) = {
            let page = self.pages.get_mut(&key)?;
            page.ref_count += 1;
            (page.lru_stamp, page.ppn())
        };
        let stamp = self.touch(key, Some(old_stamp));
        if let Some(page) = self.pages.get_mut(&key) {
            page.lru_stamp = stamp;
        }
        Some(ppn)
    }

    /// Inserts a freshly loaded file page or reuses an existing one.
    ///
    /// The returned PPN is pinned for the caller's mapping in both cases.
    pub(crate) fn insert_loaded_page_and_inc_ref(
        &mut self,
        key: PageCacheKey,
        frame: FrameTracker,
        file_size_at_load: usize,
    ) -> PhysPageNum {
        if let Some(page) = self.pages.get_mut(&key) {
            page.ref_count += 1;
            let old_stamp = page.lru_stamp;
            let ppn = page.ppn();
            let stamp = self.touch(key, Some(old_stamp));
            if let Some(page) = self.pages.get_mut(&key) {
                page.lru_stamp = stamp;
            }
            return ppn;
        }
        let mut page = PageCachePage::new(frame, key, file_size_at_load);
        page.ref_count = 1;
        let ppn = page.ppn();
        page.lru_stamp = self.touch(key, None);
        self.pages.insert(key, page);
        ppn
    }

    /// Drops one mapping reference without evicting the cached page.
    pub(crate) fn dec_ref(&mut self, key: PageCacheKey) {
        if let Some(page) = self.pages.get_mut(&key) {
            page.ref_count = page.ref_count.saturating_sub(1);
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
        let (old_stamp, len) = {
            let page = self.pages.get(&key)?;
            if page.ref_count != 0 || page.dirty || page_offset >= PAGE_SIZE {
                return None;
            }
            let len = len.min(PAGE_SIZE - page_offset).min(dst.len());
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
    ) -> usize {
        if let Some(page) = self.pages.get(&key) {
            let old_stamp = page.lru_stamp;
            let stamp = self.touch(key, Some(old_stamp));
            if let Some(page) = self.pages.get_mut(&key) {
                page.lru_stamp = stamp;
            }
            return 0;
        }

        let target_len = PAGE_CACHE_SOFT_MAX_PAGES.saturating_sub(1);
        let evicted = self.trim_clean_unpinned_to_len(target_len);
        if self.pages.len() >= PAGE_CACHE_SOFT_MAX_PAGES {
            return evicted;
        }

        let mut page = PageCachePage::new(frame, key, file_size_at_load);
        page.lru_stamp = self.touch(key, None);
        self.pages.insert(key, page);
        evicted
    }

    /// Drops clean unpinned ordinary-read pages for one file.
    pub(crate) fn invalidate_clean_unreferenced(&mut self, id: PageCacheId) -> (usize, usize) {
        let start = PageCacheKey { id, page_index: 0 };
        let end = PageCacheKey {
            id,
            page_index: usize::MAX,
        };
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

lazy_static! {
    pub(crate) static ref PAGE_CACHE: UPIntrFreeCell<PageCache> =
        unsafe { UPIntrFreeCell::new(PageCache::new()) };
}
