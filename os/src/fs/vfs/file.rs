use super::super::dentry_cache;
use super::super::devfs;
use super::super::dirent::{DT_DIR, RawDirEntry, write_dir_entries_with_offset_base};
use super::super::inode::{OpenFlags, link_node_in};
use super::super::mount::{
    MountId, MountNamespaceId, mount_is_devfs, mount_is_noatime, mount_is_nodev,
    mount_is_nodiratime, mount_is_nosymfollow, mount_is_read_only, mount_supports_dirty_writeback,
    mount_supports_page_cache, release_inode_from_drop, synthetic_children_for_dir, with_mount,
};
use super::super::named_fifo::open_named_fifo;
use super::super::path::{PathContext, WorkingDir};
use super::super::status_flags::StatusFlagsCell;
use super::super::{
    FS_APPEND_FL, FS_IMMUTABLE_FL, File, FileStat, FileTimestamp, S_IFBLK, S_IFCHR, S_IFDIR,
    S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK, SeekWhence,
};
use super::path::{self as vfs_path, LookupMode, VfsOpenTarget};
use super::{FsError, FsNodeKind, FsResult, VfsNodeId, VfsPath};
use crate::config::PAGE_SIZE;
use crate::mm::{
    UserBuffer, frame_alloc, frame_alloc_uninit,
    page_cache::{PAGE_CACHE, PageCacheId, PageCacheKey},
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
const VFS_READ_CACHE_MAX_FILE_SIZE: usize = 1024 * 1024;
const VFS_READ_CACHE_READAHEAD_PAGES: usize = VFS_READ_CHUNK_SIZE / PAGE_SIZE;
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
    // CONTEXT: These counters approximate Linux's open-writer vs executable
    // exclusion for ETXTBSY without adding per-inode objects to every backend.
    // They are keyed by VfsNodeId, so callers must update them only at VfsFile
    // open/drop and exec image lifetime boundaries.
    static ref WRITABLE_REGULAR_OPEN_COUNTS: SleepMutex<BTreeMap<VfsNodeId, usize>> =
        SleepMutex::new(BTreeMap::new());
    static ref EXECUTABLE_REGULAR_COUNTS: SleepMutex<BTreeMap<VfsNodeId, usize>> =
        SleepMutex::new(BTreeMap::new());
    static ref DIRTY_REGULAR_FILES: SleepMutex<BTreeMap<VfsNodeId, DirtyFileCache>> =
        SleepMutex::new(BTreeMap::new());
}

#[cfg(feature = "perf-counters")]
lazy_static! {
    static ref DIRTY_WRITEBACK_COUNTERS: SleepMutex<DirtyWritebackCounters> =
        SleepMutex::new(DirtyWritebackCounters::new());
}

#[derive(Debug)]
struct DirtyFileCache {
    logical_size: usize,
    mtime: FileTimestamp,
    ctime: FileTimestamp,
    pages: BTreeMap<usize, Vec<u8>>,
}

impl DirtyFileCache {
    fn new(logical_size: usize, timestamp: FileTimestamp) -> Self {
        Self {
            logical_size,
            mtime: timestamp,
            ctime: timestamp,
            pages: BTreeMap::new(),
        }
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

fn dirty_regular_file_has_pages(node: VfsNodeId) -> bool {
    DIRTY_REGULAR_FILES
        .lock()
        .get(&node)
        .is_some_and(|cache| !cache.pages.is_empty())
}

fn overlay_dirty_regular_stat(node: VfsNodeId, stat: &mut FileStat) {
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
    mount_id: MountId,
    offset: usize,
    len: usize,
    status_flags: OpenFlags,
) -> bool {
    kind == FsNodeKind::RegularFile
        && len > 0
        && len <= VFS_DIRTY_WRITEBACK_MAX_WRITE_SIZE
        && offset % PAGE_SIZE == 0
        && len % PAGE_SIZE == 0
        && !status_flags.intersects(OpenFlags::DIRECT | OpenFlags::DSYNC | OpenFlags::SYNC)
        && mount_supports_dirty_writeback(mount_id)
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

fn cache_dirty_regular_write(node: VfsNodeId, offset: usize, buf: &[u8]) -> DirtyCacheWriteResult {
    if buf.is_empty() {
        return DirtyCacheWriteResult::Cached(0);
    }
    let stat = match with_mount(node.mount_id, |mount| mount.stat(node.ino)) {
        Some(Ok(stat)) => stat,
        _ => return DirtyCacheWriteResult::Fallback,
    };
    let base_size = stat.size as usize;
    let logical_size = dirty_logical_size(node).unwrap_or(base_size);
    let Some(end) = offset.checked_add(buf.len()) else {
        return DirtyCacheWriteResult::Fallback;
    };
    if offset > logical_size {
        return DirtyCacheWriteResult::Fallback;
    }

    let page_start = offset / PAGE_SIZE;
    let page_count = buf.len() / PAGE_SIZE;
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
        match with_mount(node.mount_id, |mount| mount.retain_inode(node.ino)) {
            Some(Ok(())) => true,
            _ => return DirtyCacheWriteResult::Fallback,
        }
    } else {
        false
    };

    let timestamp = FileTimestamp::now();
    let mut release_extra_pin = false;
    let mut dirty = DIRTY_REGULAR_FILES.lock();
    let (dirty_pages, new_pages) = dirty_write_page_pressure(&dirty, node, page_start, page_count);
    if dirty_pages.saturating_add(new_pages) > VFS_DIRTY_WRITEBACK_MAX_PAGES {
        drop(dirty);
        if retained_pin {
            release_inode_from_drop(node.mount_id, node.ino);
        }
        return DirtyCacheWriteResult::NeedsPressureFlush;
    }
    if retained_pin && dirty.contains_key(&node) {
        release_extra_pin = true;
    }
    let cache = dirty
        .entry(node)
        .or_insert_with(|| DirtyFileCache::new(logical_size, timestamp));
    cache.logical_size = cache.logical_size.max(end);
    cache.mtime = timestamp;
    cache.ctime = timestamp;
    for (page_offset, chunk) in buf.chunks(PAGE_SIZE).enumerate() {
        let page_index = page_start + page_offset;
        let mut page = Vec::with_capacity(PAGE_SIZE);
        page.extend_from_slice(chunk);
        cache.pages.insert(page_index, page);
    }
    let current_dirty_pages = dirty.values().map(|cache| cache.pages.len()).sum::<usize>();
    drop(dirty);
    if release_extra_pin {
        release_inode_from_drop(node.mount_id, node.ino);
    }

    record_dirty_cache_write(buf.len() / PAGE_SIZE, buf.len());
    record_dirty_cache_peak(current_dirty_pages);
    DirtyCacheWriteResult::Cached(buf.len())
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
        let dst_start = copy_start - offset;
        let src_start = copy_start - page_start;
        let len = copy_end - copy_start;
        buf[dst_start..dst_start + len].copy_from_slice(&page[src_start..src_start + len]);
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

#[derive(Debug)]
struct DirtyWritebackBatch {
    logical_size: usize,
    runs: Vec<DirtyWritebackRun>,
}

fn collect_dirty_writeback(node: VfsNodeId) -> Option<DirtyWritebackBatch> {
    let mut dirty = DIRTY_REGULAR_FILES.lock();
    let cache = dirty.remove(&node)?;
    let logical_size = cache.logical_size;
    let mut runs = Vec::new();
    let mut current_offset = 0usize;
    let mut current_data = Vec::new();
    for (page_index, page) in cache.pages {
        let page_offset = page_index.saturating_mul(PAGE_SIZE);
        if current_data.is_empty() {
            current_offset = page_offset;
        } else if current_offset + current_data.len() != page_offset {
            runs.push(DirtyWritebackRun {
                offset: current_offset,
                data: current_data,
            });
            current_offset = page_offset;
            current_data = Vec::new();
        }
        current_data.extend_from_slice(&page);
        if current_data.len() >= VFS_WRITE_CHUNK_SIZE {
            runs.push(DirtyWritebackRun {
                offset: current_offset,
                data: current_data,
            });
            current_data = Vec::new();
        }
    }
    if !current_data.is_empty() {
        runs.push(DirtyWritebackRun {
            offset: current_offset,
            data: current_data,
        });
    }
    Some(DirtyWritebackBatch { logical_size, runs })
}

fn restore_dirty_writeback(node: VfsNodeId, batch: DirtyWritebackBatch) {
    let timestamp = FileTimestamp::now();
    let mut dirty = DIRTY_REGULAR_FILES.lock();
    let release_batch_pin = dirty.contains_key(&node);
    let cache = dirty
        .entry(node)
        .or_insert_with(|| DirtyFileCache::new(batch.logical_size, timestamp));
    cache.logical_size = cache.logical_size.max(batch.logical_size);
    for run in batch.runs {
        for (page_offset, chunk) in run.data.chunks(PAGE_SIZE).enumerate() {
            let page_index = run.offset / PAGE_SIZE + page_offset;
            let mut page = Vec::with_capacity(PAGE_SIZE);
            page.extend_from_slice(chunk);
            cache.pages.entry(page_index).or_insert(page);
        }
    }
    drop(dirty);
    if release_batch_pin {
        release_inode_from_drop(node.mount_id, node.ino);
    }
}

fn flush_dirty_regular_file_for_reason(node: VfsNodeId, reason: DirtyFlushReason) -> FsResult {
    let Some(batch) = collect_dirty_writeback(node) else {
        return Ok(());
    };
    let mut pages = 0usize;
    let mut bytes = 0usize;
    let mut result = Ok(());
    for run in batch.runs.iter() {
        perf::record_vfs_write_backend(run.data.len());
        let write_size = with_mount(node.mount_id, |mount| {
            mount.write_at(node.ino, &run.data, run.offset as u64)
        });
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
        pages = pages.saturating_add(run.data.len() / PAGE_SIZE);
        bytes = bytes.saturating_add(run.data.len());
    }
    if result.is_err() {
        restore_dirty_writeback(node, batch);
        record_dirty_cache_flush_failure(reason);
        return result;
    }
    record_dirty_cache_flush(reason, pages, bytes);
    release_inode_from_drop(node.mount_id, node.ino);
    Ok(())
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

fn flush_dirty_regular_files_for_pressure() -> FsResult {
    let nodes = {
        let dirty = DIRTY_REGULAR_FILES.lock();
        dirty.keys().copied().collect::<Vec<_>>()
    };
    let mut result = Ok(());
    for node in nodes {
        if let Err(err) = flush_dirty_regular_file_for_reason(node, DirtyFlushReason::Pressure) {
            result = result.and(Err(err));
        }
    }
    result
}

fn track_writable_regular_open(node: VfsNodeId, kind: FsNodeKind, writable: bool) {
    if kind != FsNodeKind::RegularFile || !writable {
        return;
    }
    let mut counts = WRITABLE_REGULAR_OPEN_COUNTS.lock();
    *counts.entry(node).or_insert(0) += 1;
}

fn untrack_writable_regular_open(node: VfsNodeId, kind: FsNodeKind, writable: bool) {
    if kind != FsNodeKind::RegularFile || !writable {
        return;
    }
    let mut counts = WRITABLE_REGULAR_OPEN_COUNTS.lock();
    let Some(count) = counts.get_mut(&node) else {
        return;
    };
    if *count > 1 {
        *count -= 1;
    } else {
        counts.remove(&node);
    }
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
    let Ok(path) = vfs_path::resolve_existing_in(context, name, LookupMode::NoFollowFinal) else {
        return Ok(());
    };
    if path.kind == FsNodeKind::Symlink && mount_is_nosymfollow(path.node.mount_id) {
        Err(FsError::Loop)
    } else {
        Ok(())
    }
}

fn page_cache_id_for_node(node: VfsNodeId, kind: FsNodeKind) -> Option<PageCacheId> {
    if kind != FsNodeKind::RegularFile || !mount_supports_page_cache(node.mount_id) {
        return None;
    }
    Some(PageCacheId::new(node.mount_id, node.ino))
}

pub(crate) fn invalidate_regular_file_read_cache(node: VfsNodeId, kind: FsNodeKind) {
    let Some(id) = page_cache_id_for_node(node, kind) else {
        return;
    };
    let (removed, scanned) = PAGE_CACHE
        .exclusive_access()
        .invalidate_clean_unreferenced(id);
    perf::record_vfs_read_cache_invalidation(removed, scanned);
}

pub(crate) fn regular_file_is_open_writable_in(context: PathContext, name: &str) -> FsResult<bool> {
    let path = vfs_path::resolve_existing_in(context, name, LookupMode::FollowFinal)?;
    if path.kind != FsNodeKind::RegularFile {
        return Ok(false);
    }
    Ok(regular_file_node_is_open_writable(path.node))
}

pub(crate) fn regular_file_node_is_open_writable(node: VfsNodeId) -> bool {
    WRITABLE_REGULAR_OPEN_COUNTS
        .lock()
        .get(&node)
        .copied()
        .unwrap_or(0)
        > 0
}

pub(crate) fn mount_has_writable_regular_open(mount_id: MountId) -> bool {
    WRITABLE_REGULAR_OPEN_COUNTS
        .lock()
        .keys()
        .any(|node| node.mount_id == mount_id)
}

pub(crate) fn track_regular_file_executable(node: VfsNodeId) {
    let mut counts = EXECUTABLE_REGULAR_COUNTS.lock();
    *counts.entry(node).or_insert(0) += 1;
}

pub(crate) fn untrack_regular_file_executable(node: VfsNodeId) {
    let mut counts = EXECUTABLE_REGULAR_COUNTS.lock();
    let Some(count) = counts.get_mut(&node) else {
        return;
    };
    if *count > 1 {
        *count -= 1;
    } else {
        counts.remove(&node);
    }
}

pub(crate) fn regular_file_node_is_executable(node: VfsNodeId) -> bool {
    EXECUTABLE_REGULAR_COUNTS
        .lock()
        .get(&node)
        .copied()
        .unwrap_or(0)
        > 0
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
    parent: Option<VfsNodeId>,
    kind: FsNodeKind,
    namespace_id: MountNamespaceId,
    visible_path: Option<String>,
    offset: SleepMutex<usize>,
    read_snapshot: SleepMutex<Option<Vec<u8>>>,
    read_snapshot_supported: bool,
    readable: bool,
    writable: bool,
    status_flags: StatusFlagsCell,
    suppress_fanotify: bool,
}

impl VfsFile {
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
        with_mount(node.mount_id, |mount| mount.retain_inode(node.ino)).ok_or(FsError::Io)??;
        let read_snapshot_supported = with_mount(node.mount_id, |mount| {
            mount.supports_read_snapshot(node.ino)
        })
        .unwrap_or(false);
        track_writable_regular_open(node, kind, writable);
        let file = Self {
            node,
            parent,
            kind,
            namespace_id,
            visible_path,
            offset: SleepMutex::new(0),
            read_snapshot: SleepMutex::new(None),
            read_snapshot_supported,
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
            let len = self.read_backend_at(*offset, &mut buffer);
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
            let stat = match with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino)) {
                Some(Ok(stat)) => stat,
                _ => {
                    return 0;
                }
            };
            *offset = stat_logical_size(self.node, stat.size) as usize;
        }
        *self.read_snapshot.lock() = None;
        if buf.len() > 0 {
            invalidate_regular_file_read_cache(self.node, self.kind);
        }
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
        let mut total_write_size = 0usize;
        for chunk in buf.chunks(VFS_WRITE_CHUNK_SIZE) {
            let Some(chunk_offset) = offset.checked_add(total_write_size) else {
                break;
            };
            let mut cached_dirty = false;
            if can_cache_dirty_write(
                self.kind,
                self.node.mount_id,
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
                            if flush_dirty_regular_files_for_pressure().is_err() {
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
                if flush_dirty_regular_file(self.node).is_err() {
                    break;
                }
            }
            perf::record_vfs_write_backend(chunk.len());
            let Some(write_size) = with_mount(self.node.mount_id, |mount| {
                mount.write_at(self.node.ino, chunk, chunk_offset as u64)
            }) else {
                break;
            };
            total_write_size = total_write_size.saturating_add(write_size);
            if write_size < chunk.len() {
                break;
            }
        }
        total_write_size
    }

    fn read_backend_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let Some(read_size) = with_mount(self.node.mount_id, |mount| {
            mount.read_at(self.node.ino, buf, offset as u64)
        }) else {
            return 0;
        };
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

    fn read_snapshot_at(&self, offset: usize, buf: &mut [u8]) -> Option<usize> {
        if !self.read_snapshot_supported {
            return None;
        }
        let mut snapshot = self.read_snapshot.lock();
        if offset == 0 {
            *snapshot = None;
        }
        if snapshot.is_none() {
            let content = match with_mount(self.node.mount_id, |mount| {
                mount.read_snapshot(self.node.ino)
            })? {
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
        let stat = with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino))?.ok()?;
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
            let read_size = self.read_backend_at(*offset, &mut bounce[..read_limit]);
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
        let stat = with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino))?.ok()?;
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
            let _ = with_mount(self.node.mount_id, |mount| {
                mount.set_times(self.node.ino, Some(atime), None, ctime)
            });
        }
    }

    fn touch_directory_atime(&self) {
        if mount_is_noatime(self.node.mount_id) || mount_is_nodiratime(self.node.mount_id) {
            return;
        }
        let Some(Ok(stat)) = with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino))
        else {
            return;
        };
        let ctime = FileTimestamp {
            sec: stat.ctime_sec,
            nsec: stat.ctime_nsec,
        };
        let _ = with_mount(self.node.mount_id, |mount| {
            mount.set_times(self.node.ino, Some(FileTimestamp::now()), None, ctime)
        });
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
        let stat = with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino))
            .ok_or(FsError::Io)??;
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
            let read_len = with_mount(self.node.mount_id, |mount| {
                mount.read_at(self.node.ino, &mut buf[..valid_len], block_start as u64)
            })
            .ok_or(FsError::Io)?;
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
        match self.inode_flags() {
            Ok(flags) => Ok(flags),
            // CONTEXT: procfs and other synthetic filesystems do not expose
            // ext-style inode flags. Treat them as having no immutable/append
            // bits so writable sysctl-style files can be updated normally.
            Err(FsError::Unsupported) => Ok(0),
            Err(err) => Err(err),
        }
    }

    fn read_synthetic_dirent64(&self, entry_offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        let Some(parent_path) = self.visible_path.as_deref() else {
            return Ok((0, entry_offset));
        };
        let entries: Vec<RawDirEntry> =
            synthetic_children_for_dir(self.namespace_id, self.node, parent_path)
                .into_iter()
                .filter(|entry| {
                    !with_mount(self.node.mount_id, |mount| {
                        mount
                            .lookup_component_from(self.node.ino, entry.name.as_str())
                            .is_ok()
                    })
                    .unwrap_or(false)
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

    fn read_cache_id_for_size(&self, file_size: usize) -> Option<PageCacheId> {
        if file_size > VFS_READ_CACHE_MAX_FILE_SIZE {
            perf::record_vfs_read_cache_skip_too_large();
            return None;
        }
        if dirty_regular_file_has_pages(self.node) {
            perf::record_vfs_read_cache_skip_dirty_pages();
            return None;
        }
        let id = page_cache_id_for_node(self.node, self.kind);
        if id.is_some() {
            perf::record_vfs_read_cache_eligible();
        }
        id
    }

    fn read_regular_cached_at(&self, offset: usize, buf: &mut [u8]) -> Option<usize> {
        if buf.is_empty() {
            return Some(0);
        }
        let stat = with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino))?.ok()?;
        let file_size = stat.size as usize;
        let id = self.read_cache_id_for_size(file_size)?;
        let mut total_read_size = 0usize;

        while total_read_size < buf.len() {
            let file_offset = offset.checked_add(total_read_size)?;
            if file_offset >= file_size {
                break;
            }
            let page_start = file_offset / PAGE_SIZE * PAGE_SIZE;
            let page_offset = file_offset - page_start;
            let valid_len = PAGE_SIZE.min(file_size - page_start);
            if page_offset >= valid_len {
                break;
            }
            let copy_len = (buf.len() - total_read_size).min(valid_len - page_offset);
            let key = PageCacheKey {
                id,
                page_index: page_start / PAGE_SIZE,
            };

            if let Some(read_size) = PAGE_CACHE.exclusive_access().copy_read_cache_page_data(
                key,
                page_offset,
                copy_len,
                &mut buf[total_read_size..total_read_size + copy_len],
            ) {
                total_read_size += read_size;
                perf::record_vfs_read_cache_hit(read_size);
                continue;
            }

            perf::record_vfs_read_cache_miss();
            let max_readahead_pages =
                ((file_size - page_start).div_ceil(PAGE_SIZE)).min(VFS_READ_CACHE_READAHEAD_PAGES);
            let readahead_pages = {
                let cache = PAGE_CACHE.exclusive_access();
                let mut pages = 1usize;
                while pages < max_readahead_pages {
                    let next_key = PageCacheKey {
                        id,
                        page_index: key.page_index + pages,
                    };
                    if cache.contains(next_key) {
                        break;
                    }
                    pages += 1;
                }
                pages
            };
            let read_limit = (readahead_pages * PAGE_SIZE).min(file_size - page_start);
            let mut read_buf = vec![0u8; read_limit];

            let read_len = with_mount(self.node.mount_id, |mount| {
                mount.read_at(self.node.ino, read_buf.as_mut_slice(), page_start as u64)
            })
            .expect("filesystem mount is missing");
            perf::record_vfs_read_cache_backend_read();
            if read_len == 0 || page_offset >= read_len {
                break;
            }

            let mut pages_to_cache = Vec::new();
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
                    frame_alloc()
                };
                let Some(frame) = frame else {
                    continue;
                };
                frame.ppn.get_bytes_array()[..page_valid_len]
                    .copy_from_slice(&read_buf[batch_offset..batch_offset + page_valid_len]);
                pages_to_cache.push((
                    PageCacheKey {
                        id,
                        page_index: key.page_index + page_delta,
                    },
                    frame,
                ));
            }

            if !pages_to_cache.is_empty() {
                let readahead_cached_pages = pages_to_cache.len().saturating_sub(1);
                let mut evicted = 0usize;
                let mut cache = PAGE_CACHE.exclusive_access();
                for (cache_key, frame) in pages_to_cache {
                    evicted += cache.insert_read_cache_page(cache_key, frame, file_size);
                }
                drop(cache);
                if evicted > 0 {
                    perf::record_page_cache_clean_eviction(evicted);
                }
                if readahead_cached_pages > 0 {
                    perf::record_vfs_read_cache_readahead(readahead_cached_pages);
                }
            }

            let copy_len = copy_len.min(read_len - page_offset);
            buf[total_read_size..total_read_size + copy_len]
                .copy_from_slice(&read_buf[page_offset..page_offset + copy_len]);
            total_read_size += copy_len;
            if read_len < valid_len {
                break;
            }
        }

        Some(total_read_size)
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
                    flush_dirty_regular_file(path.node)?;
                    invalidate_regular_file_read_cache(path.node, path.kind);
                    with_mount(path.node.mount_id, |mount| mount.set_len(path.node.ino, 0))
                        .ok_or(FsError::Io)??;
                }
                (path, parent_hint, readable, writable)
            }
        }
        VfsOpenTarget::Create(target) => {
            if flags.contains(OpenFlags::DIRECTORY) {
                return Err(FsError::InvalidInput);
            }
            ensure_mount_writable(target.parent.mount_id)?;
            let parent_stat = with_mount(target.parent.mount_id, |mount| {
                mount.stat(target.parent.ino)
            })
            .ok_or(FsError::Io)??;
            let ino = with_mount(target.parent.mount_id, |mount| {
                mount.create_file(target.parent.ino, target.leaf_name)
            })
            .ok_or(FsError::Io)??;
            dentry_cache::invalidate_parent(target.parent);
            if let Some(attrs) = create_attrs {
                let gid = if parent_stat.mode & MODE_SETGID != 0 {
                    parent_stat.gid
                } else {
                    attrs.gid
                };
                with_mount(target.parent.mount_id, |mount| {
                    mount.set_owner(ino, Some(attrs.uid), Some(gid))
                })
                .ok_or(FsError::Io)??;
                let mode = prepare_created_file_mode(parent_stat, &attrs);
                with_mount(target.parent.mount_id, |mount| mount.set_mode(ino, mode))
                    .ok_or(FsError::Io)??;
            }
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

    let parent_stat = with_mount(directory.node.mount_id, |mount| {
        mount.stat(directory.node.ino)
    })
    .ok_or(FsError::Io)??;
    let (ino, leaf_name) = {
        let mut created = None;
        for _ in 0..TMPFILE_CREATE_ATTEMPTS {
            let seq = TMPFILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let leaf_name = format!(".whusp-tmpfile-{seq:x}");
            let result = with_mount(directory.node.mount_id, |mount| {
                mount.create_file(directory.node.ino, leaf_name.as_str())
            })
            .ok_or(FsError::Io)?;
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

    if let Some(attrs) = create_attrs {
        let gid = if parent_stat.mode & MODE_SETGID != 0 {
            parent_stat.gid
        } else {
            attrs.gid
        };
        with_mount(directory.node.mount_id, |mount| {
            mount.set_owner(ino, Some(attrs.uid), Some(gid))
        })
        .ok_or(FsError::Io)??;
        let mode = prepare_created_file_mode(parent_stat, &attrs);
        with_mount(directory.node.mount_id, |mount| mount.set_mode(ino, mode))
            .ok_or(FsError::Io)??;
    }

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

    match with_mount(directory.node.mount_id, |mount| {
        mount.unlink(directory.node.ino, leaf_name.as_str())
    })
    .ok_or(FsError::Io)?
    {
        Ok(()) => {
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

    let stat =
        with_mount(node.mount_id, |mount| mount.stat(node.ino)).ok_or(FsError::NotFound)??;
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
        flush_dirty_regular_file(node)?;
        invalidate_regular_file_read_cache(node, kind);
        with_mount(node.mount_id, |mount| mount.set_len(node.ino, 0)).ok_or(FsError::Io)??;
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
    let mode = if follow_final_symlink {
        LookupMode::FollowFinal
    } else {
        LookupMode::NoFollowFinal
    };
    let path = vfs_path::resolve_existing_in(context, name, mode)?;
    let mut stat =
        with_mount(path.node.mount_id, |mount| mount.stat(path.node.ino)).ok_or(FsError::Io)??;
    stat.dev = path.node.mount_id.0 as u64;
    if path.kind == FsNodeKind::RegularFile {
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
    let mut stat =
        with_mount(path.node.mount_id, |mount| mount.stat(path.node.ino)).ok_or(FsError::Io)??;
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
    let mut stat =
        with_mount(path.node.mount_id, |mount| mount.stat(path.node.ino)).ok_or(FsError::Io)??;
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
    with_mount(path.node.mount_id, |mount| {
        mount.set_mode(path.node.ino, mode)
    })
    .ok_or(FsError::Io)?
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
    with_mount(path.node.mount_id, |mount| {
        mount.set_owner(path.node.ino, uid, gid)
    })
    .ok_or(FsError::Io)?
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
    flush_dirty_regular_file(path.node)?;
    invalidate_regular_file_read_cache(path.node, path.kind);
    with_mount(path.node.mount_id, |mount| {
        mount.set_len(path.node.ino, len as u64)
    })
    .ok_or(FsError::Io)?
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
        let requested_len = buf.len();
        let noatime_snapshot = self.noatime_snapshot();
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
                .or_else(|| self.read_regular_cached_at(*offset, slice))
                .unwrap_or_else(|| self.read_backend_at(*offset, slice));
                if read_size == 0 {
                    break;
                }
                *offset += read_size;
                total_read_size += read_size;
            }
        }
        drop(offset);
        if requested_len > 0 {
            self.restore_noatime(noatime_snapshot);
        }
        total_read_size
    }

    fn write(&self, buf: UserBuffer) -> usize {
        self.write_inner(buf, false)
    }

    fn write_append(&self, buf: UserBuffer) -> usize {
        self.write_inner(buf, true)
    }

    fn stat(&self) -> FsResult<FileStat> {
        let mut stat = with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino))
            .ok_or(FsError::Io)??;
        stat.dev = self.node.mount_id.0 as u64;
        if self.kind == FsNodeKind::RegularFile {
            overlay_dirty_regular_stat(self.node, &mut stat);
        }
        Ok(stat)
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        let noatime_snapshot = self.noatime_snapshot();
        let has_dirty_pages = dirty_regular_file_has_pages(self.node);
        let read_size = (if has_dirty_pages {
            None
        } else {
            self.read_snapshot_at(offset, buf)
        })
        .or_else(|| self.read_regular_cached_at(offset, buf))
        .unwrap_or_else(|| self.read_backend_at(offset, buf));
        if !buf.is_empty() {
            self.restore_noatime(noatime_snapshot);
        }
        read_size
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        *self.read_snapshot.lock() = None;
        if !buf.is_empty() {
            invalidate_regular_file_read_cache(self.node, self.kind);
        }
        self.write_at_chunks(offset, buf)
    }

    fn set_len(&self, len: usize) -> FsResult {
        if self.kind != FsNodeKind::RegularFile {
            return Err(FsError::InvalidInput);
        }
        if !self.writable {
            return Err(FsError::PermissionDenied);
        }
        self.check_set_len(len)?;
        flush_dirty_regular_file(self.node)?;
        invalidate_regular_file_read_cache(self.node, self.kind);
        with_mount(self.node.mount_id, |mount| {
            mount.set_len(self.node.ino, len as u64)
        })
        .ok_or(FsError::Io)?
    }

    fn allocate_range(&self, offset: usize, len: usize, keep_size: bool) -> FsResult {
        if self.kind != FsNodeKind::RegularFile {
            return Err(FsError::InvalidInput);
        }
        if !self.writable {
            return Err(FsError::PermissionDenied);
        }
        self.check_write_at(offset, len)?;
        flush_dirty_regular_file(self.node)?;
        invalidate_regular_file_read_cache(self.node, self.kind);
        with_mount(self.node.mount_id, |mount| {
            mount.allocate_range(self.node.ino, offset as u64, len as u64, keep_size)
        })
        .ok_or(FsError::Io)?
    }

    fn zero_range(&self, offset: usize, len: usize, keep_size: bool) -> FsResult {
        if self.kind != FsNodeKind::RegularFile {
            return Err(FsError::InvalidInput);
        }
        if !self.writable {
            return Err(FsError::PermissionDenied);
        }
        self.check_write_at(offset, len)?;
        flush_dirty_regular_file(self.node)?;
        invalidate_regular_file_read_cache(self.node, self.kind);
        with_mount(self.node.mount_id, |mount| {
            mount.zero_range(self.node.ino, offset as u64, len as u64, keep_size)
        })
        .ok_or(FsError::Io)?
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
        flush_dirty_regular_file(self.node)?;
        invalidate_regular_file_read_cache(self.node, self.kind);
        with_mount(self.node.mount_id, |mount| {
            mount.punch_hole(self.node.ino, offset as u64, len as u64)
        })
        .ok_or(FsError::Io)?
    }

    fn sync(&self, data_only: bool) -> FsResult {
        flush_dirty_regular_file(self.node)?;
        with_mount(self.node.mount_id, |mount| {
            mount.sync(self.node.ino, data_only)
        })
        .ok_or(FsError::Io)?
    }

    fn seek(&self, offset: i64, whence: SeekWhence) -> FsResult<usize> {
        let mut current = self.offset.lock();
        let base = match whence {
            SeekWhence::Set => 0i128,
            SeekWhence::Current => *current as i128,
            SeekWhence::End => {
                let stat = with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino))
                    .ok_or(FsError::Io)??;
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
        let mut kernel_buf = vec![0u8; user_buf.len()];
        let current_offset = *offset as u64;
        let (read_size, next_offset) = if current_offset >= SYNTHETIC_DIRENT_OFFSET_BASE {
            self.read_synthetic_dirent64(
                current_offset - SYNTHETIC_DIRENT_OFFSET_BASE,
                &mut kernel_buf,
            )?
        } else {
            let (read_size, next_offset) = with_mount(self.node.mount_id, |mount| {
                mount.read_dirent64(self.node.ino, current_offset, &mut kernel_buf)
            })
            .ok_or(FsError::Io)??;
            if read_size == 0 {
                // Synthetic mountpoint dirents are appended after backend EOF
                // and resume from a disjoint high offset range, so real
                // filesystem offsets never collide with VFS overlay entries.
                self.read_synthetic_dirent64(0, &mut kernel_buf)?
            } else {
                (read_size, next_offset)
            }
        };
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
        with_mount(self.node.mount_id, |mount| {
            mount.readlink(self.node.ino, buf)
        })
        .ok_or(FsError::Io)?
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
        with_mount(self.node.mount_id, |mount| {
            mount.set_times(self.node.ino, atime, mtime, ctime)
        })
        .ok_or(FsError::Io)?
    }

    fn set_mode(&self, mode: u32) -> FsResult {
        with_mount(self.node.mount_id, |mount| {
            mount.set_mode(self.node.ino, mode)
        })
        .ok_or(FsError::Io)?
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> FsResult {
        with_mount(self.node.mount_id, |mount| {
            mount.set_owner(self.node.ino, uid, gid)
        })
        .ok_or(FsError::Io)?
    }

    fn inode_flags(&self) -> FsResult<u32> {
        with_mount(self.node.mount_id, |mount| mount.inode_flags(self.node.ino))
            .ok_or(FsError::Io)?
    }

    fn set_inode_flags(&self, flags: u32) -> FsResult {
        with_mount(self.node.mount_id, |mount| {
            mount.set_inode_flags(self.node.ino, flags)
        })
        .ok_or(FsError::Io)?
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
            let stat = with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino))
                .ok_or(FsError::Io)??
                .size;
            stat_logical_size(self.node, stat)
        } else {
            *self.offset.lock() as u64
        };
        with_mount(self.node.mount_id, |mount| {
            mount.check_write_at(self.node.ino, offset, len)
        })
        .ok_or(FsError::Io)?
    }

    fn check_write_at(&self, offset: usize, len: usize) -> FsResult {
        ensure_mount_writable(self.node.mount_id)?;
        let flags = self.inode_flags_or_empty()?;
        if flags & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0 {
            return Err(FsError::PermissionDenied);
        }
        with_mount(self.node.mount_id, |mount| {
            mount.check_write_at(self.node.ino, offset as u64, len)
        })
        .ok_or(FsError::Io)?
    }

    fn check_set_len(&self, len: usize) -> FsResult {
        ensure_mount_writable(self.node.mount_id)?;
        let flags = self.inode_flags_or_empty()?;
        if flags & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0 {
            return Err(FsError::PermissionDenied);
        }
        with_mount(self.node.mount_id, |mount| {
            mount.check_set_len(self.node.ino, len as u64)
        })
        .ok_or(FsError::Io)?
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
        page_cache_id_for_node(self.node, self.kind)
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
        release_inode_from_drop(self.node.mount_id, self.node.ino);
    }
}
