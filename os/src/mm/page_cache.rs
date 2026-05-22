#![allow(dead_code)]

use super::{FrameTracker, PhysPageNum};
use crate::config::PAGE_SIZE;
use crate::fs::MountId;
use crate::sync::UPIntrFreeCell;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use lazy_static::*;

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
    pub(crate) file_size_at_load: usize,
    pub(crate) dirty: bool,
    pub(crate) ref_count: usize,
}

impl PageCachePage {
    pub(crate) fn new(frame: FrameTracker, key: PageCacheKey, file_size_at_load: usize) -> Self {
        Self {
            frame,
            key,
            file_size_at_load,
            dirty: false,
            ref_count: 0,
        }
    }

    pub(crate) fn ppn(&self) -> PhysPageNum {
        self.frame.ppn
    }
}

pub(crate) struct PageCache {
    pages: BTreeMap<PageCacheKey, PageCachePage>,
}

impl PageCache {
    pub(crate) fn new() -> Self {
        Self {
            pages: BTreeMap::new(),
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

    /// Returns a cached frame and pins it for one additional mapping.
    pub(crate) fn get_and_inc_ref(&mut self, key: PageCacheKey) -> Option<PhysPageNum> {
        let page = self.pages.get_mut(&key)?;
        page.ref_count += 1;
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
    ) -> PhysPageNum {
        if let Some(page) = self.pages.get_mut(&key) {
            page.ref_count += 1;
            return page.ppn();
        }
        let mut page = PageCachePage::new(frame, key, file_size_at_load);
        page.ref_count = 1;
        let ppn = page.ppn();
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
            self.pages.remove(&key)
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
        &self,
        key: PageCacheKey,
        page_offset: usize,
        len: usize,
        dst: &mut [u8],
    ) -> Option<usize> {
        let page = self.pages.get(&key)?;
        if page.ref_count != 0 || page.dirty || page_offset >= PAGE_SIZE {
            return None;
        }
        let len = len.min(PAGE_SIZE - page_offset).min(dst.len());
        dst[..len].copy_from_slice(&page.ppn().get_bytes_array()[page_offset..page_offset + len]);
        Some(len)
    }

    /// Inserts a clean unpinned page for ordinary read(2) reuse.
    pub(crate) fn insert_read_cache_page(
        &mut self,
        key: PageCacheKey,
        frame: FrameTracker,
        file_size_at_load: usize,
    ) {
        self.pages
            .entry(key)
            .or_insert_with(|| PageCachePage::new(frame, key, file_size_at_load));
    }

    /// Drops clean unpinned ordinary-read pages for one file.
    pub(crate) fn invalidate_clean_unreferenced(&mut self, id: PageCacheId) -> usize {
        let before = self.pages.len();
        self.pages
            .retain(|key, page| key.id != id || page.ref_count != 0 || page.dirty);
        before.saturating_sub(self.pages.len())
    }

    /// Marks a shared mmap page dirty after the first write fault.
    pub(crate) fn mark_dirty(&mut self, key: PageCacheKey) -> bool {
        let Some(page) = self.pages.get_mut(&key) else {
            return false;
        };
        page.dirty = true;
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
