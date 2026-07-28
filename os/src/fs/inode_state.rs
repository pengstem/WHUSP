use super::{FileStat, MountNamespaceId, WorkingDir, vfs::VfsNodeId};
use crate::config::MAX_CPUS;
use crate::sync::{SleepRwLock, SleepRwLockReadGuard, SleepRwLockWriteGuard, SpinRwLock};
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use lazy_static::lazy_static;

const INODE_STATE_SHARDS: usize = 32;
const DIRECT_STAT_CACHE_SLOTS: usize = 16;
const DIRECT_STAT_NAME_MAX: usize = 255;

static DIRECT_STAT_CACHE_EPOCH: AtomicUsize = AtomicUsize::new(1);

#[derive(Clone, Copy)]
struct DirectStatCacheEntry {
    epoch: usize,
    namespace_id: MountNamespaceId,
    parent: WorkingDir,
    name_len: usize,
    name: [u8; DIRECT_STAT_NAME_MAX],
    stat: FileStat,
}

struct DirectStatCacheData {
    entries: [Option<DirectStatCacheEntry>; DIRECT_STAT_CACHE_SLOTS],
}

#[repr(align(64))]
struct DirectStatCpuCache {
    data: UnsafeCell<DirectStatCacheData>,
}

unsafe impl Sync for DirectStatCpuCache {}

impl DirectStatCpuCache {
    fn new() -> Self {
        Self {
            data: UnsafeCell::new(DirectStatCacheData {
                entries: [None; DIRECT_STAT_CACHE_SLOTS],
            }),
        }
    }
}

#[repr(align(64))]
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
    // These shards are shared by tasks running on different CPUs. Merely
    // masking local interrupts (UPIntrFreeCell) does not serialize SMP access
    // and allowed concurrent BTreeMap mutation to manufacture multiple states
    // for one inode. Keep the table critical section short and SMP-safe; all
    // sleeping per-inode work remains below the Arc returned from the shard.
    static ref SHARDS: Vec<SpinRwLock<InodeStateShard>> = (0..INODE_STATE_SHARDS)
        .map(|_| SpinRwLock::new(InodeStateShard::new()))
        .collect();
    static ref DIRECT_STAT_CACHES: Vec<DirectStatCpuCache> =
        (0..MAX_CPUS).map(|_| DirectStatCpuCache::new()).collect();
}

fn direct_stat_slot(parent: WorkingDir, name: &str) -> usize {
    let mut hash = (parent.mount_id().0 as u64).rotate_left(17) ^ parent.ino() as u64;
    for byte in name.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash as usize & (DIRECT_STAT_CACHE_SLOTS - 1)
}

pub(crate) fn direct_stat_cache_lookup(
    namespace_id: MountNamespaceId,
    parent: WorkingDir,
    name: &str,
) -> Option<FileStat> {
    if name.len() > DIRECT_STAT_NAME_MAX {
        return None;
    }
    let epoch = DIRECT_STAT_CACHE_EPOCH.load(Ordering::Acquire);
    let slot = direct_stat_slot(parent, name);
    // Kernel-mode timer/IPI traps do not schedule or migrate the interrupted
    // syscall, so the current CPU owns this cache for the whole access. Avoid
    // toggling the interrupt CSR on every metadata hit, especially on LA.
    let cache = unsafe { &*DIRECT_STAT_CACHES[crate::cpu::current_id()].data.get() };
    let entry = cache.entries[slot]?;
    if DIRECT_STAT_CACHE_EPOCH.load(Ordering::Acquire) != epoch
        || entry.epoch != epoch
        || entry.namespace_id != namespace_id
        || entry.parent != parent
        || entry.name_len != name.len()
        || entry.name[..entry.name_len] != name.as_bytes()[..]
    {
        return None;
    }
    Some(entry.stat)
}

pub(crate) fn direct_stat_cache_insert(
    expected_epoch: usize,
    namespace_id: MountNamespaceId,
    parent: WorkingDir,
    name: &str,
    stat: FileStat,
) {
    if name.len() > DIRECT_STAT_NAME_MAX {
        return;
    }
    let mut entry = DirectStatCacheEntry {
        epoch: expected_epoch,
        namespace_id,
        parent,
        name_len: name.len(),
        name: [0; DIRECT_STAT_NAME_MAX],
        stat,
    };
    entry.name[..name.len()].copy_from_slice(name.as_bytes());
    let slot = direct_stat_slot(parent, name);
    // See lookup above: non-preemptible kernel execution keeps this write on
    // the selected CPU without masking interrupts.
    if DIRECT_STAT_CACHE_EPOCH.load(Ordering::Acquire) != expected_epoch {
        return;
    }
    let cache = unsafe { &mut *DIRECT_STAT_CACHES[crate::cpu::current_id()].data.get() };
    cache.entries[slot] = Some(entry);
}

pub(crate) fn direct_stat_cache_epoch() -> usize {
    DIRECT_STAT_CACHE_EPOCH.load(Ordering::Acquire)
}

pub(crate) fn invalidate_direct_stat_cache() {
    DIRECT_STAT_CACHE_EPOCH.fetch_add(1, Ordering::AcqRel);
}

#[repr(align(64))]
pub(crate) struct InodeState {
    node: VfsNodeId,
    incarnation: usize,
    alive: AtomicBool,
    metadata: SleepRwLock<MetadataCache>,
    metadata_version: VersionDomain,
    directory_version: VersionDomain,
    mapping_version: VersionDomain,
}

#[derive(Clone, Copy)]
struct CachedMetadata {
    version: usize,
    stat: FileStat,
}

#[derive(Default)]
struct MetadataCache {
    basic: Option<CachedMetadata>,
    full: Option<CachedMetadata>,
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

    fn read(&self) -> SleepRwLockReadGuard<'_, ()> {
        self.writer.read()
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
            alive: AtomicBool::new(true),
            metadata: SleepRwLock::new(MetadataCache::default()),
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

    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn retire(&self) {
        self.alive.store(false, Ordering::Release);
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

    fn cached_metadata(&self, full: bool, version: usize) -> Option<FileStat> {
        let metadata = self.metadata.read();
        let cached = if full { metadata.full } else { metadata.basic };
        cached
            .filter(|cached| cached.version == version)
            .map(|cached| cached.stat)
    }

    fn clear_cached_metadata(&self) {
        *self.metadata.write() = MetadataCache::default();
    }
}

fn shard_index(node: VfsNodeId) -> usize {
    (node.mount_id.0.wrapping_mul(0x9e37_79b1) ^ node.ino as usize) % INODE_STATE_SHARDS
}

pub(crate) fn state_for(node: VfsNodeId) -> Arc<InodeState> {
    let shard = &SHARDS[shard_index(node)];
    if let Some(state) = shard.read().states.get(&node).cloned() {
        return state;
    }
    let mut shard = shard.write();
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
    invalidate_direct_stat_cache();
    let mut shard = SHARDS[shard_index(node)].write();
    let incarnation = shard
        .states
        .remove(&node)
        .map(|state| {
            state.retire();
            state
        })
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
    invalidate_direct_stat_cache();
    state
}

pub(crate) fn with_metadata_mutation<V>(
    node: VfsNodeId,
    mutation: impl FnOnce() -> super::vfs::FsResult<V>,
) -> super::vfs::FsResult<V> {
    invalidate_direct_stat_cache();
    let state = state_for(node);
    let mut guard = state.begin_metadata_mutation();
    let result = mutation();
    // A backend transaction may have made a partial visible change before
    // reporting an error. Conservatively invalidate and advance the epoch for
    // every attempted mutation so an old snapshot can never become stable
    // again after such an error.
    state.clear_cached_metadata();
    guard.commit();
    invalidate_direct_stat_cache();
    result
}

pub(crate) fn with_mapping_mutation<V>(
    node: VfsNodeId,
    mutation: impl FnOnce() -> super::vfs::FsResult<V>,
) -> super::vfs::FsResult<V> {
    invalidate_direct_stat_cache();
    let state = state_for(node);
    let mut guard = state.begin_mapping_mutation();
    let result = mutation();
    state.clear_cached_metadata();
    guard.commit();
    invalidate_direct_stat_cache();
    result
}

/// Variant for write paths whose backend compatibility API reports a byte
/// count instead of `FsResult`. A non-empty write attempt conservatively bumps
/// the mapping epoch because it may allocate, extend, or convert extents.
pub(crate) fn with_mapping_mutation_value<V>(node: VfsNodeId, mutation: impl FnOnce() -> V) -> V {
    invalidate_direct_stat_cache();
    let state = state_for(node);
    let mut guard = state.begin_mapping_mutation();
    let result = mutation();
    state.clear_cached_metadata();
    guard.commit();
    invalidate_direct_stat_cache();
    result
}

pub(crate) fn with_directory_mutation<V>(
    parent: VfsNodeId,
    mutation: impl FnOnce() -> super::vfs::FsResult<V>,
) -> super::vfs::FsResult<V> {
    invalidate_direct_stat_cache();
    let state = state_for(parent);
    let mut guard = state.begin_directory_mutation();
    let result = mutation();
    state.clear_cached_metadata();
    guard.commit();
    invalidate_direct_stat_cache();
    result
}

/// Serializes a cross-directory namespace transaction in stable node order.
/// Equal parents take only one writer guard.
pub(crate) fn with_directory_mutations<V>(
    first: VfsNodeId,
    second: VfsNodeId,
    mutation: impl FnOnce() -> super::vfs::FsResult<V>,
) -> super::vfs::FsResult<V> {
    invalidate_direct_stat_cache();
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
    low_state.clear_cached_metadata();
    high_state.clear_cached_metadata();
    low_guard.commit();
    high_guard.commit();
    invalidate_direct_stat_cache();
    result
}

pub(crate) fn with_metadata_read<V>(node: VfsNodeId, read: impl FnOnce() -> V) -> V {
    let state = state_for(node);
    let _lease = state.metadata_version.read();
    read()
}

pub(crate) fn with_mapping_read<V>(node: VfsNodeId, read: impl FnOnce() -> V) -> V {
    let state = state_for(node);
    let _lease = state.mapping_version.read();
    read()
}

pub(crate) fn with_directory_read<V>(node: VfsNodeId, read: impl FnOnce() -> V) -> V {
    let state = state_for(node);
    let _lease = state.directory_version.read();
    read()
}

fn metadata_or_load(
    node: VfsNodeId,
    full: bool,
    load: impl FnOnce() -> super::vfs::FsResult<FileStat>,
) -> super::vfs::FsResult<FileStat> {
    let state = state_for(node);
    if let Some(version) = state.metadata_version.stable_version()
        && let Some(stat) = state.cached_metadata(full, version)
    {
        return Ok(stat);
    }

    // Lock order is version read lease -> metadata cache. Mutations use
    // version write lease -> metadata cache, so a miss cannot deadlock a
    // mutation or observe a partially-flushed inode.
    let _version_lease = state.metadata_version.read();
    let mut metadata = state.metadata.write();
    let version = state
        .metadata_version
        .stable_version()
        .expect("metadata version must be stable under a read lease");
    let cached = if full { metadata.full } else { metadata.basic };
    if let Some(cached) = cached.filter(|cached| cached.version == version) {
        return Ok(cached.stat);
    }

    let stat = load()?;
    let cached = Some(CachedMetadata { version, stat });
    if full {
        metadata.full = cached;
    } else {
        metadata.basic = cached;
    }
    Ok(stat)
}

pub(crate) fn stat_basic_or_load(
    node: VfsNodeId,
    load: impl FnOnce() -> super::vfs::FsResult<FileStat>,
) -> super::vfs::FsResult<FileStat> {
    metadata_or_load(node, false, load)
}

pub(crate) fn stat_full_or_load(
    node: VfsNodeId,
    load: impl FnOnce() -> super::vfs::FsResult<FileStat>,
) -> super::vfs::FsResult<FileStat> {
    metadata_or_load(node, true, load)
}

/// O(1) generation invalidation for dentry entries keyed by this directory.
pub(crate) fn invalidate_directory(node: VfsNodeId) {
    invalidate_direct_stat_cache();
    let state = state_for(node);
    let mut guard = state.begin_directory_mutation();
    state.clear_cached_metadata();
    guard.commit();
    invalidate_direct_stat_cache();
}

pub(crate) fn invalidate_metadata(node: VfsNodeId) {
    invalidate_direct_stat_cache();
    let state = state_for(node);
    let mut guard = state.begin_metadata_mutation();
    state.clear_cached_metadata();
    guard.commit();
    invalidate_direct_stat_cache();
}

/// Removes only the exact incarnation held by a final-release record. A stale
/// Arc can therefore never erase state installed for a reused inode number.
pub(crate) fn remove_if_same(node: VfsNodeId, expected: &Arc<InodeState>) -> bool {
    let mut shard = SHARDS[shard_index(node)].write();
    let matches = shard
        .states
        .get(&node)
        .is_some_and(|current| Arc::ptr_eq(current, expected));
    if !matches {
        return false;
    }
    invalidate_direct_stat_cache();
    expected.retire();
    shard.states.remove(&node);
    let next = expected
        .incarnation()
        .checked_add(1)
        .expect("inode incarnation exhausted");
    shard.next_incarnations.insert(node, next);
    invalidate_direct_stat_cache();
    true
}

/// Checks whether an owner captured by an open or dirty-cache pin still names
/// the table's current inode incarnation.
pub(crate) fn is_current(expected: &Arc<InodeState>) -> bool {
    let node = expected.node();
    SHARDS[shard_index(node)]
        .read()
        .states
        .get(&node)
        .is_some_and(|current| Arc::ptr_eq(current, expected))
}
