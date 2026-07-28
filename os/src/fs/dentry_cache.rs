use super::inode_state;
use super::mount::MountNamespaceId;
use super::vfs::{FsNodeKind, VfsNodeId};
#[cfg(feature = "perf-counters")]
use crate::perf::PerCpuCounter;
use crate::sync::{SleepMutex, SpinRwLock};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::*;

const DEFAULT_DENTRY_CACHE_CAPACITY: usize = 4096;
const DENTRY_CACHE_SHARD_COUNT: usize = 32;
const DENTRY_LOOKUP_FLIGHT_COUNT: usize = 64;
const DENTRY_CACHE_SHARD_CAPACITY: usize = DEFAULT_DENTRY_CACHE_CAPACITY / DENTRY_CACHE_SHARD_COUNT;
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

struct DentryCacheEntry {
    component: String,
    parent_state: Arc<inode_state::InodeState>,
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
        mount_epoch: usize,
        lru_stamp: usize,
    },
    Negative {
        parent_generation: usize,
        mount_epoch: usize,
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

    fn mount_epoch(self) -> usize {
        match self {
            Self::Positive { mount_epoch, .. } | Self::Negative { mount_epoch, .. } => mount_epoch,
        }
    }

    fn with_lru_stamp(self, lru_stamp: usize) -> Self {
        match self {
            Self::Positive {
                node,
                kind,
                parent_generation,
                mount_epoch,
                ..
            } => Self::Positive {
                node,
                kind,
                parent_generation,
                mount_epoch,
                lru_stamp,
            },
            Self::Negative {
                parent_generation,
                mount_epoch,
                ..
            } => Self::Negative {
                parent_generation,
                mount_epoch,
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

#[derive(Clone)]
pub(crate) struct DentryVersionToken {
    parent_state: Arc<inode_state::InodeState>,
    parent_version: usize,
    mount_epoch: usize,
}

impl DentryVersionToken {
    fn is_current(&self) -> bool {
        self.parent_state.is_alive()
            && self.parent_state.directory_version() == Some(self.parent_version)
            && DENTRY_MOUNT_EPOCH.load(Ordering::Acquire) == self.mount_epoch
    }
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

#[repr(align(64))]
struct DentryCache {
    enabled: bool,
    capacity: usize,
    entries: BTreeMap<DentryCacheBucketKey, Vec<DentryCacheEntry>>,
    entry_count: usize,
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
            lru: BTreeSet::new(),
            lru_clock: 0,
            stats: DentryCacheStats {
                enabled: true,
                capacity,
                ..DentryCacheStats::default()
            },
        }
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
        &self,
        bucket: DentryCacheBucketKey,
        mount_epoch: usize,
        component: &str,
    ) -> (Option<DentryLookupResult>, bool, usize) {
        if !self.enabled {
            return (None, false, 0);
        }
        let Some(entries) = self.entries.get(&bucket) else {
            return (None, false, 0);
        };
        let mut collision_scans = 0;
        let mut found = None;
        for (index, entry) in entries.iter().enumerate() {
            if index > 0 {
                collision_scans += 1;
            }
            if entry.component == component {
                found = Some(entry);
                break;
            }
        }
        let Some(entry) = found else {
            return (None, false, collision_scans);
        };
        let value = entry.value;
        if !entry.parent_state.is_alive()
            || value.parent_generation()
                != entry.parent_state.directory_version().unwrap_or(usize::MAX)
            || value.mount_epoch() != mount_epoch
        {
            return (None, true, collision_scans);
        }
        let result = match value {
            DentryCacheValue::Positive { node, kind, .. } => {
                Some(DentryLookupResult::Positive { node, kind })
            }
            DentryCacheValue::Negative { .. } => Some(DentryLookupResult::Negative),
        };
        (result, false, collision_scans)
    }

    fn insert_positive(
        &mut self,
        bucket: DentryCacheBucketKey,
        parent_state: Arc<inode_state::InodeState>,
        parent_generation: usize,
        mount_epoch: usize,
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
            parent_generation,
            mount_epoch,
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
                entry.parent_state = parent_state;
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
                    parent_state,
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
        parent_state: Arc<inode_state::InodeState>,
        parent_generation: usize,
        mount_epoch: usize,
        component: &str,
    ) {
        if !self.enabled || self.capacity == 0 {
            return;
        }
        let value = DentryCacheValue::Negative {
            parent_generation,
            mount_epoch,
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
                entry.parent_state = parent_state;
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
                    parent_state,
                    value: value.with_lru_stamp(stamp),
                });
            self.entry_count += 1;
        }
        record_dentry_stat! {
            self.stats.insert_negative += 1;
        }
        self.trim_to_capacity();
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

static DENTRY_MOUNT_EPOCH: AtomicUsize = AtomicUsize::new(1);
static DENTRY_INVALIDATE_PARENT_CALLS: AtomicUsize = AtomicUsize::new(0);
static DENTRY_INVALIDATE_ALL_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "perf-counters")]
static DENTRY_POSITIVE_HITS: PerCpuCounter = PerCpuCounter::new();
#[cfg(feature = "perf-counters")]
static DENTRY_NEGATIVE_HITS: PerCpuCounter = PerCpuCounter::new();
#[cfg(feature = "perf-counters")]
static DENTRY_MISSES: PerCpuCounter = PerCpuCounter::new();
#[cfg(feature = "perf-counters")]
static DENTRY_REVALIDATE_FAILURES: PerCpuCounter = PerCpuCounter::new();
#[cfg(feature = "perf-counters")]
static DENTRY_COLLISION_SCANS: PerCpuCounter = PerCpuCounter::new();

lazy_static! {
    static ref DENTRY_CACHE_SHARDS: Vec<SpinRwLock<DentryCache>> = (0
        ..DENTRY_CACHE_SHARD_COUNT)
        .map(|_| SpinRwLock::new(DentryCache::new(DENTRY_CACHE_SHARD_CAPACITY)))
        .collect();
    // Same-key misses always map to the same sleeping gate. Hash collisions
    // may conservatively serialize unrelated misses, but never duplicate one
    // key's backend lookup.
    static ref DENTRY_LOOKUP_FLIGHTS: Vec<SleepMutex<()>> = (0
        ..DENTRY_LOOKUP_FLIGHT_COUNT)
        .map(|_| SleepMutex::new(()))
        .collect();
}

#[cfg(feature = "perf-counters")]
fn record_lookup_result(
    result: Option<DentryLookupResult>,
    revalidate_failed: bool,
    collision_scans: usize,
) {
    DENTRY_COLLISION_SCANS.fetch_add(collision_scans, Ordering::Relaxed);
    match result {
        Some(DentryLookupResult::Positive { .. }) => {
            DENTRY_POSITIVE_HITS.fetch_add(1, Ordering::Relaxed);
        }
        Some(DentryLookupResult::Negative) => {
            DENTRY_NEGATIVE_HITS.fetch_add(1, Ordering::Relaxed);
        }
        None if revalidate_failed => {
            DENTRY_REVALIDATE_FAILURES.fetch_add(1, Ordering::Relaxed);
        }
        None => {
            DENTRY_MISSES.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_lookup_result(
    _result: Option<DentryLookupResult>,
    _revalidate_failed: bool,
    _collision_scans: usize,
) {
}

fn shard_index(bucket: DentryCacheBucketKey, count: usize) -> usize {
    debug_assert!(count.is_power_of_two());
    let mixed = bucket.component_hash
        ^ (bucket.parent.ino as u64).rotate_left(17)
        ^ (bucket.parent.mount_id.0 as u64).rotate_left(31)
        ^ (bucket.namespace_id.0 as u64).rotate_left(47);
    mixed as usize & (count - 1)
}

pub(crate) fn version_token(parent: VfsNodeId) -> Option<DentryVersionToken> {
    let parent_state = inode_state::state_for(parent);
    Some(DentryVersionToken {
        parent_version: parent_state.directory_version()?,
        parent_state,
        mount_epoch: DENTRY_MOUNT_EPOCH.load(Ordering::Acquire),
    })
}

pub(crate) fn with_lookup_single_flight<V>(
    namespace_id: MountNamespaceId,
    parent: VfsNodeId,
    component: &str,
    operation: impl FnOnce() -> V,
) -> V {
    let bucket = DentryCacheBucketKey::new(namespace_id, parent, component);
    let _guard = DENTRY_LOOKUP_FLIGHTS[shard_index(bucket, DENTRY_LOOKUP_FLIGHT_COUNT)].lock();
    operation()
}

pub(crate) fn lookup(
    namespace_id: MountNamespaceId,
    parent: VfsNodeId,
    component: &str,
) -> Option<DentryLookupResult> {
    let bucket = DentryCacheBucketKey::new(namespace_id, parent, component);
    let mount_epoch = DENTRY_MOUNT_EPOCH.load(Ordering::Acquire);
    let (result, revalidate_failed, collision_scans) = DENTRY_CACHE_SHARDS
        [shard_index(bucket, DENTRY_CACHE_SHARD_COUNT)]
    .read()
    .lookup(bucket, mount_epoch, component);
    record_lookup_result(result, revalidate_failed, collision_scans);
    if DENTRY_MOUNT_EPOCH.load(Ordering::Acquire) != mount_epoch {
        return None;
    }
    result
}

pub(crate) fn insert_positive(
    namespace_id: MountNamespaceId,
    parent: VfsNodeId,
    expected: DentryVersionToken,
    component: &str,
    node: VfsNodeId,
    kind: FsNodeKind,
) {
    if !expected.is_current() {
        return;
    }
    let bucket = DentryCacheBucketKey::new(namespace_id, parent, component);
    DENTRY_CACHE_SHARDS[shard_index(bucket, DENTRY_CACHE_SHARD_COUNT)]
        .write()
        .insert_positive(
            bucket,
            Arc::clone(&expected.parent_state),
            expected.parent_version,
            expected.mount_epoch,
            component,
            node,
            kind,
        );
}

pub(crate) fn insert_negative(
    namespace_id: MountNamespaceId,
    parent: VfsNodeId,
    expected: DentryVersionToken,
    component: &str,
) {
    if !expected.is_current() {
        return;
    }
    let bucket = DentryCacheBucketKey::new(namespace_id, parent, component);
    DENTRY_CACHE_SHARDS[shard_index(bucket, DENTRY_CACHE_SHARD_COUNT)]
        .write()
        .insert_negative(
            bucket,
            Arc::clone(&expected.parent_state),
            expected.parent_version,
            expected.mount_epoch,
            component,
        );
}

pub(crate) fn invalidate_parent(parent: VfsNodeId) {
    inode_state::invalidate_directory(parent);
    DENTRY_INVALIDATE_PARENT_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn clear_all() {
    inode_state::invalidate_direct_stat_cache();
    DENTRY_MOUNT_EPOCH
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
            epoch.checked_add(1)
        })
        .expect("dentry mount epoch exhausted");
    DENTRY_INVALIDATE_ALL_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn stats_snapshot() -> DentryCacheStats {
    let mut total = DentryCacheStats {
        enabled: true,
        capacity: DEFAULT_DENTRY_CACHE_CAPACITY,
        ..DentryCacheStats::default()
    };
    for shard in DENTRY_CACHE_SHARDS.iter() {
        let stats = shard.read().stats_snapshot();
        total.entries += stats.entries;
        total.positive_hit += stats.positive_hit;
        total.negative_hit += stats.negative_hit;
        total.miss += stats.miss;
        total.revalidate_fail += stats.revalidate_fail;
        total.insert_positive += stats.insert_positive;
        total.insert_negative += stats.insert_negative;
        total.evict += stats.evict;
        total.lru_touch += stats.lru_touch;
        total.lru_scan_slots += stats.lru_scan_slots;
        #[cfg(feature = "perf-counters")]
        {
            total.key_allocs += stats.key_allocs;
            total.collision_scans += stats.collision_scans;
        }
    }
    let parent_invalidations = DENTRY_INVALIDATE_PARENT_CALLS.load(Ordering::Relaxed);
    total.invalidate_parent = parent_invalidations;
    total.invalidate_parent_calls = parent_invalidations;
    total.invalidate_all = DENTRY_INVALIDATE_ALL_CALLS.load(Ordering::Relaxed);
    #[cfg(feature = "perf-counters")]
    {
        total.positive_hit += DENTRY_POSITIVE_HITS.load(Ordering::Relaxed);
        total.negative_hit += DENTRY_NEGATIVE_HITS.load(Ordering::Relaxed);
        total.miss += DENTRY_MISSES.load(Ordering::Relaxed);
        total.revalidate_fail += DENTRY_REVALIDATE_FAILURES.load(Ordering::Relaxed);
        total.collision_scans += DENTRY_COLLISION_SCANS.load(Ordering::Relaxed);
    }
    total
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
