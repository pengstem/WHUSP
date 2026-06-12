use super::mount::MountNamespaceId;
use super::vfs::{FsNodeKind, VfsNodeId};
use crate::sync::UPIntrFreeCell;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use lazy_static::*;

const DEFAULT_DENTRY_CACHE_CAPACITY: usize = 4096;
// FNV-1a 64-bit constants. The full component string stays in each bucket, so
// hash collisions only add a short linear scan and never create false hits.
const DENTRY_HASH_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const DENTRY_HASH_PRIME: u64 = 0x0000_0100_0000_01b3;

#[cfg(feature = "perf-counters")]
macro_rules! record_dentry_stat {
    ($($body:tt)*) => {
        $($body)*
    };
}

#[cfg(not(feature = "perf-counters"))]
macro_rules! record_dentry_stat {
    ($($body:tt)*) => {};
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DentryCacheBucketKey {
    namespace_id: MountNamespaceId,
    parent: VfsNodeId,
    component_hash: u64,
}

impl DentryCacheBucketKey {
    fn new(namespace_id: MountNamespaceId, parent: VfsNodeId, component: &str) -> Self {
        Self {
            namespace_id,
            parent,
            component_hash: hash_component(component),
        }
    }
}

fn hash_component(component: &str) -> u64 {
    let mut hash = DENTRY_HASH_OFFSET;
    for byte in component.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(DENTRY_HASH_PRIME);
    }
    hash
}

#[derive(Clone, Debug)]
struct DentryCacheEntry {
    component: String,
    value: DentryCacheValue,
}

#[derive(Clone, Copy, Debug)]
enum DentryCacheValue {
    // Parent generation is the coherency contract: create/unlink/rename bump
    // the parent and make both positive and negative child entries stale.
    Positive {
        node: VfsNodeId,
        kind: FsNodeKind,
        parent_generation: usize,
        lru_stamp: usize,
    },
    Negative {
        parent_generation: usize,
        lru_stamp: usize,
    },
}

impl DentryCacheValue {
    fn parent_generation(self) -> usize {
        match self {
            Self::Positive {
                parent_generation, ..
            }
            | Self::Negative {
                parent_generation, ..
            } => parent_generation,
        }
    }

    fn lru_stamp(self) -> usize {
        match self {
            Self::Positive { lru_stamp, .. } | Self::Negative { lru_stamp, .. } => lru_stamp,
        }
    }

    fn with_lru_stamp(self, lru_stamp: usize) -> Self {
        match self {
            Self::Positive {
                node,
                kind,
                parent_generation,
                ..
            } => Self::Positive {
                node,
                kind,
                parent_generation,
                lru_stamp,
            },
            Self::Negative {
                parent_generation, ..
            } => Self::Negative {
                parent_generation,
                lru_stamp,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DentryCacheLruEntry {
    stamp: usize,
    bucket: DentryCacheBucketKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DentryLookupResult {
    Positive { node: VfsNodeId, kind: FsNodeKind },
    Negative,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DentryCacheStats {
    pub(crate) enabled: bool,
    pub(crate) entries: usize,
    pub(crate) capacity: usize,
    pub(crate) positive_hit: usize,
    pub(crate) negative_hit: usize,
    pub(crate) miss: usize,
    pub(crate) revalidate_fail: usize,
    pub(crate) insert_positive: usize,
    pub(crate) insert_negative: usize,
    pub(crate) invalidate_parent: usize,
    pub(crate) invalidate_parent_calls: usize,
    pub(crate) invalidate_parent_entry_scans: usize,
    pub(crate) invalidate_parent_lru_scans: usize,
    pub(crate) invalidate_all: usize,
    pub(crate) evict: usize,
    pub(crate) lru_touch: usize,
    pub(crate) lru_scan_slots: usize,
    #[cfg(feature = "perf-counters")]
    pub(crate) key_allocs: usize,
    #[cfg(feature = "perf-counters")]
    pub(crate) collision_scans: usize,
}

struct DentryCache {
    enabled: bool,
    capacity: usize,
    entries: BTreeMap<DentryCacheBucketKey, Vec<DentryCacheEntry>>,
    entry_count: usize,
    parent_generations: BTreeMap<VfsNodeId, usize>,
    lru: BTreeSet<DentryCacheLruEntry>,
    lru_clock: usize,
    stats: DentryCacheStats,
}

impl DentryCache {
    fn new(capacity: usize) -> Self {
        Self {
            enabled: true,
            capacity,
            entries: BTreeMap::new(),
            entry_count: 0,
            parent_generations: BTreeMap::new(),
            lru: BTreeSet::new(),
            lru_clock: 0,
            stats: DentryCacheStats {
                enabled: true,
                capacity,
                ..DentryCacheStats::default()
            },
        }
    }

    fn parent_generation(&self, parent: VfsNodeId) -> usize {
        self.parent_generations.get(&parent).copied().unwrap_or(0)
    }

    #[cfg(feature = "perf-counters")]
    fn record_key_alloc(&mut self) {
        self.stats.key_allocs += 1;
    }

    #[cfg(not(feature = "perf-counters"))]
    #[inline(always)]
    fn record_key_alloc(&mut self) {}

    #[cfg(feature = "perf-counters")]
    fn record_collision_scans(&mut self, scans: usize) {
        self.stats.collision_scans += scans;
    }

    #[cfg(not(feature = "perf-counters"))]
    #[inline(always)]
    fn record_collision_scans(&mut self, _scans: usize) {}

    fn find_entry_index(&mut self, bucket: DentryCacheBucketKey, component: &str) -> Option<usize> {
        let (index, extra_scans) = {
            let Some(entries) = self.entries.get(&bucket) else {
                return None;
            };
            let mut extra_scans = 0;
            let mut found = None;
            for (index, entry) in entries.iter().enumerate() {
                if index > 0 {
                    extra_scans += 1;
                }
                if entry.component == component {
                    found = Some(index);
                    break;
                }
            }
            (found, extra_scans)
        };
        self.record_collision_scans(extra_scans);
        index
    }

    fn touch(&mut self, bucket: DentryCacheBucketKey, old_stamp: Option<usize>) -> usize {
        record_dentry_stat! {
            self.stats.lru_touch += 1;
        }
        if let Some(stamp) = old_stamp {
            let old_lru_entry = DentryCacheLruEntry { stamp, bucket };
            self.lru.remove(&old_lru_entry);
        }
        self.lru_clock = self.lru_clock.wrapping_add(1);
        let stamp = self.lru_clock;
        self.lru.insert(DentryCacheLruEntry { stamp, bucket });
        stamp
    }

    fn touch_hit(&mut self, bucket: DentryCacheBucketKey, stamp: usize) -> usize {
        // PERF: Exact hit recency only matters once insertions can evict entries.
        if self.entry_count < self.capacity {
            return stamp;
        }
        self.touch(bucket, Some(stamp))
    }

    fn remove_entry_at(
        &mut self,
        bucket: DentryCacheBucketKey,
        index: usize,
        lru_stamp: usize,
    ) -> bool {
        let mut remove_bucket = false;
        let removed = if let Some(entries) = self.entries.get_mut(&bucket) {
            if index >= entries.len() {
                false
            } else {
                entries.swap_remove(index);
                self.entry_count = self.entry_count.saturating_sub(1);
                remove_bucket = entries.is_empty();
                true
            }
        } else {
            false
        };
        if remove_bucket {
            self.entries.remove(&bucket);
        }
        if removed {
            self.lru.remove(&DentryCacheLruEntry {
                stamp: lru_stamp,
                bucket,
            });
        }
        removed
    }

    fn trim_to_capacity(&mut self) {
        while self.entry_count > self.capacity {
            let Some(victim) = self.lru.iter().next().copied() else {
                break;
            };
            self.lru.remove(&victim);
            let mut remove_bucket = false;
            let removed = if let Some(entries) = self.entries.get_mut(&victim.bucket) {
                record_dentry_stat! {
                    self.stats.lru_scan_slots += entries.len();
                }
                if let Some(index) = entries
                    .iter()
                    .position(|entry| entry.value.lru_stamp() == victim.stamp)
                {
                    entries.swap_remove(index);
                    self.entry_count = self.entry_count.saturating_sub(1);
                    remove_bucket = entries.is_empty();
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if remove_bucket {
                self.entries.remove(&victim.bucket);
            }
            if removed {
                record_dentry_stat! {
                    self.stats.evict += 1;
                }
            }
        }
    }

    fn lookup(
        &mut self,
        bucket: DentryCacheBucketKey,
        parent: VfsNodeId,
        component: &str,
    ) -> Option<DentryLookupResult> {
        if !self.enabled {
            return None;
        }
        let Some(index) = self.find_entry_index(bucket, component) else {
            record_dentry_stat! {
                self.stats.miss += 1;
            }
            return None;
        };
        let Some(value) = self
            .entries
            .get(&bucket)
            .and_then(|entries| entries.get(index))
            .map(|entry| entry.value)
        else {
            record_dentry_stat! {
                self.stats.miss += 1;
            }
            return None;
        };
        if value.parent_generation() != self.parent_generation(parent) {
            self.remove_entry_at(bucket, index, value.lru_stamp());
            record_dentry_stat! {
                self.stats.revalidate_fail += 1;
            }
            return None;
        }
        let stamp = self.touch_hit(bucket, value.lru_stamp());
        if let Some(entry) = self
            .entries
            .get_mut(&bucket)
            .and_then(|entries| entries.get_mut(index))
        {
            entry.value = value.with_lru_stamp(stamp);
        }
        match value {
            DentryCacheValue::Positive { node, kind, .. } => {
                record_dentry_stat! {
                    self.stats.positive_hit += 1;
                }
                Some(DentryLookupResult::Positive { node, kind })
            }
            DentryCacheValue::Negative { .. } => {
                record_dentry_stat! {
                    self.stats.negative_hit += 1;
                }
                Some(DentryLookupResult::Negative)
            }
        }
    }

    fn insert_positive(
        &mut self,
        bucket: DentryCacheBucketKey,
        parent: VfsNodeId,
        component: &str,
        node: VfsNodeId,
        kind: FsNodeKind,
    ) {
        if !self.enabled || self.capacity == 0 {
            return;
        }
        let value = DentryCacheValue::Positive {
            node,
            kind,
            parent_generation: self.parent_generation(parent),
            lru_stamp: 0,
        };
        if let Some(index) = self.find_entry_index(bucket, component) {
            let old_stamp = self
                .entries
                .get(&bucket)
                .and_then(|entries| entries.get(index))
                .map(|entry| entry.value.lru_stamp());
            let stamp = self.touch(bucket, old_stamp);
            if let Some(entry) = self
                .entries
                .get_mut(&bucket)
                .and_then(|entries| entries.get_mut(index))
            {
                entry.value = value.with_lru_stamp(stamp);
            }
        } else {
            self.record_key_alloc();
            let stamp = self.touch(bucket, None);
            self.entries
                .entry(bucket)
                .or_default()
                .push(DentryCacheEntry {
                    component: String::from(component),
                    value: value.with_lru_stamp(stamp),
                });
            self.entry_count += 1;
        }
        record_dentry_stat! {
            self.stats.insert_positive += 1;
        }
        self.trim_to_capacity();
    }

    fn insert_negative(
        &mut self,
        bucket: DentryCacheBucketKey,
        parent: VfsNodeId,
        component: &str,
    ) {
        if !self.enabled || self.capacity == 0 {
            return;
        }
        let value = DentryCacheValue::Negative {
            parent_generation: self.parent_generation(parent),
            lru_stamp: 0,
        };
        if let Some(index) = self.find_entry_index(bucket, component) {
            let old_stamp = self
                .entries
                .get(&bucket)
                .and_then(|entries| entries.get(index))
                .map(|entry| entry.value.lru_stamp());
            let stamp = self.touch(bucket, old_stamp);
            if let Some(entry) = self
                .entries
                .get_mut(&bucket)
                .and_then(|entries| entries.get_mut(index))
            {
                entry.value = value.with_lru_stamp(stamp);
            }
        } else {
            self.record_key_alloc();
            let stamp = self.touch(bucket, None);
            self.entries
                .entry(bucket)
                .or_default()
                .push(DentryCacheEntry {
                    component: String::from(component),
                    value: value.with_lru_stamp(stamp),
                });
            self.entry_count += 1;
        }
        record_dentry_stat! {
            self.stats.insert_negative += 1;
        }
        self.trim_to_capacity();
    }

    fn invalidate_parent(&mut self, parent: VfsNodeId) {
        if !self.enabled {
            return;
        }
        let generation = self.parent_generation(parent).wrapping_add(1);
        self.parent_generations.insert(parent, generation);
        record_dentry_stat! {
            self.stats.invalidate_parent_calls += 1;
        }
    }

    fn clear_all(&mut self) {
        if !self.enabled || self.entry_count == 0 {
            return;
        }
        record_dentry_stat! {
            self.stats.invalidate_all += self.entry_count;
        }
        self.entries.clear();
        self.entry_count = 0;
        self.lru.clear();
    }

    fn stats_snapshot(&self) -> DentryCacheStats {
        DentryCacheStats {
            enabled: self.enabled,
            entries: self.entry_count,
            capacity: self.capacity,
            ..self.stats
        }
    }
}

lazy_static! {
    static ref DENTRY_CACHE: UPIntrFreeCell<DentryCache> =
        unsafe { UPIntrFreeCell::new(DentryCache::new(DEFAULT_DENTRY_CACHE_CAPACITY)) };
}

pub(crate) fn lookup(
    namespace_id: MountNamespaceId,
    parent: VfsNodeId,
    component: &str,
) -> Option<DentryLookupResult> {
    let bucket = DentryCacheBucketKey::new(namespace_id, parent, component);
    DENTRY_CACHE
        .exclusive_access()
        .lookup(bucket, parent, component)
}

pub(crate) fn insert_positive(
    namespace_id: MountNamespaceId,
    parent: VfsNodeId,
    component: &str,
    node: VfsNodeId,
    kind: FsNodeKind,
) {
    let bucket = DentryCacheBucketKey::new(namespace_id, parent, component);
    DENTRY_CACHE
        .exclusive_access()
        .insert_positive(bucket, parent, component, node, kind);
}

pub(crate) fn insert_negative(namespace_id: MountNamespaceId, parent: VfsNodeId, component: &str) {
    let bucket = DentryCacheBucketKey::new(namespace_id, parent, component);
    DENTRY_CACHE
        .exclusive_access()
        .insert_negative(bucket, parent, component);
}

pub(crate) fn invalidate_parent(parent: VfsNodeId) {
    DENTRY_CACHE.exclusive_access().invalidate_parent(parent);
}

pub(crate) fn clear_all() {
    DENTRY_CACHE.exclusive_access().clear_all();
}

pub(crate) fn stats_snapshot() -> DentryCacheStats {
    DENTRY_CACHE.exclusive_access().stats_snapshot()
}

pub(crate) fn stats_content() -> String {
    let stats = stats_snapshot();
    format!(
        "enabled {}\nentries {}\ncapacity {}\npositive_hit {}\nnegative_hit {}\nmiss {}\nrevalidate_fail {}\ninsert_positive {}\ninsert_negative {}\ninvalidate_parent {}\ninvalidate_parent_calls {}\ninvalidate_parent_entry_scans {}\ninvalidate_parent_lru_scans {}\ninvalidate_all {}\nevict {}\nlru_touch {}\nlru_scan_slots {}\n",
        stats.enabled as usize,
        stats.entries,
        stats.capacity,
        stats.positive_hit,
        stats.negative_hit,
        stats.miss,
        stats.revalidate_fail,
        stats.insert_positive,
        stats.insert_negative,
        stats.invalidate_parent,
        stats.invalidate_parent_calls,
        stats.invalidate_parent_entry_scans,
        stats.invalidate_parent_lru_scans,
        stats.invalidate_all,
        stats.evict,
        stats.lru_touch,
        stats.lru_scan_slots,
    )
}
