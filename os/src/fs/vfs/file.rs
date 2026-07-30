use super::super::dentry_cache;
use super::super::devfs;
use super::super::dirent::{DT_DIR, RawDirEntry, write_dir_entries_with_offset_base};
use super::super::inode::{OpenFlags, link_node_in};
use super::super::inode_state::{self, DIRTY_REGULAR_FILES, DirtyFileCache, DirtyPage};
use super::super::mount::{
    MountId, MountNamespaceId, MountedBackendLease, mount_any_nosymfollow, mount_exists,
    mount_is_devfs, mount_is_noatime, mount_is_nodev, mount_is_nodiratime, mount_is_nosymfollow,
    mount_is_read_only, mount_supports_dirty_writeback, mount_supports_page_cache,
    mounted_backend_lease, release_inode_from_drop, release_inode_from_drop_with_lease,
    retain_inode, retain_inode_with_lease, stat_basic_cached,
    stat_basic_cached_with_state_and_lease, stat_full_cached,
    stat_full_cached_with_state_and_lease, synthetic_children_for_dir, with_mount,
};
use super::super::named_fifo::open_named_fifo;
use super::super::path::{PathContext, WorkingDir};
use super::super::status_flags::StatusFlagsCell;
use super::super::{
    FS_APPEND_FL, FS_IMMUTABLE_FL, File, FileStat, FileTimestamp, S_IFBLK, S_IFCHR, S_IFDIR,
    S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK, SeekWhence,
};
use super::path::{self as vfs_path, LookupMode, VfsOpenTarget};
use super::{
    BackendDirectorySnapshot, BackendOp, FsError, FsNodeKind, FsResult, VfsNodeId, VfsPath,
};
use crate::config::PAGE_SIZE;
use crate::mm::{
    UserBuffer, frame_alloc, frame_alloc_contiguous_uninit, frame_alloc_uninit,
    page_cache::{
        PAGE_CACHE, PageCacheId, PageCacheKey, PageCacheLoadGate, PageCacheMutationGuard,
        ReadCacheLoadReservation, begin_page_cache_mutation,
    },
};
use crate::perf;
use crate::sync::SleepMutex;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;

// Bound each backend write while a shared file offset lock is held; large user
// buffers still progress in order without monopolizing one mount backend.
const VFS_WRITE_CHUNK_SIZE: usize = 64 * 1024;
const VFS_READ_CHUNK_SIZE: usize = 64 * 1024;
const VFS_READ_ALL_CHUNK_SIZE: usize = 64 * 1024;
const VFS_DIRENT_SCRATCH_MAX: usize = 4 * 1024;
// CONTEXT: Raised to 8 MiB so that iozone 4 MiB files can use the small‑read cache
// and the page cache instead of falling through to backend on every read.
const VFS_READ_CACHE_MAX_FILE_SIZE: usize = 8 * 1024 * 1024;
const VFS_SMALL_READ_CACHE_MIN_FILE_SIZE: usize = 64 * 1024;
// CONTEXT: Eight 8 MiB shards bound the cache at 64 MiB. This preserves the
// existing per-file eligibility limit while allowing eight independent 4 MiB
// iozone files to remain hot without one global copy lock.
const VFS_SMALL_READ_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const VFS_SMALL_READ_CACHE_SHARDS: usize = 8;
const VFS_SMALL_READ_CACHE_SHARD_MAX_BYTES: usize =
    VFS_SMALL_READ_CACHE_MAX_BYTES / VFS_SMALL_READ_CACHE_SHARDS;
const VFS_READ_CACHE_READAHEAD_PAGES: usize = 6;
const VFS_DIRTY_WRITEBACK_MAX_WRITE_SIZE: usize = 64 * 1024;
const VFS_DIRTY_WRITEBACK_MAX_PAGES: usize = 4096;
const MODE_PERMISSIONS_MASK: u32 = 0o7777;
const MODE_SETGID: u32 = 0o2000;
const TMPFILE_CREATE_ATTEMPTS: usize = 64;
const SEEK_SCAN_MIN_BLOCK_SIZE: usize = 1;
// Synthetic mountpoint entries live in a high offset range so they cannot
// collide with real backend dirent offsets returned by the filesystem.
const SYNTHETIC_DIRENT_OFFSET_BASE: u64 = 1 << 60;

static TMPFILE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

lazy_static! {
    static ref SMALL_REGULAR_READ_FILES: SmallRegularReadCaches = SmallRegularReadCaches::new();
}

#[cfg(feature = "perf-counters")]
lazy_static! {
    static ref DIRTY_WRITEBACK_COUNTERS: SleepMutex<DirtyWritebackCounters> =
        SleepMutex::new(DirtyWritebackCounters::new());
}

#[derive(Debug)]
struct SmallRegularReadCache {
    data: Vec<u8>,
}

struct SmallRegularReadCacheShard {
    files: BTreeMap<VfsNodeId, SmallRegularReadCache>,
    bytes: usize,
}

struct SmallRegularReadCaches {
    shards: [SleepMutex<SmallRegularReadCacheShard>; VFS_SMALL_READ_CACHE_SHARDS],
}

impl SmallRegularReadCache {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        if offset >= self.data.len() {
            return 0;
        }
        let len = buf.len().min(self.data.len() - offset);
        buf[..len].copy_from_slice(&self.data[offset..offset + len]);
        len
    }
}

impl SmallRegularReadCacheShard {
    fn new() -> Self {
        Self {
            files: BTreeMap::new(),
            bytes: 0,
        }
    }

    fn remove(&mut self, node: VfsNodeId) {
        if let Some(cache) = self.files.remove(&node) {
            self.bytes = self.bytes.saturating_sub(cache.data.len());
        }
    }

    fn insert(&mut self, node: VfsNodeId, cache: SmallRegularReadCache) {
        let cache_bytes = cache.data.len();
        if cache_bytes > VFS_SMALL_READ_CACHE_SHARD_MAX_BYTES {
            return;
        }
        self.remove(node);
        if self.bytes.saturating_add(cache_bytes) > VFS_SMALL_READ_CACHE_SHARD_MAX_BYTES {
            self.files.clear();
            self.bytes = 0;
        }
        self.bytes += cache_bytes;
        self.files.insert(node, cache);
    }
}

impl SmallRegularReadCaches {
    fn new() -> Self {
        Self {
            shards: core::array::from_fn(|_| SleepMutex::new(SmallRegularReadCacheShard::new())),
        }
    }

    fn shard_index(node: VfsNodeId) -> usize {
        debug_assert!(VFS_SMALL_READ_CACHE_SHARDS.is_power_of_two());
        (node.ino as usize ^ node.mount_id.0.rotate_left(8)) & (VFS_SMALL_READ_CACHE_SHARDS - 1)
    }

    fn read_at(&self, node: VfsNodeId, offset: usize, buf: &mut [u8]) -> Option<usize> {
        let shard = self.shards[Self::shard_index(node)].lock();
        shard
            .files
            .get(&node)
            .map(|cache| cache.read_at(offset, buf))
    }

    fn remove(&self, node: VfsNodeId) {
        self.shards[Self::shard_index(node)].lock().remove(node);
    }

    fn insert(&self, node: VfsNodeId, cache: SmallRegularReadCache) {
        self.shards[Self::shard_index(node)]
            .lock()
            .insert(node, cache);
    }
}

#[cfg(feature = "perf-counters")]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DirtyWritebackStats {
    pub(crate) dirty_files: usize,
    pub(crate) dirty_pages: usize,
    pub(crate) dirty_bytes: usize,
    pub(crate) cached_writes: usize,
    pub(crate) cached_pages: usize,
    pub(crate) cached_bytes: usize,
    pub(crate) fallback_writes: usize,
    pub(crate) flush_calls: usize,
    pub(crate) flushed_pages: usize,
    pub(crate) flushed_bytes: usize,
    pub(crate) dirty_pages_peak: usize,
    pub(crate) dirty_bytes_peak: usize,
    pub(crate) pressure_flushes: usize,
    pub(crate) pressure_flushed_pages: usize,
    pub(crate) pressure_flushed_bytes: usize,
    pub(crate) pressure_flush_failures: usize,
}

#[cfg(feature = "perf-counters")]
#[derive(Debug)]
struct DirtyWritebackCounters {
    cached_writes: usize,
    cached_pages: usize,
    cached_bytes: usize,
    fallback_writes: usize,
    flush_calls: usize,
    flushed_pages: usize,
    flushed_bytes: usize,
    dirty_pages_peak: usize,
    pressure_flushes: usize,
    pressure_flushed_pages: usize,
    pressure_flushed_bytes: usize,
    pressure_flush_failures: usize,
}

#[cfg(feature = "perf-counters")]
impl DirtyWritebackCounters {
    const fn new() -> Self {
        Self {
            cached_writes: 0,
            cached_pages: 0,
            cached_bytes: 0,
            fallback_writes: 0,
            flush_calls: 0,
            flushed_pages: 0,
            flushed_bytes: 0,
            dirty_pages_peak: 0,
            pressure_flushes: 0,
            pressure_flushed_pages: 0,
            pressure_flushed_bytes: 0,
            pressure_flush_failures: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirtyCacheWriteResult {
    Cached(usize),
    NeedsPressureFlush,
    Fallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirtyFlushReason {
    Explicit,
    Pressure,
}

#[cfg(feature = "perf-counters")]
fn record_dirty_cache_write(pages: usize, bytes: usize) {
    let mut counters = DIRTY_WRITEBACK_COUNTERS.lock();
    counters.cached_writes = counters.cached_writes.saturating_add(1);
    counters.cached_pages = counters.cached_pages.saturating_add(pages);
    counters.cached_bytes = counters.cached_bytes.saturating_add(bytes);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_dirty_cache_write(_pages: usize, _bytes: usize) {}

#[cfg(feature = "perf-counters")]
fn record_dirty_cache_fallback() {
    let mut counters = DIRTY_WRITEBACK_COUNTERS.lock();
    counters.fallback_writes = counters.fallback_writes.saturating_add(1);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_dirty_cache_fallback() {}

#[cfg(feature = "perf-counters")]
fn record_dirty_cache_peak(dirty_pages: usize) {
    let mut counters = DIRTY_WRITEBACK_COUNTERS.lock();
    counters.dirty_pages_peak = counters.dirty_pages_peak.max(dirty_pages);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_dirty_cache_peak(_dirty_pages: usize) {}

#[cfg(feature = "perf-counters")]
fn record_dirty_cache_flush(reason: DirtyFlushReason, pages: usize, bytes: usize) {
    let mut counters = DIRTY_WRITEBACK_COUNTERS.lock();
    counters.flush_calls = counters.flush_calls.saturating_add(1);
    counters.flushed_pages = counters.flushed_pages.saturating_add(pages);
    counters.flushed_bytes = counters.flushed_bytes.saturating_add(bytes);
    if reason == DirtyFlushReason::Pressure {
        counters.pressure_flushes = counters.pressure_flushes.saturating_add(1);
        counters.pressure_flushed_pages = counters.pressure_flushed_pages.saturating_add(pages);
        counters.pressure_flushed_bytes = counters.pressure_flushed_bytes.saturating_add(bytes);
    }
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_dirty_cache_flush(_reason: DirtyFlushReason, _pages: usize, _bytes: usize) {}

#[cfg(feature = "perf-counters")]
fn record_dirty_cache_flush_failure(reason: DirtyFlushReason) {
    if reason != DirtyFlushReason::Pressure {
        return;
    }
    let mut counters = DIRTY_WRITEBACK_COUNTERS.lock();
    counters.pressure_flush_failures = counters.pressure_flush_failures.saturating_add(1);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_dirty_cache_flush_failure(_reason: DirtyFlushReason) {}

#[cfg(feature = "perf-counters")]
pub(crate) fn dirty_writeback_stats_snapshot() -> DirtyWritebackStats {
    let dirty = DIRTY_REGULAR_FILES.lock();
    let counters = DIRTY_WRITEBACK_COUNTERS.lock();
    let dirty_files = dirty.len();
    let dirty_pages = dirty.values().map(|cache| cache.pages.len()).sum::<usize>();
    DirtyWritebackStats {
        dirty_files,
        dirty_pages,
        dirty_bytes: dirty_pages.saturating_mul(PAGE_SIZE),
        cached_writes: counters.cached_writes,
        cached_pages: counters.cached_pages,
        cached_bytes: counters.cached_bytes,
        fallback_writes: counters.fallback_writes,
        flush_calls: counters.flush_calls,
        flushed_pages: counters.flushed_pages,
        flushed_bytes: counters.flushed_bytes,
        dirty_pages_peak: counters.dirty_pages_peak,
        dirty_bytes_peak: counters.dirty_pages_peak.saturating_mul(PAGE_SIZE),
        pressure_flushes: counters.pressure_flushes,
        pressure_flushed_pages: counters.pressure_flushed_pages,
        pressure_flushed_bytes: counters.pressure_flushed_bytes,
        pressure_flush_failures: counters.pressure_flush_failures,
    }
}

fn dirty_logical_size(node: VfsNodeId) -> Option<usize> {
    DIRTY_REGULAR_FILES
        .lock()
        .get(&node)
        .map(|cache| cache.logical_size)
}

fn dirty_or_backend_logical_size(node: VfsNodeId) -> Option<usize> {
    if let Some(size) = dirty_logical_size(node) {
        return Some(size);
    }
    stat_full_cached(node).ok().map(|stat| stat.size as usize)
}

fn any_regular_file_dirty() -> bool {
    inode_state::any_regular_file_dirty()
}

fn sync_dirty_regular_file_count(map: &BTreeMap<VfsNodeId, DirtyFileCache>) {
    inode_state::sync_dirty_regular_file_count(map);
}

fn dirty_regular_file_has_pages(node: VfsNodeId) -> bool {
    if !any_regular_file_dirty() {
        return false;
    }
    DIRTY_REGULAR_FILES
        .lock()
        .get(&node)
        .is_some_and(|cache| !cache.pages.is_empty())
}

fn overlay_dirty_regular_stat(node: VfsNodeId, stat: &mut FileStat) {
    if !any_regular_file_dirty() {
        return;
    }
    let dirty = DIRTY_REGULAR_FILES.lock();
    let Some(cache) = dirty.get(&node) else {
        return;
    };
    stat.size = cache.logical_size as u64;
    let dirty_blocks = cache.pages.len().saturating_mul(PAGE_SIZE).div_ceil(512) as u64;
    stat.blocks = stat.blocks.max(dirty_blocks);
    stat.mtime_sec = cache.mtime.sec;
    stat.mtime_nsec = cache.mtime.nsec;
    stat.ctime_sec = cache.ctime.sec;
    stat.ctime_nsec = cache.ctime.nsec;
}

fn stat_logical_size(node: VfsNodeId, stat_size: u64) -> u64 {
    dirty_logical_size(node)
        .map(|size| size as u64)
        .unwrap_or(stat_size)
}

fn can_cache_dirty_write(
    kind: FsNodeKind,
    supports_dirty_writeback: bool,
    _offset: usize,
    len: usize,
    status_flags: OpenFlags,
) -> bool {
    kind == FsNodeKind::RegularFile
        && len > 0
        && len <= VFS_DIRTY_WRITEBACK_MAX_WRITE_SIZE
        && !status_flags.intersects(OpenFlags::DIRECT | OpenFlags::DSYNC | OpenFlags::SYNC)
        && supports_dirty_writeback
}

fn can_cache_dirty_user_buffer_write(
    kind: FsNodeKind,
    supports_dirty_writeback: bool,
    offset: usize,
    len: usize,
    status_flags: OpenFlags,
) -> bool {
    can_cache_dirty_write(kind, supports_dirty_writeback, offset, len, status_flags)
        && offset % PAGE_SIZE == 0
        && len % PAGE_SIZE == 0
}

fn dirty_write_page_pressure(
    dirty: &BTreeMap<VfsNodeId, DirtyFileCache>,
    node: VfsNodeId,
    page_start: usize,
    page_count: usize,
) -> (usize, usize) {
    let existing_pages = dirty
        .get(&node)
        .map(|cache| {
            (0..page_count)
                .filter(|page_offset| cache.pages.contains_key(&(page_start + page_offset)))
                .count()
        })
        .unwrap_or(0);
    let dirty_pages = dirty.values().map(|cache| cache.pages.len()).sum::<usize>();
    (dirty_pages, page_count.saturating_sub(existing_pages))
}

fn dirty_write_existing_pages(
    dirty: &BTreeMap<VfsNodeId, DirtyFileCache>,
    node: VfsNodeId,
    page_start: usize,
    page_count: usize,
) -> Vec<bool> {
    let Some(cache) = dirty.get(&node) else {
        let mut pages = Vec::with_capacity(page_count);
        pages.resize(page_count, false);
        return pages;
    };
    (0..page_count)
        .map(|page_offset| cache.pages.contains_key(&(page_start + page_offset)))
        .collect()
}

fn dirty_write_page_range(offset: usize, len: usize) -> Option<(usize, usize)> {
    if len == 0 {
        return Some((offset / PAGE_SIZE, 0));
    }
    let end = offset.checked_add(len)?;
    let page_start = offset / PAGE_SIZE;
    let page_end = (end - 1) / PAGE_SIZE;
    Some((page_start, page_end - page_start + 1))
}

fn prepare_dirty_regular_pages(
    offset: usize,
    buf: &[u8],
    existing_pages: &[bool],
) -> Option<BTreeMap<usize, DirtyPage>> {
    let end = offset.checked_add(buf.len())?;
    let (page_start, page_count) = dirty_write_page_range(offset, buf.len())?;
    let mut pages = BTreeMap::new();
    for page_delta in 0..page_count {
        if existing_pages.get(page_delta).copied().unwrap_or_default() {
            continue;
        }
        let page_index = page_start + page_delta;
        let page_offset = page_index.checked_mul(PAGE_SIZE)?;
        let copy_start = offset.max(page_offset);
        let copy_end = end.min(page_offset + PAGE_SIZE);
        let src_start = copy_start - offset;
        let dst_start = copy_start - page_offset;
        let copy_len = copy_end - copy_start;
        let page = if dst_start == 0 && copy_len == PAGE_SIZE {
            DirtyPage::full(buf[src_start..src_start + copy_len].to_vec())
        } else {
            let mut page = DirtyPage::empty();
            page.data[dst_start..dst_start + copy_len]
                .copy_from_slice(&buf[src_start..src_start + copy_len]);
            page.mark_dirty(dst_start, dst_start + copy_len);
            page
        };
        pages.insert(page_index, page);
    }
    Some(pages)
}

fn merge_dirty_page_write(page_index: usize, page: &mut DirtyPage, offset: usize, buf: &[u8]) {
    let page_offset = page_index * PAGE_SIZE;
    let write_end = offset + buf.len();
    let copy_start = offset.max(page_offset);
    let copy_end = write_end.min(page_offset + PAGE_SIZE);
    if copy_start >= copy_end {
        return;
    }
    if page.data.len() < PAGE_SIZE {
        page.data.resize(PAGE_SIZE, 0);
    }
    let src_start = copy_start - offset;
    let dst_start = copy_start - page_offset;
    let copy_len = copy_end - copy_start;
    page.data[dst_start..dst_start + copy_len]
        .copy_from_slice(&buf[src_start..src_start + copy_len]);
    page.mark_dirty(dst_start, dst_start + copy_len);
}

fn cache_dirty_regular_write(node: VfsNodeId, offset: usize, buf: &[u8]) -> DirtyCacheWriteResult {
    if buf.is_empty() {
        return DirtyCacheWriteResult::Cached(0);
    }
    let Some(logical_size) = dirty_or_backend_logical_size(node) else {
        return DirtyCacheWriteResult::Fallback;
    };
    let Some(end) = offset.checked_add(buf.len()) else {
        return DirtyCacheWriteResult::Fallback;
    };
    if offset > logical_size {
        return DirtyCacheWriteResult::Fallback;
    }

    let Some((page_start, page_count)) = dirty_write_page_range(offset, buf.len()) else {
        return DirtyCacheWriteResult::Fallback;
    };
    let existing_pages = {
        let dirty = DIRTY_REGULAR_FILES.lock();
        let (dirty_pages, new_pages) =
            dirty_write_page_pressure(&dirty, node, page_start, page_count);
        if dirty_pages.saturating_add(new_pages) > VFS_DIRTY_WRITEBACK_MAX_PAGES {
            return DirtyCacheWriteResult::NeedsPressureFlush;
        }
        dirty_write_existing_pages(&dirty, node, page_start, page_count)
    };
    let Some(mut prepared_pages) = prepare_dirty_regular_pages(offset, buf, &existing_pages) else {
        return DirtyCacheWriteResult::Fallback;
    };
    let needs_pin = {
        let dirty = DIRTY_REGULAR_FILES.lock();
        let (dirty_pages, new_pages) =
            dirty_write_page_pressure(&dirty, node, page_start, page_count);
        if dirty_pages.saturating_add(new_pages) > VFS_DIRTY_WRITEBACK_MAX_PAGES {
            return DirtyCacheWriteResult::NeedsPressureFlush;
        }
        !dirty.contains_key(&node)
    };
    let retained_pin = if needs_pin {
        match retain_inode(node) {
            Ok(state) => Some(state),
            Err(_) => return DirtyCacheWriteResult::Fallback,
        }
    } else {
        None
    };

    let timestamp = FileTimestamp::now();
    let mut release_extra_pin = false;
    let mut dirty = DIRTY_REGULAR_FILES.lock();
    let (dirty_pages, new_pages) = dirty_write_page_pressure(&dirty, node, page_start, page_count);
    if dirty_pages.saturating_add(new_pages) > VFS_DIRTY_WRITEBACK_MAX_PAGES {
        drop(dirty);
        if let Some(state) = retained_pin.as_ref() {
            release_inode_from_drop(state);
        }
        return DirtyCacheWriteResult::NeedsPressureFlush;
    }
    let cache_exists = dirty.contains_key(&node);
    if !cache_exists && retained_pin.is_none() {
        drop(dirty);
        return DirtyCacheWriteResult::Fallback;
    }
    let missing_prepared_page = (0..page_count).any(|page_delta| {
        let page_index = page_start + page_delta;
        !dirty
            .get(&node)
            .is_some_and(|cache| cache.pages.contains_key(&page_index))
            && !prepared_pages.contains_key(&page_index)
    });
    if missing_prepared_page {
        sync_dirty_regular_file_count(&dirty);
        drop(dirty);
        if let Some(state) = retained_pin.as_ref() {
            release_inode_from_drop(state);
        }
        return DirtyCacheWriteResult::Fallback;
    }
    if cache_exists && retained_pin.is_some() {
        release_extra_pin = true;
    }
    let cache = dirty.entry(node).or_insert_with(|| {
        DirtyFileCache::new(
            Arc::clone(
                retained_pin
                    .as_ref()
                    .expect("new dirty cache without inode pin"),
            ),
            logical_size,
            timestamp,
        )
    });
    cache.logical_size = cache.logical_size.max(end);
    cache.mtime = timestamp;
    cache.ctime = timestamp;
    for page_delta in 0..page_count {
        let page_index = page_start + page_delta;
        match cache.pages.get_mut(&page_index) {
            Some(existing) => merge_dirty_page_write(page_index, existing, offset, buf),
            None => {
                let Some(page) = prepared_pages.remove(&page_index) else {
                    continue;
                };
                cache.pages.insert(page_index, page);
            }
        }
    }
    let current_dirty_pages = dirty.values().map(|cache| cache.pages.len()).sum::<usize>();
    sync_dirty_regular_file_count(&dirty);
    drop(dirty);
    if release_extra_pin {
        release_inode_from_drop(
            retained_pin
                .as_ref()
                .expect("missing extra dirty inode pin"),
        );
    }

    record_dirty_cache_write(page_count, buf.len());
    record_dirty_cache_peak(current_dirty_pages);
    DirtyCacheWriteResult::Cached(buf.len())
}

fn cache_dirty_regular_user_buffer_write(
    node: VfsNodeId,
    offset: usize,
    buf: &UserBuffer,
) -> DirtyCacheWriteResult {
    let len = buf.len();
    if len == 0 {
        return DirtyCacheWriteResult::Cached(0);
    }
    if buf.buffers.iter().any(|slice| slice.len() % PAGE_SIZE != 0) {
        return DirtyCacheWriteResult::Fallback;
    }
    let Some(logical_size) = dirty_or_backend_logical_size(node) else {
        return DirtyCacheWriteResult::Fallback;
    };
    let Some(end) = offset.checked_add(len) else {
        return DirtyCacheWriteResult::Fallback;
    };
    if offset > logical_size {
        return DirtyCacheWriteResult::Fallback;
    }

    let page_start = offset / PAGE_SIZE;
    let page_count = len / PAGE_SIZE;
    let needs_pin = {
        let dirty = DIRTY_REGULAR_FILES.lock();
        let (dirty_pages, new_pages) =
            dirty_write_page_pressure(&dirty, node, page_start, page_count);
        if dirty_pages.saturating_add(new_pages) > VFS_DIRTY_WRITEBACK_MAX_PAGES {
            return DirtyCacheWriteResult::NeedsPressureFlush;
        }
        !dirty.contains_key(&node)
    };
    let retained_pin = if needs_pin {
        match retain_inode(node) {
            Ok(state) => Some(state),
            Err(_) => return DirtyCacheWriteResult::Fallback,
        }
    } else {
        None
    };

    let timestamp = FileTimestamp::now();
    let mut release_extra_pin = false;
    let mut dirty = DIRTY_REGULAR_FILES.lock();
    let (dirty_pages, new_pages) = dirty_write_page_pressure(&dirty, node, page_start, page_count);
    if dirty_pages.saturating_add(new_pages) > VFS_DIRTY_WRITEBACK_MAX_PAGES {
        drop(dirty);
        if let Some(state) = retained_pin.as_ref() {
            release_inode_from_drop(state);
        }
        return DirtyCacheWriteResult::NeedsPressureFlush;
    }
    let cache_exists = dirty.contains_key(&node);
    if !cache_exists && retained_pin.is_none() {
        drop(dirty);
        return DirtyCacheWriteResult::Fallback;
    }
    if cache_exists && retained_pin.is_some() {
        release_extra_pin = true;
    }
    let cache = dirty.entry(node).or_insert_with(|| {
        DirtyFileCache::new(
            Arc::clone(
                retained_pin
                    .as_ref()
                    .expect("new dirty cache without inode pin"),
            ),
            logical_size,
            timestamp,
        )
    });
    cache.logical_size = cache.logical_size.max(end);
    cache.mtime = timestamp;
    cache.ctime = timestamp;
    let mut page_index = page_start;
    for source in buf.buffers.iter() {
        for chunk in source.chunks(PAGE_SIZE) {
            cache
                .pages
                .insert(page_index, DirtyPage::full(chunk.to_vec()));
            page_index += 1;
        }
    }
    let current_dirty_pages = dirty.values().map(|cache| cache.pages.len()).sum::<usize>();
    sync_dirty_regular_file_count(&dirty);
    drop(dirty);
    if release_extra_pin {
        release_inode_from_drop(
            retained_pin
                .as_ref()
                .expect("missing extra dirty inode pin"),
        );
    }

    record_dirty_cache_write(page_count, len);
    record_dirty_cache_peak(current_dirty_pages);
    DirtyCacheWriteResult::Cached(len)
}

fn overlay_dirty_regular_read(node: VfsNodeId, offset: usize, buf: &mut [u8]) -> Option<usize> {
    if buf.is_empty() {
        return Some(0);
    }
    let dirty = DIRTY_REGULAR_FILES.lock();
    let cache = dirty.get(&node)?;
    if offset >= cache.logical_size {
        return Some(0);
    }
    let read_len = buf.len().min(cache.logical_size - offset);
    let first_page = offset / PAGE_SIZE;
    let last_page = (offset + read_len - 1) / PAGE_SIZE;
    for page_index in first_page..=last_page {
        let page_start = page_index * PAGE_SIZE;
        let page_end = page_start + PAGE_SIZE;
        let copy_start = offset.max(page_start);
        let copy_end = (offset + read_len).min(page_end);
        if copy_start >= copy_end {
            continue;
        }
        let Some(page) = cache.pages.get(&page_index) else {
            continue;
        };
        for (dirty_start, dirty_end) in page.dirty_ranges() {
            let dirty_file_start = page_start + dirty_start;
            let dirty_file_end = page_start + dirty_end;
            let dirty_copy_start = copy_start.max(dirty_file_start);
            let dirty_copy_end = copy_end.min(dirty_file_end);
            if dirty_copy_start >= dirty_copy_end {
                continue;
            }
            let dst_start = dirty_copy_start - offset;
            let src_start = dirty_copy_start - page_start;
            let len = dirty_copy_end - dirty_copy_start;
            buf[dst_start..dst_start + len].copy_from_slice(&page.data[src_start..src_start + len]);
        }
    }
    Some(read_len)
}

fn dirty_regular_read_len(node: VfsNodeId, offset: usize, len: usize) -> Option<usize> {
    if len == 0 {
        return Some(0);
    }
    let dirty = DIRTY_REGULAR_FILES.lock();
    let cache = dirty.get(&node)?;
    if offset >= cache.logical_size {
        Some(0)
    } else {
        Some(len.min(cache.logical_size - offset))
    }
}

#[derive(Debug)]
struct DirtyWritebackRun {
    offset: usize,
    data: Vec<u8>,
}

struct DirtyWritebackBatch {
    inode_state: Arc<inode_state::InodeState>,
    logical_size: usize,
    pages: BTreeMap<usize, DirtyPage>,
    runs: Vec<DirtyWritebackRun>,
}

fn collect_dirty_writeback(node: VfsNodeId) -> Option<DirtyWritebackBatch> {
    let mut dirty = DIRTY_REGULAR_FILES.lock();
    let cache = dirty.remove(&node)?;
    sync_dirty_regular_file_count(&dirty);
    let logical_size = cache.logical_size;
    let inode_state = cache.inode_state;
    let mut runs = Vec::new();
    let mut current_offset = 0usize;
    let mut current_data = Vec::new();
    let pages = cache.pages;
    for (page_index, page) in pages.iter() {
        let page_offset = page_index.saturating_mul(PAGE_SIZE);
        for (dirty_start, dirty_end) in page.dirty_ranges() {
            let dirty_offset = page_offset + dirty_start;
            let dirty_data = &page.data[dirty_start..dirty_end];
            if current_data.is_empty() {
                current_offset = dirty_offset;
            } else if current_offset + current_data.len() != dirty_offset {
                runs.push(DirtyWritebackRun {
                    offset: current_offset,
                    data: current_data,
                });
                current_offset = dirty_offset;
                current_data = Vec::new();
            }
            current_data.extend_from_slice(dirty_data);
            if current_data.len() >= VFS_WRITE_CHUNK_SIZE {
                runs.push(DirtyWritebackRun {
                    offset: current_offset,
                    data: current_data,
                });
                current_data = Vec::new();
            }
        }
    }
    if !current_data.is_empty() {
        runs.push(DirtyWritebackRun {
            offset: current_offset,
            data: current_data,
        });
    }
    Some(DirtyWritebackBatch {
        inode_state,
        logical_size,
        pages,
        runs,
    })
}

fn restore_dirty_writeback(node: VfsNodeId, batch: DirtyWritebackBatch) {
    let timestamp = FileTimestamp::now();
    let mut dirty = DIRTY_REGULAR_FILES.lock();
    let release_batch_pin = dirty.contains_key(&node);
    let cache = dirty.entry(node).or_insert_with(|| {
        DirtyFileCache::new(
            Arc::clone(&batch.inode_state),
            batch.logical_size,
            timestamp,
        )
    });
    cache.logical_size = cache.logical_size.max(batch.logical_size);
    for (page_index, page) in batch.pages {
        cache.pages.entry(page_index).or_insert(page);
    }
    sync_dirty_regular_file_count(&dirty);
    drop(dirty);
    if release_batch_pin {
        release_inode_from_drop(&batch.inode_state);
    }
}

fn write_backend_at(node: VfsNodeId, offset: u64, data: &[u8], allow_plan: bool) -> Option<usize> {
    if allow_plan {
        let plan = with_mount(node.mount_id, BackendOp::Write, |mount| {
            mount.prepare_write_plan(node.ino, offset, data.len())
        })
        .flatten();
        if let Some(plan) = plan {
            return Some(plan.execute(data));
        }
    }
    with_mount(node.mount_id, BackendOp::Write, |mount| {
        mount.write_at(node.ino, data, offset)
    })
}

/// Flushes one dirty file while its inode mapping-mutation lease is held.
///
/// `allow_plan` is false only for pressure reclaim of a different inode while
/// the caller already owns one mapping lease. That compatibility case avoids
/// cross-inode ABBA; normal explicit flushes and the caller's own dirty data
/// use the lock-free mapped-overwrite executor.
fn flush_dirty_regular_file_for_reason_under_mapping(
    node: VfsNodeId,
    reason: DirtyFlushReason,
    allow_plan: bool,
) -> FsResult {
    if !dirty_regular_file_has_pages(node) {
        return Ok(());
    }
    // Removing the dirty overlay before its runs reach the backend creates a
    // temporary window where an unguarded reader could observe old disk data.
    // Keep the inode generation unstable through collect, write, and restore.
    let _mutation = begin_regular_file_page_cache_mutation(node, FsNodeKind::RegularFile);
    let Some(batch) = collect_dirty_writeback(node) else {
        return Ok(());
    };
    let pages = batch.pages.len();
    let mut bytes = 0usize;
    let mut result = Ok(());
    for run in batch.runs.iter() {
        perf::record_vfs_write_backend(run.data.len());
        let write_size = write_backend_at(node, run.offset as u64, &run.data, allow_plan);
        let write_size = match write_size {
            Some(write_size) => write_size,
            None => {
                result = Err(FsError::Io);
                break;
            }
        };
        if write_size < run.data.len() {
            result = Err(FsError::Io);
            break;
        }
        bytes = bytes.saturating_add(run.data.len());
    }
    if result.is_err() {
        restore_dirty_writeback(node, batch);
        record_dirty_cache_flush_failure(reason);
        return result;
    }
    record_dirty_cache_flush(reason, pages, bytes);
    release_inode_from_drop(&batch.inode_state);
    Ok(())
}

fn flush_dirty_regular_file_for_reason(node: VfsNodeId, reason: DirtyFlushReason) -> FsResult {
    if !dirty_regular_file_has_pages(node) {
        return Ok(());
    }
    inode_state::with_mapping_mutation(node, || {
        flush_dirty_regular_file_for_reason_under_mapping(node, reason, true)
    })
}

pub(crate) fn flush_dirty_regular_file(node: VfsNodeId) -> FsResult {
    flush_dirty_regular_file_for_reason(node, DirtyFlushReason::Explicit)
}

pub(crate) fn flush_dirty_regular_files_on_mount(mount_id: MountId) -> FsResult {
    let nodes = {
        let dirty = DIRTY_REGULAR_FILES.lock();
        dirty
            .keys()
            .copied()
            .filter(|node| node.mount_id == mount_id)
            .collect::<Vec<_>>()
    };
    let mut result = Ok(());
    for node in nodes {
        if let Err(err) = flush_dirty_regular_file(node) {
            result = result.and(Err(err));
        }
    }
    result
}

fn flush_dirty_regular_files_for_pressure(mapping_locked: Option<VfsNodeId>) -> FsResult {
    let nodes = {
        let dirty = DIRTY_REGULAR_FILES.lock();
        dirty.keys().copied().collect::<Vec<_>>()
    };
    let mut result = Ok(());
    for node in nodes {
        let flush = if mapping_locked == Some(node) {
            flush_dirty_regular_file_for_reason_under_mapping(
                node,
                DirtyFlushReason::Pressure,
                true,
            )
        } else if mapping_locked.is_some() {
            // Do not acquire a second inode mapping lease while one is held.
            // The legacy backend path preserves the pre-FS4 locking behavior
            // for this rare global-pressure case.
            flush_dirty_regular_file_for_reason_under_mapping(
                node,
                DirtyFlushReason::Pressure,
                false,
            )
        } else {
            flush_dirty_regular_file_for_reason(node, DirtyFlushReason::Pressure)
        };
        if let Err(err) = flush {
            result = result.and(Err(err));
        }
    }
    result
}

fn track_writable_regular_open(node: VfsNodeId, kind: FsNodeKind, writable: bool) {
    if kind != FsNodeKind::RegularFile || !writable {
        return;
    }
    inode_state::track_writable_open(node);
}

fn untrack_writable_regular_open(node: VfsNodeId, kind: FsNodeKind, writable: bool) {
    if kind != FsNodeKind::RegularFile || !writable {
        return;
    }
    inode_state::untrack_writable_open(node);
}

fn track_writable_shared_regular_mmap(node: VfsNodeId, kind: FsNodeKind) {
    if kind != FsNodeKind::RegularFile {
        return;
    }
    invalidate_small_regular_read_cache(node, kind);
    inode_state::track_writable_shared_mmap(node);
}

fn untrack_writable_shared_regular_mmap(node: VfsNodeId, kind: FsNodeKind) {
    if kind != FsNodeKind::RegularFile {
        return;
    }
    inode_state::untrack_writable_shared_mmap(node);
}

fn ensure_mount_writable(mount_id: MountId) -> FsResult {
    if mount_is_read_only(mount_id) {
        Err(FsError::ReadOnly)
    } else {
        Ok(())
    }
}

fn ensure_special_file_open_allowed(
    mount_id: MountId,
    kind: FsNodeKind,
    flags: OpenFlags,
) -> FsResult {
    if !flags.contains(OpenFlags::PATH)
        && mount_is_nodev(mount_id)
        && matches!(kind, FsNodeKind::CharacterDevice | FsNodeKind::BlockDevice)
    {
        Err(FsError::AccessDenied)
    } else {
        Ok(())
    }
}

fn reject_nosymfollow_final_symlink(
    context: PathContext,
    name: &str,
    flags: OpenFlags,
) -> FsResult {
    if flags.contains(OpenFlags::NOFOLLOW) || flags.contains(OpenFlags::PATH) {
        return Ok(());
    }
    if !mount_any_nosymfollow() {
        return Ok(());
    }
    let Ok(path) = vfs_path::resolve_existing_in(context, name, LookupMode::NoFollowFinal) else {
        return Ok(());
    };
    if path.kind == FsNodeKind::Symlink && mount_is_nosymfollow(path.node.mount_id) {
        Err(FsError::Loop)
    } else {
        Ok(())
    }
}

fn page_cache_id_for_node_with_support(
    node: VfsNodeId,
    kind: FsNodeKind,
    supports_page_cache: bool,
) -> Option<PageCacheId> {
    if kind != FsNodeKind::RegularFile || !supports_page_cache {
        return None;
    }
    Some(PageCacheId::new(node.mount_id, node.ino))
}

fn invalidate_small_regular_read_cache(node: VfsNodeId, kind: FsNodeKind) {
    if kind == FsNodeKind::RegularFile {
        SMALL_REGULAR_READ_FILES.remove(node);
    }
}

fn cached_inode_flags(node: VfsNodeId) -> Option<u32> {
    inode_state::cached_inode_flags(node)
}

fn update_inode_flags_cache(node: VfsNodeId, flags: u32) {
    inode_state::update_inode_flags_cache(node, flags);
}

fn invalidate_inode_flags_cache(node: VfsNodeId) {
    inode_state::invalidate_inode_flags_cache(node);
}

pub(crate) fn begin_regular_file_page_cache_mutation(
    node: VfsNodeId,
    kind: FsNodeKind,
) -> Option<PageCacheMutationGuard> {
    begin_regular_file_page_cache_mutation_with_support(
        node,
        kind,
        mount_supports_page_cache(node.mount_id),
    )
}

fn begin_regular_file_page_cache_mutation_with_support(
    node: VfsNodeId,
    kind: FsNodeKind,
    supports_page_cache: bool,
) -> Option<PageCacheMutationGuard> {
    invalidate_small_regular_read_cache(node, kind);
    let id = page_cache_id_for_node_with_support(node, kind, supports_page_cache)?;
    let (guard, removed, scanned) = begin_page_cache_mutation(id);
    perf::record_vfs_read_cache_invalidation(removed, scanned);
    Some(guard)
}

/// Establishes a fresh content incarnation before a newly created inode can
/// race with a lookup after backend serialization is released.
pub(crate) fn initialize_regular_file_page_cache_incarnation(
    node: VfsNodeId,
    supports_page_cache: bool,
) {
    inode_state::initialize_new(node);
    drop(begin_regular_file_page_cache_mutation_with_support(
        node,
        FsNodeKind::RegularFile,
        supports_page_cache,
    ));
}

pub(crate) fn regular_file_is_open_writable_in(context: PathContext, name: &str) -> FsResult<bool> {
    let path = vfs_path::resolve_existing_in(context, name, LookupMode::FollowFinal)?;
    if path.kind != FsNodeKind::RegularFile {
        return Ok(false);
    }
    Ok(regular_file_node_is_open_writable(path.node))
}

pub(crate) fn regular_file_node_is_open_writable(node: VfsNodeId) -> bool {
    inode_state::is_open_writable(node)
}

fn regular_file_node_has_writable_shared_mmap(node: VfsNodeId) -> bool {
    inode_state::has_writable_shared_mmap(node)
}

pub(crate) fn mount_has_writable_regular_open(mount_id: MountId) -> bool {
    inode_state::mount_has_writable_open(mount_id)
}

pub(crate) fn track_regular_file_executable(node: VfsNodeId) {
    inode_state::track_executable(node);
}

pub(crate) fn untrack_regular_file_executable(node: VfsNodeId) {
    inode_state::untrack_executable(node);
}

pub(crate) fn regular_file_node_is_executable(node: VfsNodeId) -> bool {
    inode_state::is_executable(node)
}

#[derive(Clone, Debug)]
pub(crate) struct FileCreateAttrs {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) euid: u32,
    pub(crate) egid: u32,
    pub(crate) fsgid: u32,
    pub(crate) mode: u32,
    pub(crate) umask: u32,
    pub(crate) groups: Vec<u32>,
}

impl FileCreateAttrs {
    fn can_keep_setgid(&self, gid: u32) -> bool {
        self.euid == 0 || self.egid == gid || self.fsgid == gid || self.groups.contains(&gid)
    }
}

fn prepare_created_file_mode(parent_stat: FileStat, attrs: &FileCreateAttrs) -> u32 {
    let mut mode = attrs.mode;
    if parent_stat.mode & MODE_SETGID != 0
        && mode & MODE_SETGID != 0
        && !attrs.can_keep_setgid(parent_stat.gid)
    {
        mode &= !MODE_SETGID;
    }
    (mode & !attrs.umask) & MODE_PERMISSIONS_MASK
}

pub(crate) struct VfsFile {
    node: VfsNodeId,
    inode_state: Arc<inode_state::InodeState>,
    mount_backend: MountedBackendLease,
    parent: Option<VfsNodeId>,
    kind: FsNodeKind,
    namespace_id: MountNamespaceId,
    visible_path: Option<String>,
    offset: SleepMutex<usize>,
    read_snapshot: SleepMutex<Option<Vec<u8>>>,
    directory_snapshot: SleepMutex<Option<CachedDirectorySnapshot>>,
    read_snapshot_supported: bool,
    supports_page_cache: bool,
    supports_dirty_writeback: bool,
    readable: bool,
    writable: bool,
    status_flags: StatusFlagsCell,
    suppress_fanotify: bool,
}

struct CachedDirectorySnapshot {
    version: usize,
    snapshot: BackendDirectorySnapshot,
}

impl VfsFile {
    fn with_backend<V>(
        &self,
        op: BackendOp,
        f: impl FnOnce(&dyn super::FileSystemBackend) -> V,
    ) -> V {
        self.mount_backend.call(op, f)
    }

    fn new(
        path: VfsPath,
        parent: Option<VfsNodeId>,
        readable: bool,
        writable: bool,
        status_flags: OpenFlags,
        namespace_id: MountNamespaceId,
        suppress_fanotify: bool,
    ) -> FsResult<Self> {
        let node = path.node;
        let kind = path.kind;
        let visible_path = path.visible_path;
        // An open file description pins its backend inode even if the path is
        // later unlinked. Keep this retain paired with Drop's release path.
        let mount_backend = mounted_backend_lease(node.mount_id).ok_or(FsError::Io)?;
        let inode_state = retain_inode_with_lease(node, &mount_backend)?;
        let read_snapshot_supported = mount_backend.call(BackendOp::ReadPlan, |mount| {
            mount.supports_read_snapshot(node.ino)
        });
        let supports_page_cache = mount_supports_page_cache(node.mount_id);
        let supports_dirty_writeback = mount_supports_dirty_writeback(node.mount_id);
        track_writable_regular_open(node, kind, writable);
        let file = Self {
            node,
            inode_state,
            mount_backend,
            parent,
            kind,
            namespace_id,
            visible_path,
            offset: SleepMutex::new(0),
            read_snapshot: SleepMutex::new(None),
            directory_snapshot: SleepMutex::new(None),
            read_snapshot_supported,
            supports_page_cache,
            supports_dirty_writeback,
            readable,
            writable,
            status_flags: StatusFlagsCell::new(status_flags),
            suppress_fanotify,
        };
        Ok(file)
    }

    pub(crate) fn read_all(&self) -> Vec<u8> {
        let mut offset = self.offset.lock();
        let mut buffer = vec![0u8; VFS_READ_ALL_CHUNK_SIZE];
        let mut data = Vec::new();
        perf::record_vfs_read_all_call();
        loop {
            let len = self.read_backend_at_profiled(
                *offset,
                &mut buffer,
                perf::ProfilePoint::VfsReadAllBackend,
            );
            if len == 0 {
                break;
            }
            perf::record_vfs_read_all_backend_read(len);
            *offset += len;
            data.extend_from_slice(&buffer[..len]);
        }
        data
    }

    fn write_inner(&self, buf: UserBuffer, append: bool) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        let mut offset = self.offset.lock();
        if append {
            let stat =
                match stat_full_cached_with_state_and_lease(&self.inode_state, &self.mount_backend)
                {
                    Ok(stat) => stat,
                    Err(_) => {
                        return 0;
                    }
                };
            *offset = stat_logical_size(self.node, stat.size) as usize;
        }
        *self.read_snapshot.lock() = None;
        let _mutation = (buf.len() > 0)
            .then(|| begin_regular_file_page_cache_mutation(self.node, self.kind))
            .flatten();
        let mut total_write_size = 0usize;
        perf::record_vfs_write_user_buffer(buf.buffers.len());
        if self.kind == FsNodeKind::RegularFile && buf.buffers.len() > 1 {
            return self.write_coalesced_user_buffer(&mut offset, &buf);
        }
        for slice in buf.buffers.iter() {
            let write_size = self.write_at_chunks(*offset, slice);
            *offset = offset.checked_add(write_size).unwrap_or(usize::MAX);
            total_write_size = total_write_size.saturating_add(write_size);
            if write_size < slice.len() {
                break;
            }
        }
        total_write_size
    }

    fn write_coalesced_user_buffer(&self, offset: &mut usize, buf: &UserBuffer) -> usize {
        let mut total_write_size = 0usize;
        let mut bounce = Vec::with_capacity(VFS_WRITE_CHUNK_SIZE);
        for slice in &buf.buffers {
            let mut remaining: &[u8] = &slice[..];
            while !remaining.is_empty() {
                let available = VFS_WRITE_CHUNK_SIZE - bounce.len();
                let take = available.min(remaining.len());
                bounce.extend_from_slice(&remaining[..take]);
                remaining = &remaining[take..];
                if bounce.len() < VFS_WRITE_CHUNK_SIZE {
                    continue;
                }
                let write_size = self.flush_coalesced_write(offset, &bounce);
                total_write_size = total_write_size.saturating_add(write_size);
                if write_size < bounce.len() {
                    return total_write_size;
                }
                bounce.clear();
            }
        }
        if !bounce.is_empty() {
            let write_size = self.flush_coalesced_write(offset, &bounce);
            total_write_size = total_write_size.saturating_add(write_size);
        }
        total_write_size
    }

    fn flush_coalesced_write(&self, offset: &mut usize, chunk: &[u8]) -> usize {
        perf::record_vfs_write_coalesced(chunk.len());
        let write_size = self.write_at_chunks(*offset, chunk);
        *offset = offset.checked_add(write_size).unwrap_or(usize::MAX);
        write_size
    }

    fn write_at_chunks(&self, offset: usize, buf: &[u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        inode_state::with_mapping_mutation_value_state(&self.inode_state, || {
            let mut total_write_size = 0usize;
            for chunk in buf.chunks(VFS_WRITE_CHUNK_SIZE) {
                let Some(chunk_offset) = offset.checked_add(total_write_size) else {
                    break;
                };
                let mut cached_dirty = false;
                if can_cache_dirty_write(
                    self.kind,
                    self.supports_dirty_writeback,
                    chunk_offset,
                    chunk.len(),
                    self.status_flags.get(),
                ) {
                    let mut pressure_retried = false;
                    loop {
                        match cache_dirty_regular_write(self.node, chunk_offset, chunk) {
                            DirtyCacheWriteResult::Cached(write_size) => {
                                total_write_size = total_write_size.saturating_add(write_size);
                                if write_size < chunk.len() {
                                    break;
                                }
                                cached_dirty = true;
                                break;
                            }
                            DirtyCacheWriteResult::NeedsPressureFlush if !pressure_retried => {
                                if flush_dirty_regular_files_for_pressure(Some(self.node)).is_err()
                                {
                                    break;
                                }
                                pressure_retried = true;
                            }
                            DirtyCacheWriteResult::NeedsPressureFlush
                            | DirtyCacheWriteResult::Fallback => break,
                        }
                    }
                }
                if cached_dirty {
                    continue;
                }
                if self.kind == FsNodeKind::RegularFile && !chunk.is_empty() {
                    record_dirty_cache_fallback();
                    if flush_dirty_regular_file_for_reason_under_mapping(
                        self.node,
                        DirtyFlushReason::Explicit,
                        true,
                    )
                    .is_err()
                    {
                        break;
                    }
                }
                perf::record_vfs_write_backend(chunk.len());
                let Some(write_size) =
                    write_backend_at(self.node, chunk_offset as u64, chunk, true)
                else {
                    break;
                };
                total_write_size = total_write_size.saturating_add(write_size);
                if write_size < chunk.len() {
                    break;
                }
            }
            total_write_size
        })
    }

    fn read_backend_at_profiled(
        &self,
        offset: usize,
        buf: &mut [u8],
        point: perf::ProfilePoint,
    ) -> usize {
        let _profile_scope = perf::time_scope(point);
        let read_size = inode_state::with_mapping_read_state(&self.inode_state, || {
            let read_plan = (self.kind == FsNodeKind::RegularFile && self.supports_page_cache)
                .then(|| {
                    self.with_backend(BackendOp::ReadPlan, |mount| {
                        mount.prepare_read_plan(self.node.ino, offset as u64, buf.len())
                    })
                })
                .flatten();
            if let Some(read_plan) = read_plan {
                read_plan.execute(buf)
            } else {
                self.with_backend(BackendOp::ReadFallback, |mount| {
                    mount.read_at(self.node.ino, buf, offset as u64)
                })
            }
        });
        let read_size =
            if let Some(dirty_len) = dirty_regular_read_len(self.node, offset, buf.len()) {
                let effective_len = read_size.max(dirty_len);
                if effective_len > read_size {
                    buf[read_size..effective_len].fill(0);
                }
                let _ = overlay_dirty_regular_read(self.node, offset, &mut buf[..effective_len]);
                effective_len
            } else {
                read_size
            };
        perf::record_vfs_read_backend(read_size);
        read_size
    }

    fn read_backend_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        self.read_backend_at_profiled(offset, buf, perf::ProfilePoint::VfsReadBackend)
    }

    fn read_backend_at_preserve_noatime(&self, offset: usize, buf: &mut [u8]) -> usize {
        let noatime_snapshot = self.noatime_snapshot();
        let read_size = self.read_backend_at(offset, buf);
        if !buf.is_empty() {
            self.restore_noatime(noatime_snapshot);
        }
        read_size
    }

    fn read_snapshot_at(&self, offset: usize, buf: &mut [u8]) -> Option<usize> {
        if !self.read_snapshot_supported {
            return None;
        }
        let mut snapshot = self.read_snapshot.lock();
        if offset == 0 {
            *snapshot = None;
        }
        if snapshot.is_none() {
            let content = match inode_state::with_mapping_read_state(&self.inode_state, || {
                self.with_backend(BackendOp::ReadFallback, |mount| {
                    mount.read_snapshot(self.node.ino)
                })
            }) {
                Some(Ok(content)) => content,
                Some(Err(_)) => return Some(0),
                None => return None,
            };
            *snapshot = Some(content);
        }

        let content = snapshot.as_ref()?;
        let start = offset.min(content.len());
        let len = buf.len().min(content.len() - start);
        buf[..len].copy_from_slice(&content[start..start + len]);
        if len > 0 {
            perf::record_procfs_snapshot_hit(len);
        }
        Some(len)
    }

    fn read_snapshot_user_buffer(&self, offset: &mut usize, buf: &mut UserBuffer) -> Option<usize> {
        if !self.read_snapshot_supported {
            return None;
        }
        let mut total_read_size = 0usize;
        for slice in buf.buffers.iter_mut() {
            let read_size = self.read_snapshot_at(*offset, slice)?;
            if read_size == 0 {
                break;
            }
            *offset = offset.checked_add(read_size).unwrap_or(usize::MAX);
            total_read_size = total_read_size.saturating_add(read_size);
            if read_size < slice.len() {
                break;
            }
        }
        Some(total_read_size)
    }

    fn read_coalesced_user_buffer(
        &self,
        offset: &mut usize,
        buf: &mut UserBuffer,
    ) -> Option<usize> {
        if self.kind != FsNodeKind::RegularFile
            || buf.buffers.len() <= 1
            || buf.len() <= VFS_READ_CACHE_MAX_FILE_SIZE
        {
            return None;
        }
        let stat =
            stat_basic_cached_with_state_and_lease(&self.inode_state, &self.mount_backend).ok()?;
        let file_size = stat.size as usize;
        if self.read_cache_id_for_size(file_size).is_some() {
            return None;
        }

        let mut bounce = vec![0u8; VFS_READ_CHUNK_SIZE];
        let mut buffer_index = 0usize;
        let mut buffer_offset = 0usize;
        let mut total_read_size = 0usize;
        loop {
            let read_limit = user_buffer_chunk_len(
                buf.buffers.as_slice(),
                buffer_index,
                buffer_offset,
                VFS_READ_CHUNK_SIZE,
            );
            if read_limit == 0 {
                break;
            }
            let noatime_snapshot = self.noatime_snapshot();
            let read_size = self.read_backend_at_profiled(
                *offset,
                &mut bounce[..read_limit],
                perf::ProfilePoint::VfsReadCoalescedBackend,
            );
            self.restore_noatime(noatime_snapshot);
            if read_size == 0 {
                break;
            }
            perf::record_vfs_read_coalesced(read_size);
            let copied = copy_into_user_buffer(
                buf.buffers.as_mut_slice(),
                &mut buffer_index,
                &mut buffer_offset,
                &bounce[..read_size],
            );
            *offset = offset.checked_add(copied).unwrap_or(usize::MAX);
            total_read_size = total_read_size.saturating_add(copied);
            if copied < read_size || read_size < read_limit {
                break;
            }
        }
        Some(total_read_size)
    }

    fn noatime_snapshot(&self) -> Option<(FileTimestamp, FileTimestamp)> {
        if !self.status_flags.get().contains(OpenFlags::NOATIME)
            && !mount_is_noatime(self.node.mount_id)
        {
            return None;
        }
        let stat =
            stat_basic_cached_with_state_and_lease(&self.inode_state, &self.mount_backend).ok()?;
        Some((
            FileTimestamp {
                sec: stat.atime_sec,
                nsec: stat.atime_nsec,
            },
            FileTimestamp {
                sec: stat.ctime_sec,
                nsec: stat.ctime_nsec,
            },
        ))
    }

    fn restore_noatime(&self, snapshot: Option<(FileTimestamp, FileTimestamp)>) {
        if let Some((atime, ctime)) = snapshot {
            let _ = inode_state::with_metadata_update_state(
                &self.inode_state,
                inode_state::MetadataCacheUpdate::Times {
                    atime: Some(atime),
                    mtime: None,
                    ctime,
                },
                || {
                    self.with_backend(BackendOp::NamespaceMutation, |mount| {
                        mount.set_times(self.node.ino, Some(atime), None, ctime)
                    })
                },
            );
        }
    }

    fn touch_directory_atime(&self) {
        if mount_is_noatime(self.node.mount_id) || mount_is_nodiratime(self.node.mount_id) {
            return;
        }
        let Ok(stat) =
            stat_full_cached_with_state_and_lease(&self.inode_state, &self.mount_backend)
        else {
            return;
        };
        let ctime = FileTimestamp {
            sec: stat.ctime_sec,
            nsec: stat.ctime_nsec,
        };
        let atime = FileTimestamp::now();
        let _ = inode_state::with_metadata_update_state(
            &self.inode_state,
            inode_state::MetadataCacheUpdate::Times {
                atime: Some(atime),
                mtime: None,
                ctime,
            },
            || {
                self.with_backend(BackendOp::NamespaceMutation, |mount| {
                    mount.set_times(self.node.ino, Some(atime), None, ctime)
                })
            },
        );
    }

    fn cached_page_cache_id(&self) -> Option<PageCacheId> {
        page_cache_id_for_node_with_support(self.node, self.kind, self.supports_page_cache)
    }

    fn seek_data_or_hole(&self, offset: usize, seek_hole: bool) -> FsResult<usize> {
        if self.kind != FsNodeKind::RegularFile {
            return Err(FsError::IllegalSeek);
        }
        if dirty_regular_file_has_pages(self.node) {
            flush_dirty_regular_file(self.node)?;
        }
        // UNFINISHED: This generic fallback infers sparse data/hole ranges
        // from nonzero bytes in filesystem-sized blocks instead of querying
        // backend extent allocation, so allocated zero-filled blocks may be
        // reported as holes.
        let stat = stat_full_cached_with_state_and_lease(&self.inode_state, &self.mount_backend)?;
        let size = stat.size as usize;
        if offset > size {
            return Err(FsError::NoDeviceOrAddress);
        }
        if offset == size {
            return if seek_hole {
                Ok(size)
            } else {
                Err(FsError::NoDeviceOrAddress)
            };
        }

        let block_size = (stat.blksize as usize).max(SEEK_SCAN_MIN_BLOCK_SIZE);
        let mut buf = vec![0u8; block_size];
        let mut block_start = offset / block_size * block_size;
        let mut result = offset;

        while block_start < size {
            let block_end = block_start.saturating_add(block_size).min(size);
            let valid_len = block_end - block_start;
            buf[..valid_len].fill(0);
            let read_len = self.read_backend_at_profiled(
                block_start,
                &mut buf[..valid_len],
                perf::ProfilePoint::VfsSeekScanRead,
            );
            if read_len < valid_len {
                buf[read_len..valid_len].fill(0);
            }
            let is_data = buf[..valid_len].iter().any(|byte| *byte != 0);
            if seek_hole != is_data {
                return Ok(result.min(size));
            }

            block_start = block_start.saturating_add(block_size);
            result = block_start;
        }

        if seek_hole {
            Ok(size)
        } else {
            Err(FsError::NoDeviceOrAddress)
        }
    }

    fn inode_flags_or_empty(&self) -> FsResult<u32> {
        if let Some(flags) = cached_inode_flags(self.node) {
            return Ok(flags);
        }
        let flags = match self.inode_flags() {
            Ok(flags) => Ok(flags),
            // CONTEXT: procfs and other synthetic filesystems do not expose
            // ext-style inode flags. Treat them as having no immutable/append
            // bits so writable sysctl-style files can be updated normally.
            Err(FsError::Unsupported) => Ok(0),
            Err(err) => Err(err),
        }?;
        update_inode_flags_cache(self.node, flags);
        Ok(flags)
    }

    fn read_synthetic_dirent64(&self, entry_offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        let Some(parent_path) = self.visible_path.as_deref() else {
            return Ok((0, entry_offset));
        };
        let entries: Vec<RawDirEntry> =
            synthetic_children_for_dir(self.namespace_id, self.node, parent_path)
                .into_iter()
                .filter(|entry| {
                    !self.with_backend(BackendOp::Lookup, |mount| {
                        mount
                            .lookup_component_from(self.node.ino, entry.name.as_str())
                            .is_ok()
                    })
                })
                .map(|entry| RawDirEntry {
                    ino: entry.ino,
                    name: entry.name,
                    dtype: DT_DIR,
                })
                .collect();
        let (read_size, next_entry_offset) = write_dir_entries_with_offset_base(
            entries.as_slice(),
            entry_offset,
            SYNTHETIC_DIRENT_OFFSET_BASE,
            buf,
        )?;
        Ok((read_size, SYNTHETIC_DIRENT_OFFSET_BASE + next_entry_offset))
    }

    fn read_cache_id_for_size(&self, _file_size: usize) -> Option<PageCacheId> {
        if dirty_regular_file_has_pages(self.node) {
            perf::record_vfs_read_cache_skip_dirty_pages();
            return None;
        }
        let id = self.cached_page_cache_id();
        if id.is_some() {
            perf::record_vfs_read_cache_eligible();
        }
        id
    }

    fn read_regular_cached_at(&self, offset: usize, buf: &mut [u8]) -> Option<usize> {
        if buf.is_empty() {
            return Some(0);
        }
        if dirty_regular_file_has_pages(self.node) {
            perf::record_vfs_read_cache_skip_dirty_pages();
            return None;
        }
        let id = self.cached_page_cache_id()?;
        let generation = PAGE_CACHE.read(id).current_stable_generation(id)?;
        let mut cached_file_size = None;
        let mut total_read_size = 0usize;

        while total_read_size < buf.len() {
            let file_offset = offset.checked_add(total_read_size)?;
            let page_start = file_offset / PAGE_SIZE * PAGE_SIZE;
            let page_offset = file_offset - page_start;
            let copy_len = (buf.len() - total_read_size).min(PAGE_SIZE - page_offset);
            let key = PageCacheKey::for_page(id, generation, page_start / PAGE_SIZE);

            if let Some(read_size) = PAGE_CACHE.write(id).copy_read_cache_page_data(
                key,
                page_offset,
                copy_len,
                &mut buf[total_read_size..total_read_size + copy_len],
            ) {
                if read_size == 0 {
                    break;
                }
                total_read_size += read_size;
                perf::record_vfs_read_cache_hit(read_size);
                continue;
            }

            let file_size = match cached_file_size {
                Some(file_size) => file_size,
                None => {
                    let stat = stat_basic_cached_with_state_and_lease(
                        &self.inode_state,
                        &self.mount_backend,
                    )
                    .ok()?;
                    let file_size = stat.size as usize;
                    self.read_cache_id_for_size(file_size)?;
                    cached_file_size = Some(file_size);
                    file_size
                }
            };
            if file_offset >= file_size {
                break;
            }
            let valid_len = PAGE_SIZE.min(file_size - page_start);
            if page_offset >= valid_len {
                break;
            }
            let copy_len = (buf.len() - total_read_size).min(valid_len - page_offset);
            perf::record_vfs_read_cache_miss();
            let max_readahead_pages =
                ((file_size - page_start).div_ceil(PAGE_SIZE)).min(VFS_READ_CACHE_READAHEAD_PAGES);
            let load_gate = Arc::new(PageCacheLoadGate::new());
            let reservation = PAGE_CACHE.write(id).reserve_read_cache_load(
                key,
                max_readahead_pages,
                load_gate.clone(),
            );
            let readahead_pages = match reservation {
                ReadCacheLoadReservation::Cached => continue,
                ReadCacheLoadReservation::Wait(existing_gate) => {
                    {
                        let _profile_scope =
                            perf::time_scope(perf::ProfilePoint::PageCacheLoadWait);
                        existing_gate.wait();
                    }
                    continue;
                }
                ReadCacheLoadReservation::Owner { pages } => pages,
                ReadCacheLoadReservation::StaleGeneration => return None,
            };
            let read_limit = (readahead_pages * PAGE_SIZE).min(file_size - page_start);
            let mut frame_run = frame_alloc_contiguous_uninit(readahead_pages);
            let mut staging = if frame_run.is_some() {
                Vec::new()
            } else {
                vec![0u8; read_limit]
            };

            let noatime_snapshot = self.noatime_snapshot();
            let _profile_scope = perf::time_scope(perf::ProfilePoint::VfsReadCacheFill);
            let fill_buf = if let Some(run) = frame_run.as_mut() {
                &mut run.as_mut_bytes()[..read_limit]
            } else {
                staging.as_mut_slice()
            };
            let read_len = inode_state::with_mapping_read_state(&self.inode_state, || {
                let read_plan = self.with_backend(BackendOp::ReadPlan, |mount| {
                    mount.prepare_read_plan(self.node.ino, page_start as u64, read_limit)
                });
                if let Some(read_plan) = read_plan {
                    read_plan.execute(fill_buf)
                } else {
                    self.with_backend(BackendOp::ReadFallback, |mount| {
                        mount.read_at(self.node.ino, fill_buf, page_start as u64)
                    })
                }
            });
            self.restore_noatime(noatime_snapshot);
            perf::record_vfs_read_cache_backend_read();
            assert!(
                read_len <= read_limit,
                "filesystem backend read exceeded its destination"
            );
            if let Some(run) = frame_run.as_mut() {
                run.as_mut_bytes()[read_len..].fill(0);
            }
            if read_len == 0 || page_offset >= read_len {
                PAGE_CACHE
                    .write(id)
                    .release_read_cache_load(key, readahead_pages, &load_gate);
                load_gate.complete();
                break;
            }

            let copy_len = copy_len.min(read_len - page_offset);
            let mut pages_to_cache = Vec::with_capacity(readahead_pages);
            if let Some(mut run) = frame_run {
                buf[total_read_size..total_read_size + copy_len]
                    .copy_from_slice(&run.as_mut_bytes()[page_offset..page_offset + copy_len]);
                for (page_delta, frame) in run.into_frames().into_iter().enumerate() {
                    let batch_offset = page_delta * PAGE_SIZE;
                    if batch_offset >= read_len {
                        break;
                    }
                    let page_file_offset = page_start + batch_offset;
                    let page_valid_len = PAGE_SIZE.min(file_size - page_file_offset);
                    let page_read_len = (read_len - batch_offset).min(page_valid_len);
                    if page_read_len != page_valid_len {
                        break;
                    }
                    pages_to_cache.push((
                        PageCacheKey::for_page(id, generation, key.page_index + page_delta),
                        frame,
                    ));
                }
            } else {
                buf[total_read_size..total_read_size + copy_len]
                    .copy_from_slice(&staging[page_offset..page_offset + copy_len]);
                for page_delta in 0..readahead_pages {
                    let batch_offset = page_delta * PAGE_SIZE;
                    if batch_offset >= read_len {
                        break;
                    }
                    let page_file_offset = page_start + batch_offset;
                    let page_valid_len = PAGE_SIZE.min(file_size - page_file_offset);
                    let page_read_len = (read_len - batch_offset).min(page_valid_len);
                    if page_read_len != page_valid_len {
                        break;
                    }
                    let frame = if page_valid_len == PAGE_SIZE {
                        frame_alloc_uninit()
                    } else {
                        let _profile_scope =
                            perf::time_scope(perf::ProfilePoint::FrameAllocReadCache);
                        frame_alloc()
                    };
                    let Some(frame) = frame else {
                        continue;
                    };
                    frame.ppn.get_bytes_array()[..page_valid_len]
                        .copy_from_slice(&staging[batch_offset..batch_offset + page_valid_len]);
                    pages_to_cache.push((
                        PageCacheKey::for_page(id, generation, key.page_index + page_delta),
                        frame,
                    ));
                }
            }

            let prepared_pages = pages_to_cache.len();
            let mut evicted = 0usize;
            let mut readahead_cached_pages = 0usize;
            let mut stale_generation = false;
            let mut cache = PAGE_CACHE.write(id);
            if cache.current_stable_generation(id) == Some(generation) {
                for (cache_key, frame) in pages_to_cache {
                    let is_readahead = cache_key.page_index != key.page_index;
                    let (page_evictions, inserted) =
                        cache.insert_read_cache_page(cache_key, frame, file_size);
                    evicted += page_evictions;
                    if inserted && is_readahead {
                        readahead_cached_pages += 1;
                    }
                }
            } else {
                perf::record_page_cache_stale_fill_drop(prepared_pages);
                stale_generation = true;
            }
            cache.release_read_cache_load(key, readahead_pages, &load_gate);
            drop(cache);
            load_gate.complete();
            if stale_generation {
                // Earlier cache hits may already have copied into `buf`.
                // Returning None makes the caller overwrite the complete
                // request through the backend instead of mixing epochs.
                return None;
            }
            if evicted > 0 {
                perf::record_page_cache_clean_eviction(evicted);
            }
            if readahead_cached_pages > 0 {
                perf::record_vfs_read_cache_readahead(readahead_cached_pages);
            }

            total_read_size += copy_len;
            if read_len < valid_len {
                break;
            }
        }

        if PAGE_CACHE.read(id).current_stable_generation(id) != Some(generation) {
            perf::record_page_cache_generation_retry();
            // The caller overwrites the complete destination through the
            // backend, so cache hits copied before the mutation cannot leak as
            // a mixed-generation short read.
            return None;
        }
        Some(total_read_size)
    }

    /// Try to serve a read directly from dirty pages without hitting the
    /// backend.  When every page in `[offset, offset+len)` is present in the
    /// dirty cache and is fully dirty the data is copied straight into `buf`
    /// (zero-filling non-dirty gaps).  Partial coverage returns [`None`] so
    /// the caller can fall through to the existing backend‑read‑then‑overlay
    /// path.
    fn read_dirty_regular_at(&self, offset: usize, buf: &mut [u8]) -> Option<usize> {
        if buf.is_empty() {
            return Some(0);
        }
        if self.kind != FsNodeKind::RegularFile {
            return None;
        }
        if !any_regular_file_dirty() {
            return None;
        }
        let dirty = DIRTY_REGULAR_FILES.lock();
        let cache = dirty.get(&self.node)?;
        if offset >= cache.logical_size {
            return Some(0);
        }
        let read_len = buf.len().min(cache.logical_size - offset);
        let first_page = offset / PAGE_SIZE;
        let last_page = (offset + read_len - 1) / PAGE_SIZE;
        // Require every page in the range to exist and be fully dirty.
        for pi in first_page..=last_page {
            let page = cache.pages.get(&pi)?;
            if !page.dirty_ranges().any(|(s, e)| s == 0 && e == PAGE_SIZE) {
                return None;
            }
        }
        // All pages are fully covered – copy dirty data directly.
        buf[..read_len].fill(0);
        for pi in first_page..=last_page {
            let page_start = pi * PAGE_SIZE;
            let page = &cache.pages[&pi];
            let copy_start = offset.max(page_start);
            let copy_end = (offset + read_len).min(page_start + PAGE_SIZE);
            if copy_start >= copy_end {
                continue;
            }
            let dst_start = copy_start - offset;
            let src_start = copy_start - page_start;
            let len = copy_end - copy_start;
            buf[dst_start..dst_start + len].copy_from_slice(&page.data[src_start..src_start + len]);
        }
        Some(read_len)
    }

    #[allow(dead_code)]
    fn read_small_regular_cached_at(&self, offset: usize, buf: &mut [u8]) -> Option<usize> {
        if buf.is_empty() {
            return Some(0);
        }
        if self.kind != FsNodeKind::RegularFile
            || !self.supports_page_cache
            || regular_file_node_has_writable_shared_mmap(self.node)
            || dirty_regular_file_has_pages(self.node)
        {
            return None;
        }
        if let Some(read_size) = SMALL_REGULAR_READ_FILES.read_at(self.node, offset, buf) {
            return Some(read_size);
        }

        let stat =
            stat_basic_cached_with_state_and_lease(&self.inode_state, &self.mount_backend).ok()?;
        let file_size = stat.size as usize;
        if !(VFS_SMALL_READ_CACHE_MIN_FILE_SIZE..=VFS_READ_CACHE_MAX_FILE_SIZE).contains(&file_size)
        {
            return None;
        }
        if offset >= file_size {
            return Some(0);
        }

        let mut data = vec![0u8; file_size];
        let mut filled = 0usize;
        let noatime_snapshot = self.noatime_snapshot();
        while filled < file_size {
            let chunk_len = (file_size - filled).min(VFS_READ_CHUNK_SIZE);
            let read_size = self.read_backend_at(filled, &mut data[filled..filled + chunk_len]);
            if read_size == 0 {
                break;
            }
            filled += read_size;
            if read_size < chunk_len {
                break;
            }
        }
        self.restore_noatime(noatime_snapshot);
        data.truncate(filled);
        if data.is_empty() {
            return Some(0);
        }
        if dirty_regular_file_has_pages(self.node) {
            return None;
        }

        let cache = SmallRegularReadCache { data };
        let read_size = cache.read_at(offset, buf);
        SMALL_REGULAR_READ_FILES.insert(self.node, cache);
        Some(read_size)
    }
}

fn user_buffer_chunk_len(
    buffers: &[&'static mut [u8]],
    mut buffer_index: usize,
    mut buffer_offset: usize,
    limit: usize,
) -> usize {
    let mut len = 0usize;
    while buffer_index < buffers.len() && len < limit {
        let buffer_len = buffers[buffer_index].len();
        if buffer_offset >= buffer_len {
            buffer_index += 1;
            buffer_offset = 0;
            continue;
        }
        let take = (limit - len).min(buffer_len - buffer_offset);
        len += take;
        buffer_index += 1;
        buffer_offset = 0;
    }
    len
}

fn copy_into_user_buffer(
    buffers: &mut [&'static mut [u8]],
    buffer_index: &mut usize,
    buffer_offset: &mut usize,
    src: &[u8],
) -> usize {
    let mut copied = 0usize;
    while copied < src.len() {
        while *buffer_index < buffers.len() && *buffer_offset >= buffers[*buffer_index].len() {
            *buffer_index += 1;
            *buffer_offset = 0;
        }
        if *buffer_index >= buffers.len() {
            break;
        }
        let dst = &mut buffers[*buffer_index][*buffer_offset..];
        let take = dst.len().min(src.len() - copied);
        dst[..take].copy_from_slice(&src[copied..copied + take]);
        copied += take;
        *buffer_offset += take;
    }
    copied
}

fn parent_hint_for_open(context: &PathContext, name: &str) -> Option<VfsNodeId> {
    vfs_path::resolve_create_parent_in(context.clone(), name)
        .ok()
        .map(|target| target.parent)
}

fn open_vfs_file_impl(
    context: PathContext,
    name: &str,
    flags: OpenFlags,
    create_attrs: Option<FileCreateAttrs>,
) -> FsResult<Arc<VfsFile>> {
    let namespace_id = context.namespace_id();
    let parent_hint = parent_hint_for_open(&context, name);
    let follow_final_symlink = !flags.contains(OpenFlags::NOFOLLOW);
    reject_nosymfollow_final_symlink(context.clone(), name, flags)?;
    let resolved = vfs_path::resolve_open_in(
        context,
        name,
        follow_final_symlink,
        flags.contains(OpenFlags::CREATE),
    )?;

    let (path, parent, readable, writable) = match resolved {
        VfsOpenTarget::Existing(path) => {
            if flags.contains(OpenFlags::CREATE | OpenFlags::EXCL) {
                return Err(FsError::AlreadyExists);
            }
            if path.kind == FsNodeKind::Directory {
                if !flags.can_open_directory() {
                    return Err(FsError::IsDir);
                }
                (path, parent_hint, false, false)
            } else {
                if flags.contains(OpenFlags::DIRECTORY) {
                    return Err(FsError::NotDir);
                }
                // CONTEXT: readlinkat("", fd) needs an O_PATH|O_NOFOLLOW fd
                // that refers to the symlink itself; full O_PATH semantics are
                // intentionally deferred.
                if path.kind == FsNodeKind::Symlink
                    && flags.contains(OpenFlags::NOFOLLOW)
                    && !flags.contains(OpenFlags::PATH)
                {
                    return Err(FsError::Loop);
                }
                ensure_special_file_open_allowed(path.node.mount_id, path.kind, flags)?;
                let (readable, writable) = flags.read_write();
                if path.kind == FsNodeKind::RegularFile
                    && writable
                    && regular_file_node_is_executable(path.node)
                {
                    return Err(FsError::TextBusy);
                }
                if flags.contains(OpenFlags::TRUNC) && flags.writable_target() {
                    ensure_mount_writable(path.node.mount_id)?;
                    let _mutation = begin_regular_file_page_cache_mutation(path.node, path.kind);
                    flush_dirty_regular_file(path.node)?;
                    inode_state::with_mapping_mutation(path.node, || {
                        with_mount(path.node.mount_id, BackendOp::TruncateAllocate, |mount| {
                            mount.set_len(path.node.ino, 0)
                        })
                        .ok_or(FsError::Io)?
                    })?;
                }
                (path, parent_hint, readable, writable)
            }
        }
        VfsOpenTarget::Create(target) => {
            if flags.contains(OpenFlags::DIRECTORY) {
                return Err(FsError::InvalidInput);
            }
            ensure_mount_writable(target.parent.mount_id)?;
            let parent_stat = stat_full_cached(target.parent)?;
            let supports_page_cache = mount_supports_page_cache(target.parent.mount_id);
            let prepared_attrs = create_attrs.as_ref().map(|attrs| {
                let gid = if parent_stat.mode & MODE_SETGID != 0 {
                    parent_stat.gid
                } else {
                    attrs.gid
                };
                (
                    attrs.uid,
                    gid,
                    prepare_created_file_mode(parent_stat, attrs),
                )
            });
            let ino = inode_state::with_directory_mutation(target.parent, || {
                let ino = with_mount(
                    target.parent.mount_id,
                    BackendOp::NamespaceMutation,
                    |mount| match prepared_attrs {
                        Some((uid, gid, mode)) => mount.create_node_with_owner(
                            target.parent.ino,
                            target.leaf_name,
                            FsNodeKind::RegularFile,
                            mode,
                            0,
                            uid,
                            gid,
                        ),
                        None => mount.create_file(target.parent.ino, target.leaf_name),
                    },
                )
                .ok_or(FsError::Io)??;
                initialize_regular_file_page_cache_incarnation(
                    VfsNodeId::new(target.parent.mount_id, ino),
                    supports_page_cache,
                );
                Ok(ino)
            })?;
            dentry_cache::invalidate_parent(target.parent);
            let (readable, writable) = flags.read_write();
            (
                VfsPath::with_visible_path(
                    VfsNodeId::new(target.parent.mount_id, ino),
                    FsNodeKind::RegularFile,
                    target.leaf_path,
                ),
                Some(target.parent),
                readable,
                writable,
            )
        }
    };

    Ok(Arc::new(VfsFile::new(
        path,
        parent,
        readable,
        writable,
        OpenFlags::file_status_flags(flags),
        namespace_id,
        false,
    )?))
}

fn create_tmpfile_inode(
    namespace_id: MountNamespaceId,
    directory: VfsPath,
    flags: OpenFlags,
    create_attrs: Option<FileCreateAttrs>,
) -> FsResult<Arc<VfsFile>> {
    if directory.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    let (_, writable) = flags.read_write();
    if !writable {
        return Err(FsError::InvalidInput);
    }
    ensure_mount_writable(directory.node.mount_id)?;

    let parent_stat = stat_full_cached(directory.node)?;
    let supports_page_cache = mount_supports_page_cache(directory.node.mount_id);
    let prepared_attrs = create_attrs.as_ref().map(|attrs| {
        let gid = if parent_stat.mode & MODE_SETGID != 0 {
            parent_stat.gid
        } else {
            attrs.gid
        };
        (
            attrs.uid,
            gid,
            prepare_created_file_mode(parent_stat, attrs),
        )
    });
    let (ino, leaf_name) = {
        let mut created = None;
        for _ in 0..TMPFILE_CREATE_ATTEMPTS {
            let seq = TMPFILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let leaf_name = format!(".whusp-tmpfile-{seq:x}");
            let result = inode_state::with_directory_mutation(directory.node, || {
                let ino = with_mount(
                    directory.node.mount_id,
                    BackendOp::NamespaceMutation,
                    |mount| match prepared_attrs {
                        Some((uid, gid, mode)) => mount.create_node_with_owner(
                            directory.node.ino,
                            leaf_name.as_str(),
                            FsNodeKind::RegularFile,
                            mode,
                            0,
                            uid,
                            gid,
                        ),
                        None => mount.create_file(directory.node.ino, leaf_name.as_str()),
                    },
                )
                .ok_or(FsError::Io)??;
                initialize_regular_file_page_cache_incarnation(
                    VfsNodeId::new(directory.node.mount_id, ino),
                    supports_page_cache,
                );
                Ok(ino)
            });
            match result {
                Ok(ino) => {
                    dentry_cache::invalidate_parent(directory.node);
                    created = Some((ino, leaf_name));
                    break;
                }
                Err(FsError::AlreadyExists) => continue,
                Err(err) => return Err(err),
            }
        }
        created.ok_or(FsError::AlreadyExists)?
    };

    let (readable, writable) = flags.read_write();
    let file = Arc::new(VfsFile::new(
        VfsPath::new(
            VfsNodeId::new(directory.node.mount_id, ino),
            FsNodeKind::RegularFile,
        ),
        Some(directory.node),
        readable,
        writable,
        OpenFlags::file_status_flags(flags),
        namespace_id,
        false,
    )?);

    let _unlink_mutation = begin_regular_file_page_cache_mutation(file.node, file.kind);
    match inode_state::with_directory_mutation(directory.node, || {
        with_mount(
            directory.node.mount_id,
            BackendOp::NamespaceMutation,
            |mount| mount.unlink(directory.node.ino, leaf_name.as_str()),
        )
        .ok_or(FsError::Io)?
    }) {
        Ok(()) => {
            inode_state::invalidate_metadata(file.node);
            dentry_cache::invalidate_parent(directory.node);
            Ok(file)
        }
        Err(err) => {
            drop(file);
            Err(err)
        }
    }
}

pub(crate) fn open_tmpfile_in_with_attrs(
    context: PathContext,
    name: &str,
    flags: OpenFlags,
    create_attrs: Option<FileCreateAttrs>,
) -> FsResult<Arc<dyn File + Send + Sync>> {
    let namespace_id = context.namespace_id();
    let directory = vfs_path::resolve_existing_in(context, name, LookupMode::FollowFinal)?;
    create_tmpfile_inode(namespace_id, directory, flags, create_attrs)
        .map(|file| file as Arc<dyn File + Send + Sync>)
}

pub(crate) fn open_file(name: &str, flags: OpenFlags) -> FsResult<Arc<VfsFile>> {
    open_vfs_file_impl(PathContext::global_root(), name, flags, None)
}

pub(crate) fn open_file_in(
    context: PathContext,
    name: &str,
    flags: OpenFlags,
) -> FsResult<Arc<dyn File + Send + Sync>> {
    open_file_in_with_attrs(context, name, flags, None)
}

pub(crate) fn open_file_in_with_attrs(
    context: PathContext,
    name: &str,
    flags: OpenFlags,
    create_attrs: Option<FileCreateAttrs>,
) -> FsResult<Arc<dyn File + Send + Sync>> {
    let follow_final_symlink = !flags.contains(OpenFlags::NOFOLLOW);
    let lookup_mode = if follow_final_symlink {
        LookupMode::FollowFinal
    } else {
        LookupMode::NoFollowFinal
    };
    if let Ok(path) = vfs_path::resolve_existing_in(context.clone(), name, lookup_mode) {
        if mount_is_devfs(path.node.mount_id) {
            if path.kind == FsNodeKind::Directory {
                return open_vfs_file_impl(context, name, flags, create_attrs)
                    .map(|file| file as Arc<dyn File + Send + Sync>);
            }
            return devfs::open_inode(path.node.mount_id, path.node.ino, flags);
        }
        if path.kind == FsNodeKind::Fifo {
            if flags.contains(OpenFlags::CREATE | OpenFlags::EXCL) {
                return Err(FsError::AlreadyExists);
            }
            if flags.contains(OpenFlags::DIRECTORY) {
                return Err(FsError::NotDir);
            }
            return open_named_fifo(path.node, OpenFlags::file_status_flags(flags));
        }
    }
    open_vfs_file_impl(context, name, flags, create_attrs)
        .map(|file| file as Arc<dyn File + Send + Sync>)
}

fn node_kind_from_mode(mode: u32) -> FsNodeKind {
    match mode & S_IFMT {
        S_IFDIR => FsNodeKind::Directory,
        S_IFREG => FsNodeKind::RegularFile,
        S_IFLNK => FsNodeKind::Symlink,
        S_IFIFO => FsNodeKind::Fifo,
        S_IFCHR => FsNodeKind::CharacterDevice,
        S_IFBLK => FsNodeKind::BlockDevice,
        S_IFSOCK => FsNodeKind::Socket,
        _ => FsNodeKind::Other,
    }
}

pub(crate) fn open_file_handle_node(
    node: VfsNodeId,
    flags: OpenFlags,
    namespace_id: MountNamespaceId,
) -> FsResult<Arc<dyn File + Send + Sync>> {
    if mount_is_devfs(node.mount_id) {
        return devfs::open_inode(node.mount_id, node.ino, flags);
    }

    if !mount_exists(node.mount_id) {
        return Err(FsError::NotFound);
    }
    let stat = stat_full_cached(node)?;
    let kind = node_kind_from_mode(stat.mode);
    if flags.contains(OpenFlags::DIRECTORY) && kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    if kind == FsNodeKind::Directory && !flags.can_open_directory() {
        return Err(FsError::IsDir);
    }
    if kind == FsNodeKind::Symlink && !flags.contains(OpenFlags::PATH) {
        return Err(FsError::Loop);
    }
    if kind == FsNodeKind::Fifo {
        return open_named_fifo(node, OpenFlags::file_status_flags(flags));
    }
    ensure_special_file_open_allowed(node.mount_id, kind, flags)?;

    let (readable, writable) = flags.read_write();
    if kind == FsNodeKind::RegularFile && writable && regular_file_node_is_executable(node) {
        return Err(FsError::TextBusy);
    }
    if kind == FsNodeKind::RegularFile && flags.contains(OpenFlags::TRUNC) && writable {
        ensure_mount_writable(node.mount_id)?;
        let _mutation = begin_regular_file_page_cache_mutation(node, kind);
        flush_dirty_regular_file(node)?;
        inode_state::with_mapping_mutation(node, || {
            with_mount(node.mount_id, BackendOp::TruncateAllocate, |mount| {
                mount.set_len(node.ino, 0)
            })
            .ok_or(FsError::Io)?
        })?;
    }

    Ok(Arc::new(VfsFile::new(
        VfsPath::new(node, kind),
        None,
        readable,
        writable,
        OpenFlags::file_status_flags(flags),
        namespace_id,
        false,
    )?))
}

pub(crate) fn link_open_file_in(
    file: Arc<dyn File + Send + Sync>,
    new_context: PathContext,
    new_name: &str,
) -> FsResult {
    let Some(file) = file.as_any().downcast_ref::<VfsFile>() else {
        return Err(FsError::CrossDevice);
    };
    link_node_in(file.node, file.kind, new_context, new_name)
}

pub(crate) fn stat_in(
    context: PathContext,
    name: &str,
    follow_final_symlink: bool,
) -> FsResult<FileStat> {
    stat_in_with(context, name, follow_final_symlink, false)
}

pub(crate) fn stat_full_in(
    context: PathContext,
    name: &str,
    follow_final_symlink: bool,
) -> FsResult<FileStat> {
    stat_in_with(context, name, follow_final_symlink, true)
}

/// Stats the common `dirfd + one regular filename` shape without constructing
/// a visible pathname. Non-regular/symlink/mount-overlay cases deliberately
/// fall back to the complete resolver in the syscall layer.
pub(crate) fn stat_direct_regular_child_in(
    namespace_id: MountNamespaceId,
    parent: WorkingDir,
    name: &str,
    full_stat: bool,
) -> FsResult<Option<FileStat>> {
    if let Some(stat) = inode_state::direct_stat_cache_lookup(namespace_id, parent, name) {
        return Ok(Some(stat));
    }
    for _ in 0..2 {
        let expected_epoch = inode_state::direct_stat_cache_epoch();
        let Some(node) = vfs_path::resolve_direct_regular_child_in(namespace_id, parent, name)?
        else {
            return Ok(None);
        };
        let expected_metadata_epoch = inode_state::direct_stat_metadata_epoch(node);
        let mut stat = if full_stat {
            stat_full_cached(node)?
        } else {
            stat_basic_cached(node)?
        };
        stat.dev = node.mount_id.0 as u64;
        overlay_dirty_regular_stat(node, &mut stat);
        if inode_state::direct_stat_cache_epoch() == expected_epoch
            && inode_state::direct_stat_metadata_epoch(node) == expected_metadata_epoch
        {
            inode_state::direct_stat_cache_insert(
                expected_epoch,
                expected_metadata_epoch,
                namespace_id,
                parent,
                name,
                node,
                stat,
            );
            return Ok(Some(stat));
        }
    }
    // A continuously mutating directory/inode is not eligible for this
    // immutable fast path; the complete resolver retains its normal behavior.
    Ok(None)
}

fn stat_in_with(
    context: PathContext,
    name: &str,
    follow_final_symlink: bool,
    full_stat: bool,
) -> FsResult<FileStat> {
    let mode = if follow_final_symlink {
        LookupMode::FollowFinal
    } else {
        LookupMode::NoFollowFinal
    };
    let path = {
        let _profile_scope = perf::time_scope(perf::ProfilePoint::StatPathLookup);
        vfs_path::resolve_existing_in(context, name, mode)?
    };
    let mut stat = {
        let _profile_scope = perf::time_scope(perf::ProfilePoint::StatPathBackendStat);
        if full_stat {
            stat_full_cached(path.node)?
        } else {
            stat_basic_cached(path.node)?
        }
    };
    stat.dev = path.node.mount_id.0 as u64;
    if path.kind == FsNodeKind::RegularFile {
        let _profile_scope = perf::time_scope(perf::ProfilePoint::StatPathDirtyOverlay);
        overlay_dirty_regular_stat(path.node, &mut stat);
    }
    Ok(stat)
}

pub(crate) fn lookup_path_in(
    context: PathContext,
    name: &str,
    follow_final_symlink: bool,
) -> FsResult<VfsPath> {
    let mode = if follow_final_symlink {
        LookupMode::FollowFinal
    } else {
        LookupMode::NoFollowFinal
    };
    vfs_path::resolve_existing_in(context, name, mode)
}

pub(crate) fn lookup_dir_with_stat_in(
    context: PathContext,
    name: &str,
) -> FsResult<(WorkingDir, FileStat)> {
    let path = vfs_path::resolve_existing_in(context, name, LookupMode::FollowFinal)?;
    if path.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    let mut stat = stat_full_cached(path.node)?;
    stat.dev = path.node.mount_id.0 as u64;
    Ok((WorkingDir::new(path.node.mount_id, path.node.ino), stat))
}

pub(crate) fn lookup_dir_with_stat_path_in(
    context: PathContext,
    name: &str,
) -> FsResult<(WorkingDir, FileStat, String)> {
    let path = vfs_path::resolve_existing_in(context, name, LookupMode::FollowFinal)?;
    if path.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    let mut stat = stat_full_cached(path.node)?;
    stat.dev = path.node.mount_id.0 as u64;
    let visible_path = path.visible_path.ok_or(FsError::NotFound)?;
    Ok((
        WorkingDir::new(path.node.mount_id, path.node.ino),
        stat,
        visible_path,
    ))
}

pub(crate) fn chmod_in(
    context: PathContext,
    name: &str,
    follow_final_symlink: bool,
    mode: u32,
) -> FsResult {
    let lookup_mode = if follow_final_symlink {
        LookupMode::FollowFinal
    } else {
        LookupMode::NoFollowFinal
    };
    let path = vfs_path::resolve_existing_in(context, name, lookup_mode)?;
    inode_state::with_metadata_update(
        path.node,
        inode_state::MetadataCacheUpdate::Mode(mode),
        || {
            with_mount(path.node.mount_id, BackendOp::NamespaceMutation, |mount| {
                mount.set_mode(path.node.ino, mode)
            })
            .ok_or(FsError::Io)?
        },
    )
}

pub(crate) fn chown_in(
    context: PathContext,
    name: &str,
    follow_final_symlink: bool,
    uid: Option<u32>,
    gid: Option<u32>,
) -> FsResult {
    let lookup_mode = if follow_final_symlink {
        LookupMode::FollowFinal
    } else {
        LookupMode::NoFollowFinal
    };
    let path = vfs_path::resolve_existing_in(context, name, lookup_mode)?;
    inode_state::with_metadata_update(
        path.node,
        inode_state::MetadataCacheUpdate::Owner { uid, gid },
        || {
            with_mount(path.node.mount_id, BackendOp::NamespaceMutation, |mount| {
                mount.set_owner(path.node.ino, uid, gid)
            })
            .ok_or(FsError::Io)?
        },
    )
}

pub(crate) fn truncate_in(context: PathContext, name: &str, len: usize) -> FsResult {
    let path = vfs_path::resolve_existing_in(context, name, LookupMode::FollowFinal)?;
    if path.kind == FsNodeKind::Directory {
        return Err(FsError::IsDir);
    }
    if path.kind != FsNodeKind::RegularFile {
        return Err(FsError::InvalidInput);
    }
    ensure_mount_writable(path.node.mount_id)?;
    let _mutation = begin_regular_file_page_cache_mutation(path.node, path.kind);
    flush_dirty_regular_file(path.node)?;
    inode_state::with_mapping_mutation(path.node, || {
        with_mount(path.node.mount_id, BackendOp::TruncateAllocate, |mount| {
            mount.set_len(path.node.ino, len as u64)
        })
        .ok_or(FsError::Io)?
    })
}

impl File for VfsFile {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        self.readable
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn read(&self, mut buf: UserBuffer) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        let mut offset = self.offset.lock();
        let mut total_read_size = 0usize;
        let has_dirty_pages = dirty_regular_file_has_pages(self.node);
        if !has_dirty_pages
            && let Some(read_size) = self.read_snapshot_user_buffer(&mut offset, &mut buf)
        {
            total_read_size = read_size;
        } else if let Some(read_size) = self.read_coalesced_user_buffer(&mut offset, &mut buf) {
            total_read_size = read_size;
        } else {
            for slice in buf.buffers.iter_mut() {
                let read_size = (if has_dirty_pages {
                    None
                } else {
                    self.read_snapshot_at(*offset, slice)
                })
                .or_else(|| self.read_dirty_regular_at(*offset, slice))
                .or_else(|| self.read_regular_cached_at(*offset, slice))
                .unwrap_or_else(|| self.read_backend_at_preserve_noatime(*offset, slice));
                if read_size == 0 {
                    break;
                }
                *offset += read_size;
                total_read_size += read_size;
            }
        }
        drop(offset);
        total_read_size
    }

    fn write(&self, buf: UserBuffer) -> usize {
        self.write_inner(buf, false)
    }

    fn write_append(&self, buf: UserBuffer) -> usize {
        self.write_inner(buf, true)
    }

    fn stat(&self) -> FsResult<FileStat> {
        let mut stat =
            stat_full_cached_with_state_and_lease(&self.inode_state, &self.mount_backend)?;
        stat.dev = self.node.mount_id.0 as u64;
        if self.kind == FsNodeKind::RegularFile {
            overlay_dirty_regular_stat(self.node, &mut stat);
        }
        Ok(stat)
    }

    fn mode_type(&self) -> FsResult<u32> {
        Ok(match self.kind {
            FsNodeKind::Directory => S_IFDIR,
            FsNodeKind::RegularFile => S_IFREG,
            FsNodeKind::Symlink => S_IFLNK,
            FsNodeKind::Fifo => S_IFIFO,
            FsNodeKind::CharacterDevice => S_IFCHR,
            FsNodeKind::BlockDevice => S_IFBLK,
            FsNodeKind::Socket => S_IFSOCK,
            FsNodeKind::Other => 0,
        })
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        let has_dirty_pages = dirty_regular_file_has_pages(self.node);
        (if has_dirty_pages {
            None
        } else {
            self.read_snapshot_at(offset, buf)
        })
        .or_else(|| self.read_dirty_regular_at(offset, buf))
        .or_else(|| self.read_regular_cached_at(offset, buf))
        .unwrap_or_else(|| self.read_backend_at_preserve_noatime(offset, buf))
    }

    fn populate_clean_page_cache_at(&self, offset: usize) -> bool {
        if offset % PAGE_SIZE != 0 {
            return false;
        }
        let mut probe = [0u8; 1];
        self.read_regular_cached_at(offset, &mut probe) == Some(1)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        *self.read_snapshot.lock() = None;
        let _mutation = (!buf.is_empty())
            .then(|| begin_regular_file_page_cache_mutation(self.node, self.kind))
            .flatten();
        self.write_at_chunks(offset, buf)
    }

    fn supports_aligned_user_buffer_write_at(&self, offset: usize, len: usize) -> bool {
        can_cache_dirty_user_buffer_write(
            self.kind,
            self.supports_dirty_writeback,
            offset,
            len,
            self.status_flags.get(),
        )
    }

    fn write_at_aligned_user_buffer(&self, offset: usize, buf: UserBuffer) -> FsResult<usize> {
        let len = buf.len();
        if !self.supports_aligned_user_buffer_write_at(offset, len) {
            return Err(FsError::Unsupported);
        }
        self.check_write_at(offset, len)?;
        *self.read_snapshot.lock() = None;
        let _mutation = begin_regular_file_page_cache_mutation(self.node, self.kind);
        let mut pressure_retried = false;
        loop {
            match cache_dirty_regular_user_buffer_write(self.node, offset, &buf) {
                DirtyCacheWriteResult::Cached(write_size) => return Ok(write_size),
                DirtyCacheWriteResult::NeedsPressureFlush if !pressure_retried => {
                    flush_dirty_regular_files_for_pressure(None)?;
                    pressure_retried = true;
                }
                DirtyCacheWriteResult::NeedsPressureFlush | DirtyCacheWriteResult::Fallback => {
                    return Err(FsError::Io);
                }
            }
        }
    }

    fn supports_aligned_user_buffer_write(&self, len: usize, append: bool) -> bool {
        let offset = self.offset.lock();
        let write_offset = if append {
            let Ok(stat) = self.stat() else {
                return false;
            };
            stat_logical_size(self.node, stat.size) as usize
        } else {
            *offset
        };
        self.supports_aligned_user_buffer_write_at(write_offset, len)
    }

    fn write_aligned_user_buffer(&self, buf: UserBuffer, append: bool) -> FsResult<usize> {
        let len = buf.len();
        let mut offset = self.offset.lock();
        if append {
            let stat =
                stat_full_cached_with_state_and_lease(&self.inode_state, &self.mount_backend)?;
            *offset = stat_logical_size(self.node, stat.size) as usize;
        }
        let write_offset = *offset;
        if !self.supports_aligned_user_buffer_write_at(write_offset, len) {
            return Err(FsError::Unsupported);
        }
        self.check_write_at(write_offset, len)?;
        *self.read_snapshot.lock() = None;
        let _mutation = begin_regular_file_page_cache_mutation(self.node, self.kind);
        let mut pressure_retried = false;
        let mut offset_advanced = false;
        let write_size = loop {
            match cache_dirty_regular_user_buffer_write(self.node, write_offset, &buf) {
                DirtyCacheWriteResult::Cached(write_size) => break write_size,
                DirtyCacheWriteResult::NeedsPressureFlush if !pressure_retried => {
                    if flush_dirty_regular_files_for_pressure(None).is_ok() {
                        pressure_retried = true;
                        continue;
                    }
                    offset_advanced = true;
                    break self.write_coalesced_user_buffer(&mut offset, &buf);
                }
                DirtyCacheWriteResult::NeedsPressureFlush | DirtyCacheWriteResult::Fallback => {
                    offset_advanced = true;
                    break self.write_coalesced_user_buffer(&mut offset, &buf);
                }
            }
        };
        if !offset_advanced {
            *offset = (*offset).checked_add(write_size).unwrap_or(usize::MAX);
        }
        Ok(write_size)
    }

    fn set_len(&self, len: usize) -> FsResult {
        if self.kind != FsNodeKind::RegularFile {
            return Err(FsError::InvalidInput);
        }
        if !self.writable {
            return Err(FsError::PermissionDenied);
        }
        self.check_set_len(len)?;
        let _mutation = begin_regular_file_page_cache_mutation(self.node, self.kind);
        flush_dirty_regular_file(self.node)?;
        inode_state::with_mapping_mutation_state(&self.inode_state, || {
            self.with_backend(BackendOp::TruncateAllocate, |mount| {
                mount.set_len(self.node.ino, len as u64)
            })
        })
    }

    fn allocate_range(&self, offset: usize, len: usize, keep_size: bool) -> FsResult {
        if self.kind != FsNodeKind::RegularFile {
            return Err(FsError::InvalidInput);
        }
        if !self.writable {
            return Err(FsError::PermissionDenied);
        }
        self.check_write_at(offset, len)?;
        let _mutation = begin_regular_file_page_cache_mutation(self.node, self.kind);
        flush_dirty_regular_file(self.node)?;
        inode_state::with_mapping_mutation_state(&self.inode_state, || {
            self.with_backend(BackendOp::TruncateAllocate, |mount| {
                mount.allocate_range(self.node.ino, offset as u64, len as u64, keep_size)
            })
        })
    }

    fn zero_range(&self, offset: usize, len: usize, keep_size: bool) -> FsResult {
        if self.kind != FsNodeKind::RegularFile {
            return Err(FsError::InvalidInput);
        }
        if !self.writable {
            return Err(FsError::PermissionDenied);
        }
        self.check_write_at(offset, len)?;
        let _mutation = begin_regular_file_page_cache_mutation(self.node, self.kind);
        flush_dirty_regular_file(self.node)?;
        inode_state::with_mapping_mutation_state(&self.inode_state, || {
            self.with_backend(BackendOp::TruncateAllocate, |mount| {
                mount.zero_range(self.node.ino, offset as u64, len as u64, keep_size)
            })
        })
    }

    fn punch_hole(&self, offset: usize, len: usize) -> FsResult {
        if self.kind != FsNodeKind::RegularFile {
            return Err(FsError::InvalidInput);
        }
        if !self.writable {
            return Err(FsError::PermissionDenied);
        }
        ensure_mount_writable(self.node.mount_id)?;
        let flags = self.inode_flags_or_empty()?;
        if flags & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0 {
            return Err(FsError::PermissionDenied);
        }
        let _mutation = begin_regular_file_page_cache_mutation(self.node, self.kind);
        flush_dirty_regular_file(self.node)?;
        inode_state::with_mapping_mutation_state(&self.inode_state, || {
            self.with_backend(BackendOp::TruncateAllocate, |mount| {
                mount.punch_hole(self.node.ino, offset as u64, len as u64)
            })
        })
    }

    fn sync(&self, data_only: bool) -> FsResult {
        flush_dirty_regular_file(self.node)?;
        self.with_backend(BackendOp::Sync, |mount| {
            mount.sync(self.node.ino, data_only)
        })
    }

    fn seek(&self, offset: i64, whence: SeekWhence) -> FsResult<usize> {
        let mut current = self.offset.lock();
        let base = match whence {
            SeekWhence::Set => 0i128,
            SeekWhence::Current => *current as i128,
            SeekWhence::End => {
                let stat =
                    stat_full_cached_with_state_and_lease(&self.inode_state, &self.mount_backend)?;
                stat_logical_size(self.node, stat.size) as i128
            }
            SeekWhence::Data | SeekWhence::Hole => {
                if offset < 0 {
                    return Err(FsError::InvalidInput);
                }
                let next = self.seek_data_or_hole(offset as usize, whence == SeekWhence::Hole)?;
                *current = next;
                return Ok(next);
            }
        };
        let new_offset = base
            .checked_add(offset as i128)
            .ok_or(FsError::InvalidInput)?;
        if new_offset < 0 || new_offset > usize::MAX as i128 || new_offset > isize::MAX as i128 {
            return Err(FsError::InvalidInput);
        }
        *current = new_offset as usize;
        Ok(*current)
    }

    fn read_dirent64(&self, user_buf: UserBuffer) -> FsResult<isize> {
        if self.kind != FsNodeKind::Directory {
            return Err(FsError::NotDir);
        }
        let mut offset = self.offset.lock();
        let user_buf_len = user_buf.len();
        let mut kernel_buf = vec![0u8; user_buf_len.min(VFS_DIRENT_SCRATCH_MAX)];
        let current_offset = *offset as u64;
        let (read_size, next_offset) = if current_offset >= SYNTHETIC_DIRENT_OFFSET_BASE {
            self.read_synthetic_dirent64(
                current_offset - SYNTHETIC_DIRENT_OFFSET_BASE,
                &mut kernel_buf,
            )?
        } else {
            let (read_size, next_offset) =
                inode_state::with_directory_read_state(&self.inode_state, |directory_version| {
                    {
                        let cached = self.directory_snapshot.lock();
                        if let Some(cached) = cached
                            .as_ref()
                            .filter(|cached| cached.version == directory_version)
                        {
                            return cached
                                .snapshot
                                .read_dirent64(current_offset, &mut kernel_buf);
                        }
                    }
                    let plan = self.with_backend(BackendOp::ReadPlan, |mount| {
                        mount.prepare_directory_read_plan(self.node.ino, 0)
                    });
                    if let Some(plan) = plan {
                        let snapshot = plan.execute()?;
                        let result = snapshot.read_dirent64(current_offset, &mut kernel_buf);
                        *self.directory_snapshot.lock() = Some(CachedDirectorySnapshot {
                            version: directory_version,
                            snapshot,
                        });
                        result
                    } else {
                        self.with_backend(BackendOp::Readdir, |mount| {
                            mount.read_dirent64(self.node.ino, current_offset, &mut kernel_buf)
                        })
                    }
                })?;
            if read_size == 0 {
                // Synthetic mountpoint dirents are appended after backend EOF
                // and resume from a disjoint high offset range, so real
                // filesystem offsets never collide with VFS overlay entries.
                self.read_synthetic_dirent64(0, &mut kernel_buf)?
            } else {
                (read_size, next_offset)
            }
        };
        perf::record_vfs_dirent_read(user_buf_len, kernel_buf.len(), read_size);
        if read_size == 0 {
            return Ok(0);
        }
        self.touch_directory_atime();
        let mut user_buf = user_buf;
        let copied = user_buf.copy_from_slice(&kernel_buf[..read_size]);
        debug_assert_eq!(copied, read_size);
        *offset = next_offset as usize;
        Ok(read_size as isize)
    }

    fn readlink(&self, buf: &mut [u8]) -> FsResult<usize> {
        if self.kind != FsNodeKind::Symlink {
            return Err(FsError::InvalidInput);
        }
        inode_state::with_mapping_read_state(&self.inode_state, || {
            let plan = self.with_backend(BackendOp::ReadPlan, |mount| {
                mount.prepare_readlink_plan(self.node.ino, buf.len())
            });
            if let Some(plan) = plan {
                Ok(plan.execute(buf))
            } else {
                self.with_backend(BackendOp::Readlink, |mount| {
                    mount.readlink(self.node.ino, buf)
                })
            }
        })
    }

    fn proc_fd_target(&self) -> Option<String> {
        self.visible_path.clone()
    }

    fn set_times(
        &self,
        atime: Option<FileTimestamp>,
        mtime: Option<FileTimestamp>,
        ctime: FileTimestamp,
    ) -> FsResult {
        inode_state::with_metadata_update_state(
            &self.inode_state,
            inode_state::MetadataCacheUpdate::Times {
                atime,
                mtime,
                ctime,
            },
            || {
                self.with_backend(BackendOp::NamespaceMutation, |mount| {
                    mount.set_times(self.node.ino, atime, mtime, ctime)
                })
            },
        )
    }

    fn set_mode(&self, mode: u32) -> FsResult {
        inode_state::with_metadata_update_state(
            &self.inode_state,
            inode_state::MetadataCacheUpdate::Mode(mode),
            || {
                self.with_backend(BackendOp::NamespaceMutation, |mount| {
                    mount.set_mode(self.node.ino, mode)
                })
            },
        )
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> FsResult {
        inode_state::with_metadata_update_state(
            &self.inode_state,
            inode_state::MetadataCacheUpdate::Owner { uid, gid },
            || {
                self.with_backend(BackendOp::NamespaceMutation, |mount| {
                    mount.set_owner(self.node.ino, uid, gid)
                })
            },
        )
    }

    fn inode_flags(&self) -> FsResult<u32> {
        self.with_backend(BackendOp::StatFull, |mount| {
            mount.inode_flags(self.node.ino)
        })
    }

    fn set_inode_flags(&self, flags: u32) -> FsResult {
        let result = inode_state::with_metadata_update_state(
            &self.inode_state,
            inode_state::MetadataCacheUpdate::InodeFlags(flags),
            || {
                self.with_backend(BackendOp::NamespaceMutation, |mount| {
                    mount.set_inode_flags(self.node.ino, flags)
                })
            },
        );
        if result.is_ok() {
            update_inode_flags_cache(self.node, flags);
        }
        result
    }

    fn check_write(&self, len: usize, append: bool) -> FsResult {
        ensure_mount_writable(self.node.mount_id)?;
        let flags = self.inode_flags_or_empty()?;
        if flags & FS_IMMUTABLE_FL != 0 {
            return Err(FsError::PermissionDenied);
        }
        if flags & FS_APPEND_FL != 0 && !append {
            return Err(FsError::PermissionDenied);
        }
        let offset = if append {
            let stat =
                stat_full_cached_with_state_and_lease(&self.inode_state, &self.mount_backend)?.size;
            stat_logical_size(self.node, stat)
        } else {
            *self.offset.lock() as u64
        };
        self.with_backend(BackendOp::Write, |mount| {
            mount.check_write_at(self.node.ino, offset, len)
        })
    }

    fn check_write_at(&self, offset: usize, len: usize) -> FsResult {
        ensure_mount_writable(self.node.mount_id)?;
        let flags = self.inode_flags_or_empty()?;
        if flags & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0 {
            return Err(FsError::PermissionDenied);
        }
        self.with_backend(BackendOp::Write, |mount| {
            mount.check_write_at(self.node.ino, offset as u64, len)
        })
    }

    fn check_set_len(&self, len: usize) -> FsResult {
        ensure_mount_writable(self.node.mount_id)?;
        let flags = self.inode_flags_or_empty()?;
        if flags & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0 {
            return Err(FsError::PermissionDenied);
        }
        self.with_backend(BackendOp::TruncateAllocate, |mount| {
            mount.check_set_len(self.node.ino, len as u64)
        })
    }

    fn working_dir(&self) -> Option<WorkingDir> {
        if self.kind != FsNodeKind::Directory {
            return None;
        }
        Some(WorkingDir::new(self.node.mount_id, self.node.ino))
    }

    fn vfs_node_id(&self) -> Option<VfsNodeId> {
        Some(self.node)
    }

    fn vfs_parent_node_id(&self) -> Option<VfsNodeId> {
        self.parent
    }

    fn vfs_mount_id(&self) -> Option<super::super::mount::MountId> {
        Some(self.node.mount_id)
    }

    fn is_devfs_dir(&self) -> bool {
        self.kind == FsNodeKind::Directory && mount_is_devfs(self.node.mount_id)
    }

    fn is_devfs_misc_dir(&self) -> bool {
        mount_is_devfs(self.node.mount_id) && devfs::inode_is_misc_dir(self.node.ino)
    }

    fn is_devfs_pts_dir(&self) -> bool {
        mount_is_devfs(self.node.mount_id) && devfs::inode_is_pts_dir(self.node.ino)
    }

    fn page_cache_id(&self) -> Option<PageCacheId> {
        page_cache_id_for_node_with_support(self.node, self.kind, self.supports_page_cache)
    }

    fn inc_writable_shared_mmap(&self) {
        track_writable_shared_regular_mmap(self.node, self.kind);
    }

    fn dec_writable_shared_mmap(&self) {
        untrack_writable_shared_regular_mmap(self.node, self.kind);
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }

    fn clone_for_fanotify_event(&self, flags: OpenFlags) -> FsResult<Arc<dyn File + Send + Sync>> {
        let (readable, writable) = flags.read_write();
        Ok(Arc::new(VfsFile::new(
            VfsPath {
                node: self.node,
                kind: self.kind,
                visible_path: self.visible_path.clone(),
            },
            self.parent,
            readable,
            writable,
            OpenFlags::file_status_flags(flags),
            self.namespace_id,
            true,
        )?))
    }

    fn suppresses_fanotify(&self) -> bool {
        self.suppress_fanotify
    }
}

impl Drop for VfsFile {
    fn drop(&mut self) {
        untrack_writable_regular_open(self.node, self.kind, self.writable);
        invalidate_inode_flags_cache(self.node);
        release_inode_from_drop_with_lease(&self.inode_state, &self.mount_backend);
    }
}
