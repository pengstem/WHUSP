use crate::sync::UPIntrFreeCell;
use alloc::boxed::Box;
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
    pub(crate) write_update: usize,
    pub(crate) write_invalidate: usize,
    pub(crate) evict: usize,
    pub(crate) device_read_submit: usize,
    pub(crate) device_write_submit: usize,
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

    fn try_read(&mut self, key: BlockCacheKey, buf: &mut [u8]) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(line) = self.lines.get(&key) else {
            self.stats.read_miss += 1;
            return false;
        };
        buf.copy_from_slice(&line.data);
        let old_stamp = line.lru_stamp;
        self.stats.read_hit += 1;
        let stamp = self.touch(key, Some(old_stamp));
        if let Some(line) = self.lines.get_mut(&key) {
            line.lru_stamp = stamp;
        }
        true
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

    fn record_device_read_submit(&mut self) {
        self.stats.device_read_submit += 1;
    }

    fn record_device_write_submit(&mut self) {
        self.stats.device_write_submit += 1;
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

fn cache_try_read(device_key: usize, block_id: usize, buf: &mut [u8]) -> bool {
    let key = BlockCacheKey::new(device_key, block_id);
    BLOCK_CACHE.exclusive_access().try_read(key, buf)
}

fn cache_insert_read(device_key: usize, block_id: usize, data: [u8; BLOCK_CACHE_LINE_SIZE]) {
    let key = BlockCacheKey::new(device_key, block_id);
    BLOCK_CACHE.exclusive_access().insert_read(key, data);
}

fn cache_update_after_write(device_key: usize, block_id: usize, data: [u8; BLOCK_CACHE_LINE_SIZE]) {
    let key = BlockCacheKey::new(device_key, block_id);
    BLOCK_CACHE.exclusive_access().update_after_write(key, data);
}

fn cache_invalidate_key_after_write(device_key: usize, block_id: usize) {
    let key = BlockCacheKey::new(device_key, block_id);
    BLOCK_CACHE
        .exclusive_access()
        .invalidate_key_after_write(key);
}

fn record_device_read_submit() {
    BLOCK_CACHE.exclusive_access().record_device_read_submit();
}

fn record_device_write_submit() {
    BLOCK_CACHE.exclusive_access().record_device_write_submit();
}

fn record_bypass_unaligned() {
    BLOCK_CACHE.exclusive_access().record_bypass_unaligned();
}

pub(crate) fn read_with_cache<F>(
    device_key: usize,
    block_id: usize,
    buf: &mut [u8],
    mut read_uncached: F,
) where
    F: FnMut(usize, &mut [u8]),
{
    for (index, chunk) in buf.chunks_mut(BLOCK_CACHE_LINE_SIZE).enumerate() {
        let current_block = block_id + index;
        if chunk.len() != BLOCK_CACHE_LINE_SIZE {
            record_bypass_unaligned();
            record_device_read_submit();
            read_uncached(current_block, chunk);
            continue;
        }
        if cache_try_read(device_key, current_block, chunk) {
            continue;
        }
        let mut line = Box::new([0u8; BLOCK_CACHE_LINE_SIZE]);
        record_device_read_submit();
        read_uncached(current_block, line.as_mut());
        chunk.copy_from_slice(line.as_ref());
        cache_insert_read(device_key, current_block, *line);
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
    for (index, chunk) in buf.chunks(BLOCK_CACHE_LINE_SIZE).enumerate() {
        let current_block = block_id + index;
        if chunk.len() != BLOCK_CACHE_LINE_SIZE {
            record_bypass_unaligned();
            record_device_write_submit();
            write_uncached(current_block, chunk);
            cache_invalidate_key_after_write(device_key, current_block);
            continue;
        }
        record_device_write_submit();
        write_uncached(current_block, chunk);
        let mut line = [0u8; BLOCK_CACHE_LINE_SIZE];
        line.copy_from_slice(chunk);
        cache_update_after_write(device_key, current_block, line);
    }
}

pub(crate) fn stats_snapshot() -> BlockCacheStats {
    BLOCK_CACHE.exclusive_access().stats_snapshot()
}

pub(crate) fn stats_content() -> String {
    let stats = stats_snapshot();
    format!(
        "enabled {}\nentries {}\ncapacity {}\nread_hit {}\nread_miss {}\nwrite_update {}\nwrite_invalidate {}\nevict {}\ndevice_read_submit {}\ndevice_write_submit {}\nbypass_unaligned {}\nlru_touch {}\nlru_scan_slots {}\n",
        stats.enabled as usize,
        stats.entries,
        stats.capacity,
        stats.read_hit,
        stats.read_miss,
        stats.write_update,
        stats.write_invalidate,
        stats.evict,
        stats.device_read_submit,
        stats.device_write_submit,
        stats.bypass_unaligned,
        stats.lru_touch,
        stats.lru_scan_slots,
    )
}
