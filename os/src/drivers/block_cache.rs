use crate::config::PAGE_SIZE;
use crate::sync::UPIntrFreeCell;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use lazy_static::*;

pub(crate) const BLOCK_CACHE_LINE_SIZE: usize = 512;
const DEFAULT_BLOCK_CACHE_CAPACITY: usize = 1024;

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
struct BlockCacheLruEntry {
    stamp: usize,
    key: BlockCacheKey,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BlockCacheStats {
    pub(crate) enabled: bool,
    pub(crate) entries: usize,
    pub(crate) capacity: usize,
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
            stats: BlockCacheStats {
                enabled: true,
                capacity,
                ..BlockCacheStats::default()
            },
        }
    }

    fn touch(&mut self, key: BlockCacheKey, old_stamp: Option<usize>) -> usize {
        self.stats.lru_touch += 1;
        if let Some(stamp) = old_stamp {
            self.lru.remove(&BlockCacheLruEntry { stamp, key });
        }
        self.lru_clock = self.lru_clock.wrapping_add(1);
        let stamp = self.lru_clock;
        self.lru.insert(BlockCacheLruEntry { stamp, key });
        stamp
    }

    fn trim_to_capacity(&mut self) {
        while self.lines.len() > self.capacity {
            let Some(victim) = self.lru.iter().next().copied() else {
                break;
            };
            self.lru.remove(&victim);
            if self.lines.remove(&victim.key).is_some() {
                self.stats.evict += 1;
            }
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
            self.stats.read_hit += 1;
            let stamp = self.touch(key, Some(old_stamp));
            if let Some(line) = self.lines.get_mut(&key) {
                line.lru_stamp = stamp;
            }
            return None;
        }

        self.stats.read_miss += 1;
        let mut blocks = 1;
        while blocks < max_blocks {
            let key = BlockCacheKey::new(device_key, block_id + blocks);
            if self.lines.contains_key(&key) {
                break;
            }
            self.stats.read_miss += 1;
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
        self.stats.read_fill_sessions += 1;
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
        self.stats.write_update += 1;
        self.trim_to_capacity();
    }

    fn update_after_write_run(&mut self, device_key: usize, block_id: usize, data: &[u8]) {
        if !self.enabled || self.capacity == 0 {
            return;
        }
        self.stats.write_update_sessions += 1;
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
            self.stats.write_invalidate += 1;
        }
    }

    fn record_device_read_submit(&mut self, blocks: usize) {
        self.stats.device_read_submit += 1;
        self.stats.device_read_blocks += blocks;
        self.stats.device_read_max_blocks = self.stats.device_read_max_blocks.max(blocks);
    }

    fn record_device_write_submit(&mut self, blocks: usize) {
        self.stats.device_write_submit += 1;
        self.stats.device_write_blocks += blocks;
        self.stats.device_write_max_blocks = self.stats.device_write_max_blocks.max(blocks);
    }

    fn record_bypass_unaligned(&mut self) {
        self.stats.bypass_unaligned += 1;
    }

    fn stats_snapshot(&self) -> BlockCacheStats {
        BlockCacheStats {
            enabled: self.enabled,
            entries: self.lines.len(),
            capacity: self.capacity,
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

fn cache_invalidate_key_after_write(device_key: usize, block_id: usize) {
    let key = BlockCacheKey::new(device_key, block_id);
    BLOCK_CACHE
        .exclusive_access()
        .invalidate_key_after_write(key);
}

fn record_device_read_submit(blocks: usize) {
    BLOCK_CACHE
        .exclusive_access()
        .record_device_read_submit(blocks);
}

fn record_device_write_submit(blocks: usize) {
    BLOCK_CACHE
        .exclusive_access()
        .record_device_write_submit(blocks);
}

fn record_bypass_unaligned() {
    BLOCK_CACHE.exclusive_access().record_bypass_unaligned();
}

fn page_bounded_full_blocks(buf: &[u8], max_blocks: usize) -> usize {
    if max_blocks == 0 {
        return 0;
    }
    let page_offset = (buf.as_ptr() as usize) & (PAGE_SIZE - 1);
    let page_remaining = PAGE_SIZE - page_offset;
    (page_remaining / BLOCK_CACHE_LINE_SIZE)
        .max(1)
        .min(max_blocks)
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
        let blocks = page_bounded_full_blocks(&buf[start..full_bytes], full_blocks - index);
        let end = start + blocks * BLOCK_CACHE_LINE_SIZE;
        record_device_write_submit(blocks);
        write_uncached(block_id + index, &buf[start..end]);
        cache_update_after_write_run(device_key, block_id + index, &buf[start..end]);
        index += blocks;
    }

    let tail = &buf[full_bytes..];
    if !tail.is_empty() {
        record_bypass_unaligned();
        record_device_write_submit(1);
        write_uncached(block_id + full_blocks, tail);
        cache_invalidate_key_after_write(device_key, block_id + full_blocks);
    }
}

pub(crate) fn stats_snapshot() -> BlockCacheStats {
    BLOCK_CACHE.exclusive_access().stats_snapshot()
}

pub(crate) fn stats_content() -> String {
    let stats = stats_snapshot();
    format!(
        "enabled {}\nentries {}\ncapacity {}\nread_hit {}\nread_miss {}\nread_fill_sessions {}\nwrite_update {}\nwrite_update_sessions {}\nwrite_invalidate {}\nevict {}\ndevice_read_submit {}\ndevice_read_blocks {}\ndevice_read_max_blocks {}\ndevice_write_submit {}\ndevice_write_blocks {}\ndevice_write_max_blocks {}\nbypass_unaligned {}\nlru_touch {}\nlru_scan_slots {}\n",
        stats.enabled as usize,
        stats.entries,
        stats.capacity,
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
