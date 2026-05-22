use super::mount::MountNamespaceId;
use super::vfs::{FsNodeKind, VfsNodeId};
use crate::sync::UPIntrFreeCell;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use lazy_static::*;

const DEFAULT_DENTRY_CACHE_CAPACITY: usize = 4096;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DentryCacheKey {
    namespace_id: MountNamespaceId,
    parent: VfsNodeId,
    component: String,
}

impl DentryCacheKey {
    fn new(namespace_id: MountNamespaceId, parent: VfsNodeId, component: &str) -> Self {
        Self {
            namespace_id,
            parent,
            component: component.to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum DentryCacheValue {
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

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DentryCacheLruEntry {
    stamp: usize,
    key: DentryCacheKey,
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
    pub(crate) invalidate_all: usize,
    pub(crate) evict: usize,
    pub(crate) lru_touch: usize,
    pub(crate) lru_scan_slots: usize,
}

struct DentryCache {
    enabled: bool,
    capacity: usize,
    entries: BTreeMap<DentryCacheKey, DentryCacheValue>,
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

    fn touch(&mut self, key: DentryCacheKey, old_stamp: Option<usize>) -> usize {
        self.stats.lru_touch += 1;
        if let Some(stamp) = old_stamp {
            self.lru.remove(&DentryCacheLruEntry {
                stamp,
                key: key.clone(),
            });
        }
        self.lru_clock = self.lru_clock.wrapping_add(1);
        let stamp = self.lru_clock;
        self.lru.insert(DentryCacheLruEntry { stamp, key });
        stamp
    }

    fn trim_to_capacity(&mut self) {
        while self.entries.len() > self.capacity {
            let Some(victim) = self.lru.iter().next().cloned() else {
                break;
            };
            self.lru.remove(&victim);
            if self.entries.remove(&victim.key).is_some() {
                self.stats.evict += 1;
            }
        }
    }

    fn lookup(
        &mut self,
        namespace_id: MountNamespaceId,
        parent: VfsNodeId,
        component: &str,
    ) -> Option<DentryLookupResult> {
        if !self.enabled {
            return None;
        }
        let key = DentryCacheKey::new(namespace_id, parent, component);
        let Some(value) = self.entries.get(&key).copied() else {
            self.stats.miss += 1;
            return None;
        };
        if value.parent_generation() != self.parent_generation(parent) {
            self.entries.remove(&key);
            self.lru.remove(&DentryCacheLruEntry {
                stamp: value.lru_stamp(),
                key,
            });
            self.stats.revalidate_fail += 1;
            return None;
        }
        let stamp = self.touch(key.clone(), Some(value.lru_stamp()));
        self.entries.insert(key, value.with_lru_stamp(stamp));
        match value {
            DentryCacheValue::Positive { node, kind, .. } => {
                self.stats.positive_hit += 1;
                Some(DentryLookupResult::Positive { node, kind })
            }
            DentryCacheValue::Negative { .. } => {
                self.stats.negative_hit += 1;
                Some(DentryLookupResult::Negative)
            }
        }
    }

    fn insert_positive(
        &mut self,
        namespace_id: MountNamespaceId,
        parent: VfsNodeId,
        component: &str,
        node: VfsNodeId,
        kind: FsNodeKind,
    ) {
        if !self.enabled || self.capacity == 0 {
            return;
        }
        let key = DentryCacheKey::new(namespace_id, parent, component);
        let value = DentryCacheValue::Positive {
            node,
            kind,
            parent_generation: self.parent_generation(parent),
            lru_stamp: 0,
        };
        let old_stamp = self.entries.get(&key).map(|value| value.lru_stamp());
        let stamp = self.touch(key.clone(), old_stamp);
        self.entries.insert(key, value.with_lru_stamp(stamp));
        self.stats.insert_positive += 1;
        self.trim_to_capacity();
    }

    fn insert_negative(
        &mut self,
        namespace_id: MountNamespaceId,
        parent: VfsNodeId,
        component: &str,
    ) {
        if !self.enabled || self.capacity == 0 {
            return;
        }
        let key = DentryCacheKey::new(namespace_id, parent, component);
        let value = DentryCacheValue::Negative {
            parent_generation: self.parent_generation(parent),
            lru_stamp: 0,
        };
        let old_stamp = self.entries.get(&key).map(|value| value.lru_stamp());
        let stamp = self.touch(key.clone(), old_stamp);
        self.entries.insert(key, value.with_lru_stamp(stamp));
        self.stats.insert_negative += 1;
        self.trim_to_capacity();
    }

    fn invalidate_parent(&mut self, parent: VfsNodeId) {
        if !self.enabled {
            return;
        }
        let generation = self.parent_generation(parent).wrapping_add(1);
        self.parent_generations.insert(parent, generation);
        let before = self.entries.len();
        self.entries.retain(|key, _| key.parent != parent);
        self.lru.retain(|entry| entry.key.parent != parent);
        self.stats.invalidate_parent += before.saturating_sub(self.entries.len());
    }

    fn clear_all(&mut self) {
        if !self.enabled || self.entries.is_empty() {
            return;
        }
        self.stats.invalidate_all += self.entries.len();
        self.entries.clear();
        self.lru.clear();
    }

    fn stats_snapshot(&self) -> DentryCacheStats {
        DentryCacheStats {
            enabled: self.enabled,
            entries: self.entries.len(),
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
    DENTRY_CACHE
        .exclusive_access()
        .lookup(namespace_id, parent, component)
}

pub(crate) fn insert_positive(
    namespace_id: MountNamespaceId,
    parent: VfsNodeId,
    component: &str,
    node: VfsNodeId,
    kind: FsNodeKind,
) {
    DENTRY_CACHE
        .exclusive_access()
        .insert_positive(namespace_id, parent, component, node, kind);
}

pub(crate) fn insert_negative(namespace_id: MountNamespaceId, parent: VfsNodeId, component: &str) {
    DENTRY_CACHE
        .exclusive_access()
        .insert_negative(namespace_id, parent, component);
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
        "enabled {}\nentries {}\ncapacity {}\npositive_hit {}\nnegative_hit {}\nmiss {}\nrevalidate_fail {}\ninsert_positive {}\ninsert_negative {}\ninvalidate_parent {}\ninvalidate_all {}\nevict {}\nlru_touch {}\nlru_scan_slots {}\n",
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
        stats.invalidate_all,
        stats.evict,
        stats.lru_touch,
        stats.lru_scan_slots,
    )
}
