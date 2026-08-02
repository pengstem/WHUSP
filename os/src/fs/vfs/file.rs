use super::super::dentry_cache;
use super::super::devfs;
use super::super::dirent::{DT_DIR, RawDirEntry, write_dir_entries_with_offset_base};
use super::super::inode::{OpenFlags, ensure_backend_create_target_absent, link_node_in};
#[cfg(feature = "perf-counters")]
use super::super::inode_state::dirty_regular_file_count;
use super::super::inode_state::{
    self, DirtyFileCache, DirtyPage, dirty_inode_states_on_mount, dirty_page_count,
    dirty_pressure_candidates, lock_dirty_file, record_dirty_overlay_locked_alloc,
    record_dirty_overlay_locked_copy, record_dirty_overlay_pressure_batch, register_dirty_inode,
    release_dirty_pages, restore_dirty_pages, set_dirty_page_count, take_dirty_inode,
    total_dirty_pages, try_reserve_dirty_pages,
};
use super::super::mount::{
    MountId, MountNamespaceId, MountedBackendLease, mount_any_nosymfollow, mount_exists,
    mount_is_devfs, mount_is_noatime, mount_is_nodev, mount_is_nodiratime, mount_is_nosymfollow,
    mount_is_read_only, mount_supports_dirty_writeback, mount_supports_page_cache,
    mounted_backend_lease, release_inode_from_drop, release_inode_from_drop_with_lease,
    retain_inode, retain_inode_with_lease, stat_basic_cached,
    stat_basic_cached_with_state_and_lease, stat_full_cached,
    stat_full_cached_with_state_and_lease, static_mount_children_for_dir, with_mount,
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
const VFS_DIRTY_PRESSURE_MAX_INODES: usize = 16;
const VFS_DIRTY_PRESSURE_MAX_PAGES: usize = 256;
const MODE_PERMISSIONS_MASK: u32 = 0o7777;
const MODE_SETGID: u32 = 0o2000;
const TMPFILE_CREATE_ATTEMPTS: usize = 64;
const OPEN_CREATE_RACE_ATTEMPTS: usize = 16;
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
    pub(crate) overlay_lock_calls: usize,
    pub(crate) overlay_lock_contended: usize,
    pub(crate) overlay_lock_wait_ticks: usize,
    pub(crate) overlay_lock_hold_ticks: usize,
    pub(crate) overlay_global_scan_calls: usize,
    pub(crate) overlay_global_scan_files: usize,
    pub(crate) overlay_global_scan_pages: usize,
    pub(crate) overlay_locked_alloc_bytes: usize,
    pub(crate) overlay_locked_copy_bytes: usize,
    pub(crate) pressure_candidates: usize,
    pub(crate) pressure_batch_inodes_max: usize,
    pub(crate) pressure_batch_pages_max: usize,
    pub(crate) pressure_budget_stops: usize,
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
enum DirtyChunkWriteResult {
    Written(usize),
    NeedsPressureFlush,
    Failed,
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
    let counters = DIRTY_WRITEBACK_COUNTERS.lock();
    let overlay = inode_state::dirty_overlay_stats_snapshot();
    let dirty_files = dirty_regular_file_count();
    let dirty_pages = total_dirty_pages();
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
        overlay_lock_calls: overlay.lock_calls,
        overlay_lock_contended: overlay.lock_contended,
        overlay_lock_wait_ticks: overlay.lock_wait_ticks,
        overlay_lock_hold_ticks: overlay.lock_hold_ticks,
        overlay_global_scan_calls: overlay.global_scan_calls,
        overlay_global_scan_files: overlay.global_scan_files,
        overlay_global_scan_pages: overlay.global_scan_pages,
        overlay_locked_alloc_bytes: overlay.locked_alloc_bytes,
        overlay_locked_copy_bytes: overlay.locked_copy_bytes,
        pressure_candidates: overlay.pressure_candidates,
        pressure_batch_inodes_max: overlay.pressure_batch_inodes_max,
        pressure_batch_pages_max: overlay.pressure_batch_pages_max,
        pressure_budget_stops: overlay.pressure_budget_stops,
    }
}

fn dirty_logical_size(state: &inode_state::InodeState) -> Option<usize> {
    if dirty_page_count(state) == 0 {
        return None;
    }
    let dirty = lock_dirty_file(state);
    (!dirty.pages.is_empty()).then_some(dirty.logical_size)
}

fn dirty_or_backend_logical_size(state: &inode_state::InodeState) -> Option<usize> {
    if let Some(size) = dirty_logical_size(state) {
        return Some(size);
    }
    stat_full_cached(state.node())
        .ok()
        .map(|stat| stat.size as usize)
}

fn any_regular_file_dirty() -> bool {
    inode_state::any_regular_file_dirty()
}

fn dirty_regular_file_has_pages_state(state: &inode_state::InodeState) -> bool {
    dirty_page_count(state) != 0
}

fn dirty_regular_file_has_pages(node: VfsNodeId) -> bool {
    if !any_regular_file_dirty() {
        return false;
    }
    dirty_regular_file_has_pages_state(&inode_state::state_for(node))
}

fn overlay_dirty_regular_stat_state(state: &inode_state::InodeState, stat: &mut FileStat) {
    if !dirty_regular_file_has_pages_state(state) {
        return;
    }
    let dirty = lock_dirty_file(state);
    stat.size = dirty.logical_size as u64;
    let dirty_blocks = dirty.pages.len().saturating_mul(PAGE_SIZE).div_ceil(512) as u64;
    stat.blocks = stat.blocks.max(dirty_blocks);
    stat.mtime_sec = dirty.mtime.sec;
    stat.mtime_nsec = dirty.mtime.nsec;
    stat.ctime_sec = dirty.ctime.sec;
    stat.ctime_nsec = dirty.ctime.nsec;
}

fn stat_logical_size(node: VfsNodeId, stat_size: u64) -> u64 {
    dirty_logical_size(&inode_state::state_for(node))
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

fn dirty_write_existing_pages(
    dirty: &DirtyFileCache,
    page_start: usize,
    page_count: usize,
) -> Vec<bool> {
    (0..page_count)
        .map(|page_offset| dirty.pages.contains_key(&(page_start + page_offset)))
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
    record_dirty_overlay_locked_copy(copy_len);
    page.mark_dirty(dst_start, dst_start + copy_len);
}

fn cache_dirty_regular_write(
    state: &Arc<inode_state::InodeState>,
    offset: usize,
    buf: &[u8],
) -> DirtyCacheWriteResult {
    if buf.is_empty() {
        return DirtyCacheWriteResult::Cached(0);
    }
    let node = state.node();
    let Some(logical_size) = dirty_or_backend_logical_size(state) else {
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
        let dirty = lock_dirty_file(state);
        dirty_write_existing_pages(&dirty, page_start, page_count)
    };
    let Some(mut prepared_pages) = prepare_dirty_regular_pages(offset, buf, &existing_pages) else {
        return DirtyCacheWriteResult::Fallback;
    };
    let needs_pin = {
        let dirty = lock_dirty_file(state);
        dirty.pages.is_empty()
    };
    let retained_pin = if needs_pin {
        match retain_inode(node) {
            Ok(pin) if Arc::ptr_eq(&pin, state) => Some(pin),
            Ok(pin) => {
                release_inode_from_drop(&pin);
                return DirtyCacheWriteResult::Fallback;
            }
            Err(_) => return DirtyCacheWriteResult::Fallback,
        }
    } else {
        None
    };

    let timestamp = FileTimestamp::now();
    let mut dirty = lock_dirty_file(state);
    let new_pages = (0..page_count)
        .filter(|page_delta| !dirty.pages.contains_key(&(page_start + page_delta)))
        .count();
    let Some(current_dirty_pages) =
        try_reserve_dirty_pages(new_pages, VFS_DIRTY_WRITEBACK_MAX_PAGES)
    else {
        drop(dirty);
        if let Some(pin) = retained_pin.as_ref() {
            release_inode_from_drop(pin);
        }
        return DirtyCacheWriteResult::NeedsPressureFlush;
    };
    let missing_prepared_page = (0..page_count).any(|page_delta| {
        let page_index = page_start + page_delta;
        !dirty.pages.contains_key(&page_index) && !prepared_pages.contains_key(&page_index)
    });
    if missing_prepared_page {
        drop(dirty);
        release_dirty_pages(new_pages);
        if let Some(pin) = retained_pin.as_ref() {
            release_inode_from_drop(pin);
        }
        return DirtyCacheWriteResult::Fallback;
    }

    let first_dirty = dirty.pages.is_empty();
    if first_dirty && retained_pin.is_none() {
        drop(dirty);
        release_dirty_pages(new_pages);
        return DirtyCacheWriteResult::Fallback;
    }
    let release_extra_pin = !first_dirty && retained_pin.is_some();
    if first_dirty {
        dirty.logical_size = logical_size;
    }
    dirty.logical_size = dirty.logical_size.max(end);
    dirty.mtime = timestamp;
    dirty.ctime = timestamp;
    for page_delta in 0..page_count {
        let page_index = page_start + page_delta;
        match dirty.pages.get_mut(&page_index) {
            Some(existing) => merge_dirty_page_write(page_index, existing, offset, buf),
            None => {
                let Some(page) = prepared_pages.remove(&page_index) else {
                    continue;
                };
                dirty.pages.insert(page_index, page);
            }
        }
    }
    set_dirty_page_count(state, dirty.pages.len());
    if first_dirty {
        let pin = retained_pin
            .as_ref()
            .expect("new dirty inode lost its backend pin");
        register_dirty_inode(Arc::clone(pin));
    }
    drop(dirty);
    inode_state::invalidate_direct_stat_cache();
    if release_extra_pin && let Some(pin) = retained_pin.as_ref() {
        release_inode_from_drop(pin);
    }

    record_dirty_cache_write(page_count, buf.len());
    record_dirty_cache_peak(current_dirty_pages);
    DirtyCacheWriteResult::Cached(buf.len())
}

fn cache_dirty_regular_user_buffer_write(
    state: &Arc<inode_state::InodeState>,
    offset: usize,
    buf: &UserBuffer,
) -> DirtyCacheWriteResult {
    let len = buf.len();
    if len == 0 {
        return DirtyCacheWriteResult::Cached(0);
    }
    if buf.segments().any(|slice| slice.len() % PAGE_SIZE != 0) {
        return DirtyCacheWriteResult::Fallback;
    }
    let node = state.node();
    let Some(logical_size) = dirty_or_backend_logical_size(state) else {
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
        let dirty = lock_dirty_file(state);
        dirty.pages.is_empty()
    };
    let retained_pin = if needs_pin {
        match retain_inode(node) {
            Ok(pin) if Arc::ptr_eq(&pin, state) => Some(pin),
            Ok(pin) => {
                release_inode_from_drop(&pin);
                return DirtyCacheWriteResult::Fallback;
            }
            Err(_) => return DirtyCacheWriteResult::Fallback,
        }
    } else {
        None
    };

    let timestamp = FileTimestamp::now();
    let mut dirty = lock_dirty_file(state);
    let new_pages = (0..page_count)
        .filter(|page_delta| !dirty.pages.contains_key(&(page_start + page_delta)))
        .count();
    let Some(current_dirty_pages) =
        try_reserve_dirty_pages(new_pages, VFS_DIRTY_WRITEBACK_MAX_PAGES)
    else {
        drop(dirty);
        if let Some(pin) = retained_pin.as_ref() {
            release_inode_from_drop(pin);
        }
        return DirtyCacheWriteResult::NeedsPressureFlush;
    };
    let first_dirty = dirty.pages.is_empty();
    if first_dirty && retained_pin.is_none() {
        drop(dirty);
        release_dirty_pages(new_pages);
        return DirtyCacheWriteResult::Fallback;
    }
    let release_extra_pin = !first_dirty && retained_pin.is_some();
    if first_dirty {
        dirty.logical_size = logical_size;
    }
    dirty.logical_size = dirty.logical_size.max(end);
    dirty.mtime = timestamp;
    dirty.ctime = timestamp;
    let mut page_index = page_start;
    for source in buf.segments() {
        for chunk in source.chunks(PAGE_SIZE) {
            record_dirty_overlay_locked_alloc(chunk.len());
            dirty
                .pages
                .insert(page_index, DirtyPage::full(chunk.to_vec()));
            page_index += 1;
        }
    }
    set_dirty_page_count(state, dirty.pages.len());
    if first_dirty {
        let pin = retained_pin
            .as_ref()
            .expect("new dirty inode lost its backend pin");
        register_dirty_inode(Arc::clone(pin));
    }
    drop(dirty);
    inode_state::invalidate_direct_stat_cache();
    if release_extra_pin && let Some(pin) = retained_pin.as_ref() {
        release_inode_from_drop(pin);
    }

    record_dirty_cache_write(page_count, len);
    record_dirty_cache_peak(current_dirty_pages);
    DirtyCacheWriteResult::Cached(len)
}

fn overlay_dirty_regular_read(node: VfsNodeId, offset: usize, buf: &mut [u8]) -> Option<usize> {
    if buf.is_empty() {
        return Some(0);
    }
    let state = inode_state::state_for(node);
    if !dirty_regular_file_has_pages_state(&state) {
        return None;
    }
    let cache = lock_dirty_file(&state);
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
            record_dirty_overlay_locked_copy(len);
            buf[dst_start..dst_start + len].copy_from_slice(&page.data[src_start..src_start + len]);
        }
    }
    Some(read_len)
}

fn dirty_regular_read_len(node: VfsNodeId, offset: usize, len: usize) -> Option<usize> {
    if len == 0 {
        return Some(0);
    }
    let state = inode_state::state_for(node);
    if !dirty_regular_file_has_pages_state(&state) {
        return None;
    }
    let cache = lock_dirty_file(&state);
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
    mtime: FileTimestamp,
    ctime: FileTimestamp,
    pages: BTreeMap<usize, DirtyPage>,
    runs: Vec<DirtyWritebackRun>,
    owns_inode_pin: bool,
}

fn build_dirty_writeback_runs(pages: &BTreeMap<usize, DirtyPage>) -> Vec<DirtyWritebackRun> {
    let mut runs = Vec::new();
    let mut current_offset = 0usize;
    let mut current_data = Vec::new();
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
    runs
}

fn collect_dirty_writeback(
    state: &Arc<inode_state::InodeState>,
    page_budget: usize,
) -> Option<DirtyWritebackBatch> {
    if page_budget == 0 || !dirty_regular_file_has_pages_state(state) {
        return None;
    }
    let mut dirty = lock_dirty_file(state);
    if dirty.pages.is_empty() {
        set_dirty_page_count(state, 0);
        return None;
    }
    let page_indexes = dirty
        .pages
        .keys()
        .take(page_budget)
        .copied()
        .collect::<Vec<_>>();
    let mut pages = BTreeMap::new();
    for page_index in page_indexes {
        let page = dirty
            .pages
            .remove(&page_index)
            .expect("selected dirty page disappeared");
        pages.insert(page_index, page);
    }
    let logical_size = dirty.logical_size;
    let mtime = dirty.mtime;
    let ctime = dirty.ctime;
    set_dirty_page_count(state, dirty.pages.len());
    release_dirty_pages(pages.len());
    let drained_pin = if dirty.pages.is_empty() {
        Some(take_dirty_inode(state).expect("drained dirty inode lost its queue-owned backend pin"))
    } else {
        None
    };
    drop(dirty);
    inode_state::invalidate_direct_stat_cache();

    let runs = build_dirty_writeback_runs(&pages);
    let owns_inode_pin = drained_pin.is_some();
    let inode_state = drained_pin.unwrap_or_else(|| Arc::clone(state));
    Some(DirtyWritebackBatch {
        inode_state,
        logical_size,
        mtime,
        ctime,
        pages,
        runs,
        owns_inode_pin,
    })
}

fn newer_timestamp(left: FileTimestamp, right: FileTimestamp) -> FileTimestamp {
    if (left.sec, left.nsec) >= (right.sec, right.nsec) {
        left
    } else {
        right
    }
}

fn restore_dirty_writeback(batch: DirtyWritebackBatch) {
    let DirtyWritebackBatch {
        inode_state,
        logical_size,
        mtime,
        ctime,
        pages,
        runs: _,
        owns_inode_pin,
    } = batch;
    let mut dirty = lock_dirty_file(&inode_state);
    let was_clean = dirty.pages.is_empty();
    dirty.logical_size = dirty.logical_size.max(logical_size);
    dirty.mtime = newer_timestamp(dirty.mtime, mtime);
    dirty.ctime = newer_timestamp(dirty.ctime, ctime);
    let mut restored_pages = 0usize;
    for (page_index, page) in pages {
        if let alloc::collections::btree_map::Entry::Vacant(entry) = dirty.pages.entry(page_index) {
            entry.insert(page);
            restored_pages += 1;
        }
    }
    restore_dirty_pages(restored_pages);
    set_dirty_page_count(&inode_state, dirty.pages.len());
    let release_batch_pin = owns_inode_pin && !was_clean;
    if owns_inode_pin && was_clean {
        register_dirty_inode(Arc::clone(&inode_state));
    }
    drop(dirty);
    inode_state::invalidate_direct_stat_cache();
    if release_batch_pin {
        release_inode_from_drop(&inode_state);
    }
}

fn write_backend_at(node: VfsNodeId, offset: u64, data: &[u8]) -> Option<usize> {
    let plan = with_mount(node.mount_id, BackendOp::Write, |mount| {
        mount.prepare_write_plan(node.ino, offset, data.len())
    })
    .flatten();
    if let Some(plan) = plan {
        return Some(plan.execute(data));
    }
    with_mount(node.mount_id, BackendOp::Write, |mount| {
        mount.write_at(node.ino, data, offset)
    })
}

/// Flushes one dirty file while its inode mapping-mutation lease is held.
///
/// The caller must hold this inode's mapping-mutation lease. Cross-inode
/// pressure reclaim acquires each candidate independently, so backend I/O
/// never nests a second inode mapping lease below the writer that hit pressure.
fn flush_dirty_regular_file_for_reason_under_mapping_state(
    state: &Arc<inode_state::InodeState>,
    reason: DirtyFlushReason,
    page_budget: usize,
) -> FsResult<usize> {
    if !dirty_regular_file_has_pages_state(state) {
        return Ok(0);
    }
    let node = state.node();
    // Removing the dirty overlay before its runs reach the backend creates a
    // temporary window where an unguarded reader could observe old disk data.
    // Keep the inode generation unstable through collect, write, and restore.
    let _mutation = begin_regular_file_page_cache_mutation(node, FsNodeKind::RegularFile);
    let Some(batch) = collect_dirty_writeback(state, page_budget) else {
        return Ok(0);
    };
    let pages = batch.pages.len();
    let mut bytes = 0usize;
    let mut result = Ok(());
    for run in batch.runs.iter() {
        perf::record_vfs_write_backend(run.data.len());
        let write_size = write_backend_at(node, run.offset as u64, &run.data);
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
        restore_dirty_writeback(batch);
        record_dirty_cache_flush_failure(reason);
        return result.map(|_| 0);
    }
    record_dirty_cache_flush(reason, pages, bytes);
    if batch.owns_inode_pin {
        release_inode_from_drop(&batch.inode_state);
    }
    Ok(pages)
}

fn flush_dirty_regular_file_for_reason_state(
    state: &Arc<inode_state::InodeState>,
    reason: DirtyFlushReason,
    page_budget: usize,
) -> FsResult<usize> {
    if !dirty_regular_file_has_pages_state(state) {
        return Ok(0);
    }
    inode_state::with_mapping_mutation_state(state, || {
        flush_dirty_regular_file_for_reason_under_mapping_state(state, reason, page_budget)
    })
}

pub(crate) fn flush_dirty_regular_file(node: VfsNodeId) -> FsResult {
    let state = inode_state::state_for(node);
    flush_dirty_regular_file_for_reason_state(&state, DirtyFlushReason::Explicit, usize::MAX)
        .map(|_| ())
}

pub(crate) fn flush_dirty_regular_files_on_mount(mount_id: MountId) -> FsResult {
    let states = dirty_inode_states_on_mount(mount_id);
    let mut result = Ok(());
    for state in states {
        if let Err(err) = flush_dirty_regular_file_for_reason_state(
            &state,
            DirtyFlushReason::Explicit,
            usize::MAX,
        ) {
            result = result.and(Err(err));
        }
    }
    result
}

fn flush_dirty_regular_files_for_pressure() -> FsResult {
    let states = dirty_pressure_candidates(VFS_DIRTY_PRESSURE_MAX_INODES);
    let candidate_count = states.len();
    let mut flushed_inodes = 0usize;
    let mut flushed_pages = 0usize;
    let mut result = Ok(());
    for state in states {
        if flushed_pages >= VFS_DIRTY_PRESSURE_MAX_PAGES {
            break;
        }
        let remaining = VFS_DIRTY_PRESSURE_MAX_PAGES - flushed_pages;
        match flush_dirty_regular_file_for_reason_state(
            &state,
            DirtyFlushReason::Pressure,
            remaining,
        ) {
            Ok(0) => {}
            Ok(pages) => {
                flushed_inodes += 1;
                flushed_pages = flushed_pages.saturating_add(pages);
            }
            Err(err) => result = result.and(Err(err)),
        }
    }
    let budget_stop = flushed_pages >= VFS_DIRTY_PRESSURE_MAX_PAGES && total_dirty_pages() != 0;
    record_dirty_overlay_pressure_batch(
        candidate_count,
        flushed_inodes,
        flushed_pages,
        budget_stop,
    );
    result
}

fn track_writable_regular_open(
    state: &inode_state::InodeState,
    mount_backend: &MountedBackendLease,
    kind: FsNodeKind,
    writable: bool,
) {
    if kind != FsNodeKind::RegularFile || !writable {
        return;
    }
    inode_state::track_writable_open(state);
    mount_backend.track_writable_regular_open();
}

fn untrack_writable_regular_open(
    state: &inode_state::InodeState,
    mount_backend: &MountedBackendLease,
    kind: FsNodeKind,
    writable: bool,
) {
    if kind != FsNodeKind::RegularFile || !writable {
        return;
    }
    inode_state::untrack_writable_open(state);
    mount_backend.untrack_writable_regular_open();
}

fn track_writable_shared_regular_mmap(
    state: &inode_state::InodeState,
    node: VfsNodeId,
    kind: FsNodeKind,
) {
    if kind != FsNodeKind::RegularFile {
        return;
    }
    invalidate_small_regular_read_cache(node, kind);
    inode_state::track_writable_shared_mmap(state);
}

fn untrack_writable_shared_regular_mmap(state: &inode_state::InodeState, kind: FsNodeKind) {
    if kind != FsNodeKind::RegularFile {
        return;
    }
    inode_state::untrack_writable_shared_mmap(state);
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

fn cached_inode_flags(state: &inode_state::InodeState) -> Option<u32> {
    inode_state::cached_inode_flags(state)
}

fn update_inode_flags_cache(state: &inode_state::InodeState, flags: u32) {
    inode_state::update_inode_flags_cache(state, flags);
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

fn regular_file_has_writable_shared_mmap(state: &inode_state::InodeState) -> bool {
    inode_state::has_writable_shared_mmap(state)
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
    #[cfg(any(feature = "fanotify", feature = "inotify"))]
    parent: Option<VfsNodeId>,
    kind: FsNodeKind,
    #[cfg(feature = "fanotify")]
    namespace_id: MountNamespaceId,
    visible_path: Option<String>,
    offset: SleepMutex<usize>,
    snapshot_state: VfsFileSnapshotState,
    supports_page_cache: bool,
    supports_dirty_writeback: bool,
    readable: bool,
    writable: bool,
    status_flags: StatusFlagsCell,
    #[cfg(feature = "fanotify")]
    suppress_fanotify: bool,
}

struct CachedDirectorySnapshot {
    version: usize,
    snapshot: BackendDirectorySnapshot,
}

enum VfsFileSnapshotState {
    None,
    Read(SleepMutex<Option<Vec<u8>>>),
    Directory(SleepMutex<Option<CachedDirectorySnapshot>>),
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
        #[cfg(not(feature = "fanotify"))]
        let _ = suppress_fanotify;
        #[cfg(not(any(feature = "fanotify", feature = "inotify")))]
        let _ = parent;
        #[cfg(not(feature = "fanotify"))]
        let _ = namespace_id;
        let node = path.node;
        let kind = path.kind;
        let visible_path = path.visible_path;
        // An open file description pins its backend inode even if the path is
        // later unlinked. Keep this retain paired with Drop's release path.
        let mount_backend = mounted_backend_lease(node.mount_id).ok_or(FsError::Io)?;
        let inode_state = retain_inode_with_lease(node, &mount_backend)?;
        let snapshot_state = if kind == FsNodeKind::Directory {
            VfsFileSnapshotState::Directory(SleepMutex::new(None))
        } else if kind == FsNodeKind::RegularFile
            && mount_backend.call(BackendOp::ReadPlan, |mount| {
                mount.supports_read_snapshot(node.ino)
            })
        {
            VfsFileSnapshotState::Read(SleepMutex::new(None))
        } else {
            VfsFileSnapshotState::None
        };
        let supports_page_cache = mount_supports_page_cache(node.mount_id);
        let supports_dirty_writeback = mount_supports_dirty_writeback(node.mount_id);
        track_writable_regular_open(&inode_state, &mount_backend, kind, writable);
        let file = Self {
            node,
            inode_state,
            mount_backend,
            #[cfg(any(feature = "fanotify", feature = "inotify"))]
            parent,
            kind,
            #[cfg(feature = "fanotify")]
            namespace_id,
            visible_path,
            offset: SleepMutex::new(0),
            snapshot_state,
            supports_page_cache,
            supports_dirty_writeback,
            readable,
            writable,
            status_flags: StatusFlagsCell::new(status_flags),
            #[cfg(feature = "fanotify")]
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
        self.invalidate_read_snapshot();
        let _mutation = (buf.len() > 0)
            .then(|| begin_regular_file_page_cache_mutation(self.node, self.kind))
            .flatten();
        let mut total_write_size = 0usize;
        perf::record_vfs_write_user_buffer(buf.segment_count());
        if self.kind == FsNodeKind::RegularFile && buf.segment_count() > 1 {
            return self.write_coalesced_user_buffer(&mut offset, &buf);
        }
        for slice in buf.segments() {
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
        for slice in buf.segments() {
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
        let mut total_write_size = 0usize;
        'chunks: for chunk in buf.chunks(VFS_WRITE_CHUNK_SIZE) {
            let Some(chunk_offset) = offset.checked_add(total_write_size) else {
                break;
            };
            let mut pressure_retried = false;
            let mut force_backend = false;
            loop {
                let outcome =
                    inode_state::with_mapping_mutation_value_state(&self.inode_state, || {
                        self.write_chunk_under_mapping(chunk_offset, chunk, force_backend)
                    });
                match outcome {
                    DirtyChunkWriteResult::Written(write_size) => {
                        total_write_size = total_write_size.saturating_add(write_size);
                        if write_size < chunk.len() {
                            break 'chunks;
                        }
                        break;
                    }
                    DirtyChunkWriteResult::NeedsPressureFlush if !pressure_retried => {
                        // Leave the current inode's mapping lease before
                        // selecting and flushing unrelated dirty inodes.
                        if flush_dirty_regular_files_for_pressure().is_err() {
                            break 'chunks;
                        }
                        pressure_retried = true;
                    }
                    DirtyChunkWriteResult::NeedsPressureFlush => {
                        // A bounded batch may not make enough room under
                        // concurrent writers. Preserve forward progress by
                        // flushing this inode and using the backend directly.
                        force_backend = true;
                    }
                    DirtyChunkWriteResult::Failed => break 'chunks,
                }
            }
        }
        total_write_size
    }

    fn write_chunk_under_mapping(
        &self,
        offset: usize,
        chunk: &[u8],
        force_backend: bool,
    ) -> DirtyChunkWriteResult {
        if !force_backend
            && can_cache_dirty_write(
                self.kind,
                self.supports_dirty_writeback,
                offset,
                chunk.len(),
                self.status_flags.get(),
            )
        {
            match cache_dirty_regular_write(&self.inode_state, offset, chunk) {
                DirtyCacheWriteResult::Cached(write_size) => {
                    return DirtyChunkWriteResult::Written(write_size);
                }
                DirtyCacheWriteResult::NeedsPressureFlush => {
                    return DirtyChunkWriteResult::NeedsPressureFlush;
                }
                DirtyCacheWriteResult::Fallback => {}
            }
        }
        if self.kind == FsNodeKind::RegularFile && !chunk.is_empty() {
            record_dirty_cache_fallback();
            if flush_dirty_regular_file_for_reason_under_mapping_state(
                &self.inode_state,
                DirtyFlushReason::Explicit,
                usize::MAX,
            )
            .is_err()
            {
                return DirtyChunkWriteResult::Failed;
            }
        }
        perf::record_vfs_write_backend(chunk.len());
        let Some(write_size) = write_backend_at(self.node, offset as u64, chunk) else {
            return DirtyChunkWriteResult::Failed;
        };
        DirtyChunkWriteResult::Written(write_size)
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
        let VfsFileSnapshotState::Read(snapshot) = &self.snapshot_state else {
            return None;
        };
        let mut snapshot = snapshot.lock();
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
        if !matches!(&self.snapshot_state, VfsFileSnapshotState::Read(_)) {
            return None;
        }
        let mut total_read_size = 0usize;
        for slice in buf.segments_mut() {
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

    #[inline(always)]
    fn invalidate_read_snapshot(&self) {
        if let VfsFileSnapshotState::Read(snapshot) = &self.snapshot_state {
            *snapshot.lock() = None;
        }
    }

    fn read_coalesced_user_buffer(
        &self,
        offset: &mut usize,
        buf: &mut UserBuffer,
    ) -> Option<usize> {
        if self.kind != FsNodeKind::RegularFile
            || buf.segment_count() <= 1
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
            let read_limit =
                user_buffer_chunk_len(buf, buffer_index, buffer_offset, VFS_READ_CHUNK_SIZE);
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
                buf,
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
        if let Some(flags) = cached_inode_flags(&self.inode_state) {
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
        update_inode_flags_cache(&self.inode_state, flags);
        Ok(flags)
    }

    fn read_synthetic_dirent64(&self, entry_offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        let entries: Vec<RawDirEntry> = static_mount_children_for_dir(self.node)
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
        if !dirty_regular_file_has_pages_state(&self.inode_state) {
            return None;
        }
        let cache = lock_dirty_file(&self.inode_state);
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
            record_dirty_overlay_locked_copy(len);
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
            || regular_file_has_writable_shared_mmap(&self.inode_state)
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
    buffers: &UserBuffer,
    mut buffer_index: usize,
    mut buffer_offset: usize,
    limit: usize,
) -> usize {
    let mut len = 0usize;
    while buffer_index < buffers.segment_count() && len < limit {
        let buffer_len = buffers
            .segment(buffer_index)
            .expect("user-buffer segment index disappeared")
            .len();
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
    buffers: &mut UserBuffer,
    buffer_index: &mut usize,
    buffer_offset: &mut usize,
    src: &[u8],
) -> usize {
    let mut copied = 0usize;
    while copied < src.len() {
        while *buffer_index < buffers.segment_count()
            && *buffer_offset
                >= buffers
                    .segment(*buffer_index)
                    .expect("user-buffer segment index disappeared")
                    .len()
        {
            *buffer_index += 1;
            *buffer_offset = 0;
        }
        if *buffer_index >= buffers.segment_count() {
            break;
        }
        let dst = &mut buffers
            .segment_mut(*buffer_index)
            .expect("user-buffer segment index disappeared")[*buffer_offset..];
        let take = dst.len().min(src.len() - copied);
        dst[..take].copy_from_slice(&src[copied..copied + take]);
        copied += take;
        *buffer_offset += take;
    }
    copied
}

fn parent_hint_for_open(context: &PathContext, name: &str) -> Option<VfsNodeId> {
    #[cfg(any(feature = "fanotify", feature = "inotify"))]
    {
        return vfs_path::resolve_create_parent_in(context.clone(), name)
            .ok()
            .map(|target| target.parent);
    }
    #[cfg(not(any(feature = "fanotify", feature = "inotify")))]
    {
        let _ = (context, name);
        None
    }
}

enum OpenedVfsFile {
    Vfs(Arc<VfsFile>),
    Special(Arc<dyn File + Send + Sync>),
}

impl OpenedVfsFile {
    fn into_file(self) -> Arc<dyn File + Send + Sync> {
        match self {
            Self::Vfs(file) => file,
            Self::Special(file) => file,
        }
    }
}

fn open_vfs_file_once(
    context: PathContext,
    name: &str,
    flags: OpenFlags,
    create_attrs: Option<FileCreateAttrs>,
) -> FsResult<OpenedVfsFile> {
    let namespace_id = context.namespace_id();
    let follow_final_symlink = !flags.contains(OpenFlags::NOFOLLOW);
    reject_nosymfollow_final_symlink(context.clone(), name, flags)?;
    let resolved = vfs_path::resolve_open_in(
        context.clone(),
        name,
        follow_final_symlink,
        flags.contains(OpenFlags::CREATE),
    )?;

    let (path, parent, readable, writable) = match resolved {
        VfsOpenTarget::Existing(path) => {
            if mount_is_devfs(path.node.mount_id) && path.kind != FsNodeKind::Directory {
                return devfs::open_inode(path.node.mount_id, path.node.ino, flags)
                    .map(OpenedVfsFile::Special);
            }
            if path.kind == FsNodeKind::Fifo {
                if flags.contains(OpenFlags::CREATE | OpenFlags::EXCL) {
                    return Err(FsError::AlreadyExists);
                }
                if flags.contains(OpenFlags::DIRECTORY) {
                    return Err(FsError::NotDir);
                }
                return open_named_fifo(path.node, OpenFlags::file_status_flags(flags))
                    .map(OpenedVfsFile::Special);
            }
            if flags.contains(OpenFlags::CREATE | OpenFlags::EXCL) {
                return Err(FsError::AlreadyExists);
            }
            if path.kind == FsNodeKind::Directory {
                if !flags.can_open_directory() {
                    return Err(FsError::IsDir);
                }
                let parent = parent_hint_for_open(&context, name);
                (path, parent, false, false)
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
                let parent = parent_hint_for_open(&context, name);
                (path, parent, readable, writable)
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
                ensure_backend_create_target_absent(target.parent, target.leaf_name, false)?;
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

    Ok(OpenedVfsFile::Vfs(Arc::new(VfsFile::new(
        path,
        parent,
        readable,
        writable,
        OpenFlags::file_status_flags(flags),
        namespace_id,
        false,
    )?)))
}

fn open_vfs_file_impl(
    context: PathContext,
    name: &str,
    flags: OpenFlags,
    create_attrs: Option<FileCreateAttrs>,
) -> FsResult<OpenedVfsFile> {
    for _ in 0..OPEN_CREATE_RACE_ATTEMPTS {
        match open_vfs_file_once(context.clone(), name, flags, create_attrs.clone()) {
            Err(FsError::AlreadyExists)
                if flags.contains(OpenFlags::CREATE) && !flags.contains(OpenFlags::EXCL) =>
            {
                continue;
            }
            result => return result,
        }
    }
    // UNFINISHED: Linux does not expose this bounded internal retry. Under
    // adversarial create/unlink churn we return EEXIST instead of livelocking.
    Err(FsError::AlreadyExists)
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
                ensure_backend_create_target_absent(directory.node, leaf_name.as_str(), false)?;
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
    match open_vfs_file_impl(PathContext::global_root(), name, flags, None)? {
        OpenedVfsFile::Vfs(file) => Ok(file),
        OpenedVfsFile::Special(_) => Err(FsError::Unsupported),
    }
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
    open_vfs_file_impl(context, name, flags, create_attrs).map(OpenedVfsFile::into_file)
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
        let state = inode_state::state_for(node);
        let (stat, expected_metadata_epoch) =
            inode_state::with_mapping_read_state(&state, || -> FsResult<_> {
                let expected_metadata_epoch = inode_state::direct_stat_metadata_epoch(node);
                let mut stat = if full_stat {
                    stat_full_cached(node)?
                } else {
                    stat_basic_cached(node)?
                };
                stat.dev = node.mount_id.0 as u64;
                overlay_dirty_regular_stat_state(&state, &mut stat);
                Ok((stat, expected_metadata_epoch))
            })?;
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
    if path.kind == FsNodeKind::RegularFile {
        let state = inode_state::state_for(path.node);
        return inode_state::with_mapping_read_state(&state, || {
            let mut stat = {
                let _profile_scope = perf::time_scope(perf::ProfilePoint::StatPathBackendStat);
                if full_stat {
                    stat_full_cached(path.node)?
                } else {
                    stat_basic_cached(path.node)?
                }
            };
            stat.dev = path.node.mount_id.0 as u64;
            let _profile_scope = perf::time_scope(perf::ProfilePoint::StatPathDirtyOverlay);
            overlay_dirty_regular_stat_state(&state, &mut stat);
            Ok(stat)
        });
    }
    let mut stat = {
        let _profile_scope = perf::time_scope(perf::ProfilePoint::StatPathBackendStat);
        if full_stat {
            stat_full_cached(path.node)?
        } else {
            stat_basic_cached(path.node)?
        }
    };
    stat.dev = path.node.mount_id.0 as u64;
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
            for slice in buf.segments_mut() {
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
        if self.kind != FsNodeKind::RegularFile {
            let mut stat =
                stat_full_cached_with_state_and_lease(&self.inode_state, &self.mount_backend)?;
            stat.dev = self.node.mount_id.0 as u64;
            return Ok(stat);
        }
        inode_state::with_mapping_read_state(&self.inode_state, || {
            let mut stat =
                stat_full_cached_with_state_and_lease(&self.inode_state, &self.mount_backend)?;
            stat.dev = self.node.mount_id.0 as u64;
            overlay_dirty_regular_stat_state(&self.inode_state, &mut stat);
            Ok(stat)
        })
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
        self.invalidate_read_snapshot();
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
        self.invalidate_read_snapshot();
        let _mutation = begin_regular_file_page_cache_mutation(self.node, self.kind);
        let mut pressure_retried = false;
        loop {
            let outcome = inode_state::with_mapping_mutation_value_state(&self.inode_state, || {
                cache_dirty_regular_user_buffer_write(&self.inode_state, offset, &buf)
            });
            match outcome {
                DirtyCacheWriteResult::Cached(write_size) => return Ok(write_size),
                DirtyCacheWriteResult::NeedsPressureFlush if !pressure_retried => {
                    flush_dirty_regular_files_for_pressure()?;
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
        self.invalidate_read_snapshot();
        let _mutation = begin_regular_file_page_cache_mutation(self.node, self.kind);
        let mut pressure_retried = false;
        let mut offset_advanced = false;
        let write_size = loop {
            let outcome = inode_state::with_mapping_mutation_value_state(&self.inode_state, || {
                cache_dirty_regular_user_buffer_write(&self.inode_state, write_offset, &buf)
            });
            match outcome {
                DirtyCacheWriteResult::Cached(write_size) => break write_size,
                DirtyCacheWriteResult::NeedsPressureFlush if !pressure_retried => {
                    if flush_dirty_regular_files_for_pressure().is_ok() {
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
        let VfsFileSnapshotState::Directory(directory_snapshot) = &self.snapshot_state else {
            unreachable!("directory VfsFile missing directory snapshot state");
        };
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
                        let cached = directory_snapshot.lock();
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
                        *directory_snapshot.lock() = Some(CachedDirectorySnapshot {
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
            update_inode_flags_cache(&self.inode_state, flags);
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

    #[cfg(any(feature = "fanotify", feature = "inotify"))]
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
        track_writable_shared_regular_mmap(&self.inode_state, self.node, self.kind);
    }

    fn dec_writable_shared_mmap(&self) {
        untrack_writable_shared_regular_mmap(&self.inode_state, self.kind);
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }

    #[cfg(feature = "fanotify")]
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

    #[cfg(feature = "fanotify")]
    fn suppresses_fanotify(&self) -> bool {
        self.suppress_fanotify
    }
}

impl Drop for VfsFile {
    fn drop(&mut self) {
        untrack_writable_regular_open(
            &self.inode_state,
            &self.mount_backend,
            self.kind,
            self.writable,
        );
        release_inode_from_drop_with_lease(&self.inode_state, &self.mount_backend);
    }
}
