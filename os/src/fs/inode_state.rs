use super::{FileStat, vfs::VfsNodeId};
use crate::sync::{SleepRwLock, SleepRwLockWriteGuard, UPIntrFreeCell};
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;

const INODE_STATE_SHARDS: usize = 32;

struct InodeStateShard {
    states: BTreeMap<VfsNodeId, Arc<InodeState>>,
    next_incarnations: BTreeMap<VfsNodeId, usize>,
}

impl InodeStateShard {
    fn new() -> Self {
        Self {
            states: BTreeMap::new(),
            next_incarnations: BTreeMap::new(),
        }
    }
}

lazy_static! {
    static ref SHARDS: Vec<UPIntrFreeCell<InodeStateShard>> = (0..INODE_STATE_SHARDS)
        .map(|_| unsafe { UPIntrFreeCell::new(InodeStateShard::new()) })
        .collect();
}

pub(crate) struct InodeState {
    node: VfsNodeId,
    incarnation: usize,
    metadata: SleepRwLock<Option<FileStat>>,
    metadata_version: VersionDomain,
    directory_version: VersionDomain,
    mapping_version: VersionDomain,
}

struct VersionDomain {
    version: AtomicUsize,
    writer: SleepRwLock<()>,
}

pub(crate) struct VersionMutationGuard<'a> {
    domain: &'a VersionDomain,
    _writer: SleepRwLockWriteGuard<'a, ()>,
    start_version: usize,
    committed: bool,
}

impl VersionDomain {
    fn new() -> Self {
        Self {
            version: AtomicUsize::new(0),
            writer: SleepRwLock::new(()),
        }
    }

    fn stable_version(&self) -> Option<usize> {
        let version = self.version.load(Ordering::Acquire);
        (version & 1 == 0).then_some(version)
    }

    fn begin_mutation(&self) -> VersionMutationGuard<'_> {
        let writer = self.writer.write();
        let start_version = self.version.load(Ordering::Acquire);
        assert_eq!(start_version & 1, 0, "nested inode version mutation");
        let unstable_version = start_version
            .checked_add(1)
            .expect("inode version domain exhausted");
        self.version.store(unstable_version, Ordering::Release);
        VersionMutationGuard {
            domain: self,
            _writer: writer,
            start_version,
            committed: false,
        }
    }
}

impl VersionMutationGuard<'_> {
    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for VersionMutationGuard<'_> {
    fn drop(&mut self) {
        let current = self.domain.version.load(Ordering::Acquire);
        assert_eq!(current & 1, 1, "inode mutation ended from stable version");
        let next = if self.committed {
            self.start_version
                .checked_add(2)
                .expect("inode version domain exhausted")
        } else {
            self.start_version
        };
        self.domain.version.store(next, Ordering::Release);
    }
}

impl InodeState {
    fn new(node: VfsNodeId, incarnation: usize) -> Self {
        Self {
            node,
            incarnation,
            metadata: SleepRwLock::new(None),
            metadata_version: VersionDomain::new(),
            directory_version: VersionDomain::new(),
            mapping_version: VersionDomain::new(),
        }
    }

    pub(crate) fn node(&self) -> VfsNodeId {
        self.node
    }

    pub(crate) fn incarnation(&self) -> usize {
        self.incarnation
    }

    #[allow(dead_code)]
    pub(crate) fn metadata_version(&self) -> Option<usize> {
        self.metadata_version.stable_version()
    }

    #[allow(dead_code)]
    pub(crate) fn directory_version(&self) -> Option<usize> {
        self.directory_version.stable_version()
    }

    #[allow(dead_code)]
    pub(crate) fn mapping_version(&self) -> Option<usize> {
        self.mapping_version.stable_version()
    }

    fn begin_metadata_mutation(&self) -> VersionMutationGuard<'_> {
        self.metadata_version.begin_mutation()
    }

    fn begin_directory_mutation(&self) -> VersionMutationGuard<'_> {
        self.directory_version.begin_mutation()
    }

    fn begin_mapping_mutation(&self) -> VersionMutationGuard<'_> {
        self.mapping_version.begin_mutation()
    }

    #[allow(dead_code)]
    pub(crate) fn cached_metadata(&self) -> Option<FileStat> {
        *self.metadata.read()
    }

    #[allow(dead_code)]
    pub(crate) fn set_cached_metadata(&self, metadata: Option<FileStat>) {
        *self.metadata.write() = metadata;
    }
}

fn shard_index(node: VfsNodeId) -> usize {
    (node.mount_id.0.wrapping_mul(0x9e37_79b1) ^ node.ino as usize) % INODE_STATE_SHARDS
}

pub(crate) fn state_for(node: VfsNodeId) -> Arc<InodeState> {
    let mut shard = SHARDS[shard_index(node)].exclusive_access();
    if let Some(state) = shard.states.get(&node) {
        return Arc::clone(state);
    }
    let incarnation = shard.next_incarnations.get(&node).copied().unwrap_or(1);
    let state = Arc::new(InodeState::new(node, incarnation));
    shard.states.insert(node, Arc::clone(&state));
    state
}

/// Starts a new incarnation after a successful create transaction. Replacing
/// an old entry is safe only here: allocation has already established that the
/// inode number now denotes a new object.
pub(crate) fn initialize_new(node: VfsNodeId) -> Arc<InodeState> {
    let mut shard = SHARDS[shard_index(node)].exclusive_access();
    let incarnation = shard
        .states
        .remove(&node)
        .map(|state| {
            state
                .incarnation()
                .checked_add(1)
                .expect("inode incarnation exhausted")
        })
        .unwrap_or_else(|| shard.next_incarnations.get(&node).copied().unwrap_or(1));
    let state = Arc::new(InodeState::new(node, incarnation));
    shard.next_incarnations.insert(node, incarnation);
    shard.states.insert(node, Arc::clone(&state));
    state
}

pub(crate) fn with_metadata_mutation<V>(
    node: VfsNodeId,
    mutation: impl FnOnce() -> super::vfs::FsResult<V>,
) -> super::vfs::FsResult<V> {
    let state = state_for(node);
    let mut guard = state.begin_metadata_mutation();
    let result = mutation();
    if result.is_ok() {
        state.set_cached_metadata(None);
        guard.commit();
    }
    result
}

pub(crate) fn with_mapping_mutation<V>(
    node: VfsNodeId,
    mutation: impl FnOnce() -> super::vfs::FsResult<V>,
) -> super::vfs::FsResult<V> {
    let state = state_for(node);
    let mut guard = state.begin_mapping_mutation();
    let result = mutation();
    if result.is_ok() {
        state.set_cached_metadata(None);
        guard.commit();
    }
    result
}

/// Variant for write paths whose backend compatibility API reports a byte
/// count instead of `FsResult`. A non-empty write attempt conservatively bumps
/// the mapping epoch because it may allocate, extend, or convert extents.
pub(crate) fn with_mapping_mutation_value<V>(node: VfsNodeId, mutation: impl FnOnce() -> V) -> V {
    let state = state_for(node);
    let mut guard = state.begin_mapping_mutation();
    let result = mutation();
    state.set_cached_metadata(None);
    guard.commit();
    result
}

pub(crate) fn with_directory_mutation<V>(
    parent: VfsNodeId,
    mutation: impl FnOnce() -> super::vfs::FsResult<V>,
) -> super::vfs::FsResult<V> {
    let state = state_for(parent);
    let mut guard = state.begin_directory_mutation();
    let result = mutation();
    if result.is_ok() {
        state.set_cached_metadata(None);
        guard.commit();
    }
    result
}

/// Serializes a cross-directory namespace transaction in stable node order.
/// Equal parents take only one writer guard.
pub(crate) fn with_directory_mutations<V>(
    first: VfsNodeId,
    second: VfsNodeId,
    mutation: impl FnOnce() -> super::vfs::FsResult<V>,
) -> super::vfs::FsResult<V> {
    if first == second {
        return with_directory_mutation(first, mutation);
    }
    let (low, high) = if first < second {
        (first, second)
    } else {
        (second, first)
    };
    let low_state = state_for(low);
    let high_state = state_for(high);
    let mut low_guard = low_state.begin_directory_mutation();
    let mut high_guard = high_state.begin_directory_mutation();
    let result = mutation();
    if result.is_ok() {
        low_state.set_cached_metadata(None);
        high_state.set_cached_metadata(None);
        low_guard.commit();
        high_guard.commit();
    }
    result
}

/// Removes only the exact incarnation held by a final-release record. A stale
/// Arc can therefore never erase state installed for a reused inode number.
pub(crate) fn remove_if_same(node: VfsNodeId, expected: &Arc<InodeState>) -> bool {
    let mut shard = SHARDS[shard_index(node)].exclusive_access();
    let matches = shard
        .states
        .get(&node)
        .is_some_and(|current| Arc::ptr_eq(current, expected));
    if !matches {
        return false;
    }
    shard.states.remove(&node);
    let next = expected
        .incarnation()
        .checked_add(1)
        .expect("inode incarnation exhausted");
    shard.next_incarnations.insert(node, next);
    true
}

/// Checks whether an owner captured by an open or dirty-cache pin still names
/// the table's current inode incarnation.
pub(crate) fn is_current(expected: &Arc<InodeState>) -> bool {
    let node = expected.node();
    SHARDS[shard_index(node)]
        .exclusive_access()
        .states
        .get(&node)
        .is_some_and(|current| Arc::ptr_eq(current, expected))
}
