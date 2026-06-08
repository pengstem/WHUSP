use crate::config::PAGE_SIZE;
use crate::sync::UPIntrFreeCell;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use lazy_static::*;

pub(crate) const BLOCK_CACHE_LINE_SIZE: usize = 512;
const DEFAULT_BLOCK_CACHE_CAPACITY: usize = 1024;
const READ_CACHE_LINE_BLOCKS: usize = PAGE_SIZE / BLOCK_CACHE_LINE_SIZE;
const READ_CACHE_LINE_SIZE: usize = READ_CACHE_LINE_BLOCKS * BLOCK_CACHE_LINE_SIZE;
const DEFAULT_READ_CACHE_CAPACITY: usize = 2048;

#[cfg(feature = "perf-counters")]
macro_rules! record_cache_stat {
    ($($body:tt)*) => {
        $($body)*
    };
}

#[cfg(not(feature = "perf-counters"))]
macro_rules! record_cache_stat {
    ($($body:tt)*) => {};
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct BlockCacheKey {
    device_key: usize,
    block_id: usize,
}

impl BlockCacheKey {
    fn new(device_key: usize, block_id: usize) -> Self {
        Self {
            device_key,
            block_id,
        }
    }
}

struct BlockCacheLine {
    data: [u8; BLOCK_CACHE_LINE_SIZE],
    lru_stamp: usize,
}

impl BlockCacheLine {
    fn new(data: [u8; BLOCK_CACHE_LINE_SIZE], lru_stamp: usize) -> Self {
        Self { data, lru_stamp }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct ReadCacheKey {
    device_key: usize,
    line_block_id: usize,
}

impl ReadCacheKey {
    fn new(device_key: usize, line_block_id: usize) -> Self {
        Self {
            device_key,
            line_block_id,
        }
    }
}

struct ReadCacheLine {
    data: Box<[u8]>,
    lru_stamp: usize,
}

impl ReadCacheLine {
    fn new(data: Box<[u8]>, lru_stamp: usize) -> Self {
        Self { data, lru_stamp }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct BlockCacheLruEntry {
    stamp: usize,
    key: BlockCacheKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct ReadCacheLruEntry {
    stamp: usize,
    key: ReadCacheKey,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BlockCacheStats {
    pub(crate) enabled: bool,
    pub(crate) metrics_enabled: bool,
    pub(crate) entries: usize,
    pub(crate) capacity: usize,
    pub(crate) read4k_entries: usize,
    pub(crate) read4k_capacity: usize,
    pub(crate) read4k_hit: usize,
    pub(crate) read4k_miss: usize,
    pub(crate) read4k_fill: usize,
    pub(crate) read4k_evict: usize,
    pub(crate) read4k_invalidate: usize,
    pub(crate) read4k_fallback: usize,
    pub(crate) read4k_lru_touch: usize,
    pub(crate) write4k_update: usize,
    pub(crate) write4k_fallback: usize,
    pub(crate) write4k_legacy_invalidate: usize,
    pub(crate) read_hit: usize,
    pub(crate) read_miss: usize,
    pub(crate) read_fill_sessions: usize,
    pub(crate) write_update: usize,
    pub(crate) write_update_sessions: usize,
    pub(crate) write_invalidate: usize,
    pub(crate) evict: usize,
    pub(crate) device_read_submit: usize,
    pub(crate) device_read_blocks: usize,
    pub(crate) device_read_max_blocks: usize,
    pub(crate) device_write_submit: usize,
    pub(crate) device_write_blocks: usize,
    pub(crate) device_write_max_blocks: usize,
    pub(crate) bypass_unaligned: usize,
    pub(crate) lru_touch: usize,
    pub(crate) lru_scan_slots: usize,
}

struct BlockCache {
    enabled: bool,
    capacity: usize,
    lines: BTreeMap<BlockCacheKey, BlockCacheLine>,
    lru: BTreeSet<BlockCacheLruEntry>,
    lru_clock: usize,
    read_capacity: usize,
    read_lines: BTreeMap<ReadCacheKey, ReadCacheLine>,
    read_lru: BTreeSet<ReadCacheLruEntry>,
    read_lru_clock: usize,
    stats: BlockCacheStats,
}

impl BlockCache {
    fn new(capacity: usize) -> Self {
        Self {
            enabled: true,
            capacity,
            lines: BTreeMap::new(),
            lru: BTreeSet::new(),
            lru_clock: 0,
            read_capacity: DEFAULT_READ_CACHE_CAPACITY,
            read_lines: BTreeMap::new(),
            read_lru: BTreeSet::new(),
            read_lru_clock: 0,
            stats: BlockCacheStats {
                enabled: true,
                capacity,
                read4k_capacity: DEFAULT_READ_CACHE_CAPACITY,
                ..BlockCacheStats::default()
            },
        }
    }

    fn touch(&mut self, key: BlockCacheKey, old_stamp: Option<usize>) -> usize {
        record_cache_stat! {
            self.stats.lru_touch += 1;
        }
        if let Some(stamp) = old_stamp {
            self.lru.remove(&BlockCacheLruEntry { stamp, key });
        }
        self.lru_clock = self.lru_clock.wrapping_add(1);
        let stamp = self.lru_clock;
        self.lru.insert(BlockCacheLruEntry { stamp, key });
        stamp
    }

    fn touch_read4k(&mut self, key: ReadCacheKey, old_stamp: Option<usize>) -> usize {
        record_cache_stat! {
            self.stats.read4k_lru_touch += 1;
        }
        if let Some(stamp) = old_stamp {
            self.read_lru.remove(&ReadCacheLruEntry { stamp, key });
        }
        self.read_lru_clock = self.read_lru_clock.wrapping_add(1);
        let stamp = self.read_lru_clock;
        self.read_lru.insert(ReadCacheLruEntry { stamp, key });
        stamp
    }

    fn trim_to_capacity(&mut self) {
        while self.lines.len() > self.capacity {
            let Some(victim) = self.lru.iter().next().copied() else {
                break;
            };
            self.lru.remove(&victim);
            if self.lines.remove(&victim.key).is_some() {
                record_cache_stat! {
                    self.stats.evict += 1;
                }
            }
        }
    }

    fn trim_read4k_to_capacity(&mut self) {
        while self.read_lines.len() > self.read_capacity {
            let Some(victim) = self.read_lru.iter().next().copied() else {
                break;
            };
            self.read_lru.remove(&victim);
            if self.read_lines.remove(&victim.key).is_some() {
                record_cache_stat! {
                    self.stats.read4k_evict += 1;
                }
            }
        }
    }

    fn read4k_or_miss(&mut self, device_key: usize, block_id: usize, buf: &mut [u8]) -> bool {
        debug_assert_eq!(buf.len(), READ_CACHE_LINE_SIZE);
        if !self.enabled || self.read_capacity == 0 {
            return false;
        }
        let key = ReadCacheKey::new(device_key, block_id);
        if let Some(line) = self.read_lines.get(&key) {
            debug_assert_eq!(line.data.len(), READ_CACHE_LINE_SIZE);
            buf.copy_from_slice(line.data.as_ref());
            let old_stamp = line.lru_stamp;
            record_cache_stat! {
                self.stats.read4k_hit += 1;
            }
            let stamp = self.touch_read4k(key, Some(old_stamp));
            if let Some(line) = self.read_lines.get_mut(&key) {
                line.lru_stamp = stamp;
            }
            return true;
        }
        record_cache_stat! {
            self.stats.read4k_miss += 1;
        }
        false
    }

    fn insert_read4k(&mut self, device_key: usize, block_id: usize, data: &[u8]) {
        debug_assert_eq!(data.len(), READ_CACHE_LINE_SIZE);
        if !self.enabled || self.read_capacity == 0 {
            return;
        }
        let key = ReadCacheKey::new(device_key, block_id);
        let old_stamp = self.read_lines.get(&key).map(|line| line.lru_stamp);
        let stamp = self.touch_read4k(key, old_stamp);
        self.read_lines.insert(
            key,
            ReadCacheLine::new(data.to_vec().into_boxed_slice(), stamp),
        );
        record_cache_stat! {
            self.stats.read4k_fill += 1;
        }
        self.trim_read4k_to_capacity();
    }

    fn update_after_write4k(&mut self, device_key: usize, block_id: usize, data: &[u8]) {
        debug_assert_eq!(data.len(), READ_CACHE_LINE_SIZE);
        if !self.enabled || self.read_capacity == 0 {
            return;
        }
        let key = ReadCacheKey::new(device_key, block_id);
        let old_stamp = self.read_lines.get(&key).map(|line| line.lru_stamp);
        let stamp = self.touch_read4k(key, old_stamp);
        self.read_lines.insert(
            key,
            ReadCacheLine::new(data.to_vec().into_boxed_slice(), stamp),
        );
        record_cache_stat! {
            self.stats.write4k_update += 1;
        }
        self.trim_read4k_to_capacity();
    }

    fn invalidate_read4k_range(&mut self, device_key: usize, block_id: usize, blocks: usize) {
        if !self.enabled || blocks == 0 {
            return;
        }
        let end_block = block_id.saturating_add(blocks);
        let first_overlapping_line = block_id.saturating_sub(READ_CACHE_LINE_BLOCKS - 1);
        let victims: Vec<ReadCacheKey> = self
            .read_lines
            .range(
                ReadCacheKey::new(device_key, first_overlapping_line)
                    ..ReadCacheKey::new(device_key, end_block),
            )
            .map(|(key, _line)| *key)
            .filter(|key| {
                ranges_overlap(
                    key.line_block_id,
                    key.line_block_id.saturating_add(READ_CACHE_LINE_BLOCKS),
                    block_id,
                    end_block,
                )
            })
            .collect();
        for key in victims {
            if let Some(line) = self.read_lines.remove(&key) {
                self.read_lru.remove(&ReadCacheLruEntry {
                    stamp: line.lru_stamp,
                    key,
                });
                record_cache_stat! {
                    self.stats.read4k_invalidate += 1;
                }
            }
        }
    }

    fn invalidate_legacy_range_after_write(
        &mut self,
        device_key: usize,
        block_id: usize,
        blocks: usize,
    ) {
        if !self.enabled || blocks == 0 {
            return;
        }
        for offset in 0..blocks {
            let key = BlockCacheKey::new(device_key, block_id + offset);
            if let Some(line) = self.lines.remove(&key) {
                self.lru.remove(&BlockCacheLruEntry {
                    stamp: line.lru_stamp,
                    key,
                });
                record_cache_stat! {
                    self.stats.write_invalidate += 1;
                    self.stats.write4k_legacy_invalidate += 1;
                }
            }
        }
    }

    #[cfg(feature = "perf-counters")]
    fn record_read4k_fallback(&mut self) {
        record_cache_stat! {
            self.stats.read4k_fallback += 1;
        }
    }

    #[cfg(feature = "perf-counters")]
    fn record_write4k_fallback(&mut self) {
        record_cache_stat! {
            self.stats.write4k_fallback += 1;
        }
    }

    fn read_or_miss_run(
        &mut self,
        device_key: usize,
        block_id: usize,
        max_blocks: usize,
        first_buf: &mut [u8],
    ) -> Option<usize> {
        debug_assert_eq!(first_buf.len(), BLOCK_CACHE_LINE_SIZE);
        debug_assert!(max_blocks > 0);
        if !self.enabled {
            return Some(max_blocks);
        }
        let key = BlockCacheKey::new(device_key, block_id);
        if let Some(line) = self.lines.get(&key) {
            first_buf.copy_from_slice(&line.data);
            let old_stamp = line.lru_stamp;
            record_cache_stat! {
                self.stats.read_hit += 1;
            }
            let stamp = self.touch(key, Some(old_stamp));
            if let Some(line) = self.lines.get_mut(&key) {
                line.lru_stamp = stamp;
            }
            return None;
        }

        record_cache_stat! {
            self.stats.read_miss += 1;
        }
        let mut blocks = 1;
        while blocks < max_blocks {
            let key = BlockCacheKey::new(device_key, block_id + blocks);
            if self.lines.contains_key(&key) {
                break;
            }
            record_cache_stat! {
                self.stats.read_miss += 1;
            }
            blocks += 1;
        }
        Some(blocks)
    }

    fn insert_read(&mut self, key: BlockCacheKey, data: [u8; BLOCK_CACHE_LINE_SIZE]) {
        if !self.enabled || self.capacity == 0 {
            return;
        }
        let old_stamp = self.lines.get(&key).map(|line| line.lru_stamp);
        let stamp = self.touch(key, old_stamp);
        self.lines.insert(key, BlockCacheLine::new(data, stamp));
        self.trim_to_capacity();
    }

    fn insert_read_run(&mut self, device_key: usize, block_id: usize, data: &[u8]) {
        if !self.enabled || self.capacity == 0 {
            return;
        }
        record_cache_stat! {
            self.stats.read_fill_sessions += 1;
        }
        for (offset, chunk) in data.chunks(BLOCK_CACHE_LINE_SIZE).enumerate() {
            if chunk.len() != BLOCK_CACHE_LINE_SIZE {
                break;
            }
            let mut line = [0u8; BLOCK_CACHE_LINE_SIZE];
            line.copy_from_slice(chunk);
            self.insert_read(BlockCacheKey::new(device_key, block_id + offset), line);
        }
    }

    fn update_after_write(&mut self, key: BlockCacheKey, data: [u8; BLOCK_CACHE_LINE_SIZE]) {
        if !self.enabled || self.capacity == 0 {
            return;
        }
        let old_stamp = self.lines.get(&key).map(|line| line.lru_stamp);
        let stamp = self.touch(key, old_stamp);
        self.lines.insert(key, BlockCacheLine::new(data, stamp));
        record_cache_stat! {
            self.stats.write_update += 1;
        }
        self.trim_to_capacity();
    }

    fn update_after_write_run(&mut self, device_key: usize, block_id: usize, data: &[u8]) {
        if !self.enabled || self.capacity == 0 {
            return;
        }
        record_cache_stat! {
            self.stats.write_update_sessions += 1;
        }
        for (offset, chunk) in data.chunks(BLOCK_CACHE_LINE_SIZE).enumerate() {
            if chunk.len() != BLOCK_CACHE_LINE_SIZE {
                break;
            }
            let mut line = [0u8; BLOCK_CACHE_LINE_SIZE];
            line.copy_from_slice(chunk);
            self.update_after_write(BlockCacheKey::new(device_key, block_id + offset), line);
        }
    }

    fn invalidate_key_after_write(&mut self, key: BlockCacheKey) {
        if !self.enabled {
            return;
        }
        if let Some(line) = self.lines.remove(&key) {
            self.lru.remove(&BlockCacheLruEntry {
                stamp: line.lru_stamp,
                key,
            });
            record_cache_stat! {
                self.stats.write_invalidate += 1;
            }
        }
    }

    #[cfg(feature = "perf-counters")]
    fn record_device_read_submit(&mut self, blocks: usize) {
        record_cache_stat! {
            self.stats.device_read_submit += 1;
            self.stats.device_read_blocks += blocks;
            self.stats.device_read_max_blocks = self.stats.device_read_max_blocks.max(blocks);
        }
    }

    #[cfg(feature = "perf-counters")]
    fn record_device_write_submit(&mut self, blocks: usize) {
        record_cache_stat! {
            self.stats.device_write_submit += 1;
            self.stats.device_write_blocks += blocks;
            self.stats.device_write_max_blocks = self.stats.device_write_max_blocks.max(blocks);
        }
    }

    #[cfg(feature = "perf-counters")]
    fn record_bypass_unaligned(&mut self) {
        record_cache_stat! {
            self.stats.bypass_unaligned += 1;
        }
    }

    fn stats_snapshot(&self) -> BlockCacheStats {
        BlockCacheStats {
            enabled: self.enabled,
            metrics_enabled: cfg!(feature = "perf-counters"),
            entries: self.lines.len(),
            capacity: self.capacity,
            read4k_entries: self.read_lines.len(),
            read4k_capacity: self.read_capacity,
            ..self.stats
        }
    }
}

lazy_static! {
    // CONTEXT: The block cache is write-through and stores only clean 512-byte
    // lines. Full-sector writes update cached lines after device submission;
    // partial writes invalidate so later reads cannot observe stale sectors.
    static ref BLOCK_CACHE: UPIntrFreeCell<BlockCache> =
        unsafe { UPIntrFreeCell::new(BlockCache::new(DEFAULT_BLOCK_CACHE_CAPACITY)) };
}

fn cache_read_or_miss_run(
    device_key: usize,
    block_id: usize,
    max_blocks: usize,
    first_buf: &mut [u8],
) -> Option<usize> {
    BLOCK_CACHE
        .exclusive_access()
        .read_or_miss_run(device_key, block_id, max_blocks, first_buf)
}

fn cache_read4k_or_miss(device_key: usize, block_id: usize, buf: &mut [u8]) -> bool {
    BLOCK_CACHE
        .exclusive_access()
        .read4k_or_miss(device_key, block_id, buf)
}

fn cache_insert_read4k(device_key: usize, block_id: usize, data: &[u8]) {
    BLOCK_CACHE
        .exclusive_access()
        .insert_read4k(device_key, block_id, data);
}

fn cache_update_after_write4k(device_key: usize, block_id: usize, data: &[u8]) {
    BLOCK_CACHE
        .exclusive_access()
        .update_after_write4k(device_key, block_id, data);
}

fn cache_insert_read_run(device_key: usize, block_id: usize, data: &[u8]) {
    BLOCK_CACHE
        .exclusive_access()
        .insert_read_run(device_key, block_id, data);
}

fn cache_update_after_write_run(device_key: usize, block_id: usize, data: &[u8]) {
    BLOCK_CACHE
        .exclusive_access()
        .update_after_write_run(device_key, block_id, data);
}

fn cache_invalidate_read4k_range(device_key: usize, block_id: usize, blocks: usize) {
    BLOCK_CACHE
        .exclusive_access()
        .invalidate_read4k_range(device_key, block_id, blocks);
}

fn cache_invalidate_legacy_range_after_write(device_key: usize, block_id: usize, blocks: usize) {
    BLOCK_CACHE
        .exclusive_access()
        .invalidate_legacy_range_after_write(device_key, block_id, blocks);
}

fn cache_invalidate_key_after_write(device_key: usize, block_id: usize) {
    let key = BlockCacheKey::new(device_key, block_id);
    BLOCK_CACHE
        .exclusive_access()
        .invalidate_key_after_write(key);
}

#[cfg(feature = "perf-counters")]
fn record_device_read_submit(blocks: usize) {
    BLOCK_CACHE
        .exclusive_access()
        .record_device_read_submit(blocks);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_device_read_submit(_blocks: usize) {}

#[cfg(feature = "perf-counters")]
fn record_device_write_submit(blocks: usize) {
    BLOCK_CACHE
        .exclusive_access()
        .record_device_write_submit(blocks);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_device_write_submit(_blocks: usize) {}

#[cfg(feature = "perf-counters")]
fn record_bypass_unaligned() {
    BLOCK_CACHE.exclusive_access().record_bypass_unaligned();
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_bypass_unaligned() {}

#[cfg(feature = "perf-counters")]
fn record_read4k_fallback() {
    BLOCK_CACHE.exclusive_access().record_read4k_fallback();
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_read4k_fallback() {}

#[cfg(feature = "perf-counters")]
fn record_write4k_fallback() {
    BLOCK_CACHE.exclusive_access().record_write4k_fallback();
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_write4k_fallback() {}

fn page_bounded_full_blocks(buf: &[u8], max_blocks: usize) -> usize {
    if max_blocks == 0 {
        return 0;
    }
    // VirtioHal::share() translates the first byte of each slice. Keep each
    // cached submission inside one kernel page so DMA never assumes adjacent
    // virtual pages are physically contiguous.
    let page_offset = (buf.as_ptr() as usize) & (PAGE_SIZE - 1);
    let page_remaining = PAGE_SIZE - page_offset;
    (page_remaining / BLOCK_CACHE_LINE_SIZE)
        .max(1)
        .min(max_blocks)
}

fn ranges_overlap(a_start: usize, a_end: usize, b_start: usize, b_end: usize) -> bool {
    a_start < b_end && b_start < a_end
}

fn can_use_read4k(_block_id: usize, buf: &[u8], remaining_blocks: usize) -> bool {
    remaining_blocks >= READ_CACHE_LINE_BLOCKS
        && page_bounded_full_blocks(buf, remaining_blocks) >= READ_CACHE_LINE_BLOCKS
}

fn can_use_write4k(buf: &[u8], remaining_blocks: usize) -> bool {
    remaining_blocks >= READ_CACHE_LINE_BLOCKS
        && page_bounded_full_blocks(buf, remaining_blocks) >= READ_CACHE_LINE_BLOCKS
}

pub(crate) fn read_with_cache<F>(
    device_key: usize,
    block_id: usize,
    buf: &mut [u8],
    mut read_uncached: F,
) where
    F: FnMut(usize, &mut [u8]),
{
    let full_blocks = buf.len() / BLOCK_CACHE_LINE_SIZE;
    let full_bytes = full_blocks * BLOCK_CACHE_LINE_SIZE;
    let mut index = 0;
    while index < full_blocks {
        let start = index * BLOCK_CACHE_LINE_SIZE;
        if can_use_read4k(
            block_id + index,
            &buf[start..full_bytes],
            full_blocks - index,
        ) {
            let end = start + READ_CACHE_LINE_SIZE;
            if !cache_read4k_or_miss(device_key, block_id + index, &mut buf[start..end]) {
                record_device_read_submit(READ_CACHE_LINE_BLOCKS);
                read_uncached(block_id + index, &mut buf[start..end]);
                cache_insert_read4k(device_key, block_id + index, &buf[start..end]);
            }
            index += READ_CACHE_LINE_BLOCKS;
            continue;
        }
        record_read4k_fallback();
        let max_blocks = page_bounded_full_blocks(&buf[start..full_bytes], full_blocks - index);
        match cache_read_or_miss_run(
            device_key,
            block_id + index,
            max_blocks,
            &mut buf[start..start + BLOCK_CACHE_LINE_SIZE],
        ) {
            None => {
                index += 1;
            }
            Some(blocks) => {
                let end = start + blocks * BLOCK_CACHE_LINE_SIZE;
                record_device_read_submit(blocks);
                read_uncached(block_id + index, &mut buf[start..end]);
                cache_insert_read_run(device_key, block_id + index, &buf[start..end]);
                index += blocks;
            }
        }
    }

    let tail = &mut buf[full_bytes..];
    if !tail.is_empty() {
        record_bypass_unaligned();
        record_device_read_submit(1);
        read_uncached(block_id + full_blocks, tail);
    }
}

pub(crate) fn write_with_cache<F>(
    device_key: usize,
    block_id: usize,
    buf: &[u8],
    mut write_uncached: F,
) where
    F: FnMut(usize, &[u8]),
{
    let full_blocks = buf.len() / BLOCK_CACHE_LINE_SIZE;
    let full_bytes = full_blocks * BLOCK_CACHE_LINE_SIZE;
    let mut index = 0;
    while index < full_blocks {
        let start = index * BLOCK_CACHE_LINE_SIZE;
        if can_use_write4k(&buf[start..full_bytes], full_blocks - index) {
            let end = start + READ_CACHE_LINE_SIZE;
            record_device_write_submit(READ_CACHE_LINE_BLOCKS);
            write_uncached(block_id + index, &buf[start..end]);
            cache_invalidate_read4k_range(device_key, block_id + index, READ_CACHE_LINE_BLOCKS);
            cache_invalidate_legacy_range_after_write(
                device_key,
                block_id + index,
                READ_CACHE_LINE_BLOCKS,
            );
            cache_update_after_write4k(device_key, block_id + index, &buf[start..end]);
            index += READ_CACHE_LINE_BLOCKS;
            continue;
        }
        record_write4k_fallback();
        let blocks = page_bounded_full_blocks(&buf[start..full_bytes], full_blocks - index);
        let end = start + blocks * BLOCK_CACHE_LINE_SIZE;
        record_device_write_submit(blocks);
        write_uncached(block_id + index, &buf[start..end]);
        cache_invalidate_read4k_range(device_key, block_id + index, blocks);
        cache_update_after_write_run(device_key, block_id + index, &buf[start..end]);
        index += blocks;
    }

    let tail = &buf[full_bytes..];
    if !tail.is_empty() {
        record_bypass_unaligned();
        record_write4k_fallback();
        record_device_write_submit(1);
        write_uncached(block_id + full_blocks, tail);
        cache_invalidate_read4k_range(device_key, block_id + full_blocks, 1);
        cache_invalidate_key_after_write(device_key, block_id + full_blocks);
    }
}

pub(crate) fn stats_snapshot() -> BlockCacheStats {
    BLOCK_CACHE.exclusive_access().stats_snapshot()
}

pub(crate) fn stats_content() -> String {
    let stats = stats_snapshot();
    format!(
        "enabled {}\nmetrics_enabled {}\nentries {}\ncapacity {}\nread4k_entries {}\nread4k_capacity {}\nread4k_hit {}\nread4k_miss {}\nread4k_fill {}\nread4k_evict {}\nread4k_invalidate {}\nread4k_fallback {}\nread4k_lru_touch {}\nwrite4k_update {}\nwrite4k_fallback {}\nwrite4k_legacy_invalidate {}\nread_hit {}\nread_miss {}\nread_fill_sessions {}\nwrite_update {}\nwrite_update_sessions {}\nwrite_invalidate {}\nevict {}\ndevice_read_submit {}\ndevice_read_blocks {}\ndevice_read_max_blocks {}\ndevice_write_submit {}\ndevice_write_blocks {}\ndevice_write_max_blocks {}\nbypass_unaligned {}\nlru_touch {}\nlru_scan_slots {}\n",
        stats.enabled as usize,
        stats.metrics_enabled as usize,
        stats.entries,
        stats.capacity,
        stats.read4k_entries,
        stats.read4k_capacity,
        stats.read4k_hit,
        stats.read4k_miss,
        stats.read4k_fill,
        stats.read4k_evict,
        stats.read4k_invalidate,
        stats.read4k_fallback,
        stats.read4k_lru_touch,
        stats.write4k_update,
        stats.write4k_fallback,
        stats.write4k_legacy_invalidate,
        stats.read_hit,
        stats.read_miss,
        stats.read_fill_sessions,
        stats.write_update,
        stats.write_update_sessions,
        stats.write_invalidate,
        stats.evict,
        stats.device_read_submit,
        stats.device_read_blocks,
        stats.device_read_max_blocks,
        stats.device_write_submit,
        stats.device_write_blocks,
        stats.device_write_max_blocks,
        stats.bypass_unaligned,
        stats.lru_touch,
        stats.lru_scan_slots,
    )
}
