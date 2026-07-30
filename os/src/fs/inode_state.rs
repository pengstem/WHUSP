mod dirty;

use super::{FileStat, MountNamespaceId, WorkingDir, vfs::VfsNodeId};
use crate::config::MAX_CPUS;
use crate::sync::{
    SleepMutex, SleepRwLock, SleepRwLockReadGuard, SleepRwLockWriteGuard, SpinRwLock,
};
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use lazy_static::lazy_static;

pub(crate) use dirty::{
    DIRTY_REGULAR_FILES, DirtyFileCache, DirtyPage, any_regular_file_dirty,
    sync_dirty_regular_file_count,
};

const INODE_STATE_SHARDS: usize = 32;
const DIRECT_STAT_CACHE_SLOTS: usize = 16;
const DIRECT_STAT_NAME_MAX: usize = 255;

static DIRECT_STAT_CACHE_EPOCH: AtomicUsize = AtomicUsize::new(1);

#[derive(Clone, Copy)]
struct DirectStatCacheEntry {
    epoch: usize,
    metadata_epoch: usize,
    namespace_id: MountNamespaceId,
    parent: WorkingDir,
    node: VfsNodeId,
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

#[repr(align(64))]
struct DirectStatMetadataEpoch {
    value: AtomicUsize,
}

impl DirectStatMetadataEpoch {
    fn new() -> Self {
        Self {
            value: AtomicUsize::new(1),
        }
    }
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
    static ref DIRECT_STAT_METADATA_EPOCHS: Vec<DirectStatMetadataEpoch> =
        (0..INODE_STATE_SHARDS)
            .map(|_| DirectStatMetadataEpoch::new())
            .collect();
    // These indexes may outlive a particular InodeState Arc: executable image
    // tracking, in particular, is not a backend-inode retain. Keep their
    // VfsNodeId lifetime semantics while making this module their sole owner.
    static ref WRITABLE_REGULAR_OPEN_COUNTS: SleepMutex<BTreeMap<VfsNodeId, usize>> =
        SleepMutex::new(BTreeMap::new());
    static ref WRITABLE_SHARED_MMAP_REGULAR_COUNTS: SleepMutex<BTreeMap<VfsNodeId, usize>> =
        SleepMutex::new(BTreeMap::new());
    static ref EXECUTABLE_REGULAR_COUNTS: SleepMutex<BTreeMap<VfsNodeId, usize>> =
        SleepMutex::new(BTreeMap::new());
    static ref INODE_FLAGS_CACHE: SleepMutex<BTreeMap<VfsNodeId, u32>> =
        SleepMutex::new(BTreeMap::new());
}

fn increment_count(counts: &SleepMutex<BTreeMap<VfsNodeId, usize>>, node: VfsNodeId) {
    let mut counts = counts.lock();
    *counts.entry(node).or_insert(0) += 1;
}

fn decrement_count(counts: &SleepMutex<BTreeMap<VfsNodeId, usize>>, node: VfsNodeId) {
    let mut counts = counts.lock();
    let Some(count) = counts.get_mut(&node) else {
        return;
    };
    if *count > 1 {
        *count -= 1;
    } else {
        counts.remove(&node);
    }
}

fn has_count(counts: &SleepMutex<BTreeMap<VfsNodeId, usize>>, node: VfsNodeId) -> bool {
    counts.lock().get(&node).copied().unwrap_or(0) > 0
}

pub(crate) fn track_writable_open(node: VfsNodeId) {
    increment_count(&WRITABLE_REGULAR_OPEN_COUNTS, node);
}

pub(crate) fn untrack_writable_open(node: VfsNodeId) {
    decrement_count(&WRITABLE_REGULAR_OPEN_COUNTS, node);
}

pub(crate) fn is_open_writable(node: VfsNodeId) -> bool {
    has_count(&WRITABLE_REGULAR_OPEN_COUNTS, node)
}

pub(crate) fn mount_has_writable_open(mount_id: super::MountId) -> bool {
    WRITABLE_REGULAR_OPEN_COUNTS
        .lock()
        .keys()
        .any(|node| node.mount_id == mount_id)
}

pub(crate) fn track_writable_shared_mmap(node: VfsNodeId) {
    increment_count(&WRITABLE_SHARED_MMAP_REGULAR_COUNTS, node);
}

pub(crate) fn untrack_writable_shared_mmap(node: VfsNodeId) {
    decrement_count(&WRITABLE_SHARED_MMAP_REGULAR_COUNTS, node);
}

pub(crate) fn has_writable_shared_mmap(node: VfsNodeId) -> bool {
    has_count(&WRITABLE_SHARED_MMAP_REGULAR_COUNTS, node)
}

pub(crate) fn track_executable(node: VfsNodeId) {
    increment_count(&EXECUTABLE_REGULAR_COUNTS, node);
}

pub(crate) fn untrack_executable(node: VfsNodeId) {
    decrement_count(&EXECUTABLE_REGULAR_COUNTS, node);
}

pub(crate) fn is_executable(node: VfsNodeId) -> bool {
    has_count(&EXECUTABLE_REGULAR_COUNTS, node)
}

pub(crate) fn cached_inode_flags(node: VfsNodeId) -> Option<u32> {
    INODE_FLAGS_CACHE.lock().get(&node).copied()
}

pub(crate) fn update_inode_flags_cache(node: VfsNodeId, flags: u32) {
    INODE_FLAGS_CACHE.lock().insert(node, flags);
}

pub(crate) fn invalidate_inode_flags_cache(node: VfsNodeId) {
    INODE_FLAGS_CACHE.lock().remove(&node);
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
        || direct_stat_metadata_epoch(entry.node) != entry.metadata_epoch
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
    expected_metadata_epoch: usize,
    namespace_id: MountNamespaceId,
    parent: WorkingDir,
    name: &str,
    node: VfsNodeId,
    stat: FileStat,
) {
    if name.len() > DIRECT_STAT_NAME_MAX {
        return;
    }
    let mut entry = DirectStatCacheEntry {
        epoch: expected_epoch,
        metadata_epoch: expected_metadata_epoch,
        namespace_id,
        parent,
        node,
        name_len: name.len(),
        name: [0; DIRECT_STAT_NAME_MAX],
        stat,
    };
    entry.name[..name.len()].copy_from_slice(name.as_bytes());
    let slot = direct_stat_slot(parent, name);
    // See lookup above: non-preemptible kernel execution keeps this write on
    // the selected CPU without masking interrupts.
    if DIRECT_STAT_CACHE_EPOCH.load(Ordering::Acquire) != expected_epoch
        || direct_stat_metadata_epoch(node) != expected_metadata_epoch
    {
        return;
    }
    let cache = unsafe { &mut *DIRECT_STAT_CACHES[crate::cpu::current_id()].data.get() };
    cache.entries[slot] = Some(entry);
}

pub(crate) fn direct_stat_cache_epoch() -> usize {
    DIRECT_STAT_CACHE_EPOCH.load(Ordering::Acquire)
}

pub(crate) fn direct_stat_metadata_epoch(node: VfsNodeId) -> usize {
    DIRECT_STAT_METADATA_EPOCHS[shard_index(node)]
        .value
        .load(Ordering::Acquire)
}

fn invalidate_direct_stat_metadata(node: VfsNodeId) {
    DIRECT_STAT_METADATA_EPOCHS[shard_index(node)]
        .value
        .fetch_add(1, Ordering::AcqRel);
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

#[derive(Clone, Copy)]
pub(crate) enum MetadataCacheUpdate {
    Mode(u32),
    Owner {
        uid: Option<u32>,
        gid: Option<u32>,
    },
    Times {
        atime: Option<super::FileTimestamp>,
        mtime: Option<super::FileTimestamp>,
        ctime: super::FileTimestamp,
    },
    InodeFlags(u32),
}

impl MetadataCacheUpdate {
    fn apply(self, stat: &mut FileStat) {
        match self {
            Self::Mode(mode) => stat.mode = (stat.mode & !0o7777) | (mode & 0o7777),
            Self::Owner { uid, gid } => {
                if let Some(uid) = uid {
                    stat.uid = uid;
                }
                if let Some(gid) = gid {
                    stat.gid = gid;
                }
            }
            Self::Times {
                atime,
                mtime,
                ctime,
            } => {
                if let Some(atime) = atime {
                    stat.atime_sec = atime.sec;
                    stat.atime_nsec = atime.nsec;
                }
                if let Some(mtime) = mtime {
                    stat.mtime_sec = mtime.sec;
                    stat.mtime_nsec = mtime.nsec;
                }
                stat.ctime_sec = ctime.sec;
                stat.ctime_nsec = ctime.nsec;
            }
            Self::InodeFlags(flags) => stat.inode_flags = flags,
        }
    }
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
    fn committed_version(&self) -> usize {
        self.start_version
            .checked_add(2)
            .expect("inode version domain exhausted")
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for VersionMutationGuard<'_> {
    fn drop(&mut self) {
        let current = self.domain.version.load(Ordering::Acquire);
        assert_eq!(current & 1, 1, "inode mutation ended from stable version");
        let next = if self.committed {
            self.committed_version()
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
        let stat = cached
            .filter(|cached| cached.version == version)
            .map(|cached| cached.stat)?;
        // A mutation publishes an odd version before it can take the cache
        // writer. Recheck while the cache read guard is still held so this
        // lockless-version fast path linearizes either wholly before or wholly
        // after the mutation.
        (self.metadata_version.stable_version() == Some(version)).then_some(stat)
    }

    fn clear_cached_metadata(&self) {
        *self.metadata.write() = MetadataCache::default();
    }

    fn update_cached_metadata(
        &self,
        start_version: usize,
        committed_version: usize,
        update: MetadataCacheUpdate,
    ) {
        fn update_slot(
            slot: &mut Option<CachedMetadata>,
            start_version: usize,
            committed_version: usize,
            update: MetadataCacheUpdate,
        ) {
            let Some(cached) = slot.as_mut() else {
                return;
            };
            if cached.version != start_version {
                *slot = None;
                return;
            }
            update.apply(&mut cached.stat);
            cached.version = committed_version;
        }

        let mut metadata = self.metadata.write();
        update_slot(
            &mut metadata.basic,
            start_version,
            committed_version,
            update,
        );
        update_slot(&mut metadata.full, start_version, committed_version, update);
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
    invalidate_direct_stat_metadata(node);
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
    invalidate_direct_stat_metadata(node);
    invalidate_direct_stat_cache();
    state
}

pub(crate) fn with_metadata_update<V>(
    node: VfsNodeId,
    update: MetadataCacheUpdate,
    mutation: impl FnOnce() -> super::vfs::FsResult<V>,
) -> super::vfs::FsResult<V> {
    let state = state_for(node);
    with_metadata_update_state(&state, update, mutation)
}

pub(crate) fn with_metadata_update_state<V>(
    state: &InodeState,
    update: MetadataCacheUpdate,
    mutation: impl FnOnce() -> super::vfs::FsResult<V>,
) -> super::vfs::FsResult<V> {
    with_metadata_update_state_inner(state, update, mutation)
}

fn with_metadata_update_state_inner<V>(
    state: &InodeState,
    update: MetadataCacheUpdate,
    mutation: impl FnOnce() -> super::vfs::FsResult<V>,
) -> super::vfs::FsResult<V> {
    let node = state.node();
    invalidate_direct_stat_metadata(node);
    let mut guard = state.begin_metadata_mutation();
    let result = mutation();
    // A backend transaction may have made a partial visible change before
    // reporting an error. Conservatively invalidate and advance the epoch for
    // every attempted mutation so an old snapshot can never become stable
    // again after such an error.
    if result.is_ok() {
        state.update_cached_metadata(guard.start_version, guard.committed_version(), update);
    } else {
        state.clear_cached_metadata();
    }
    guard.commit();
    invalidate_direct_stat_metadata(node);
    result
}

pub(crate) fn with_mapping_mutation<V>(
    node: VfsNodeId,
    mutation: impl FnOnce() -> super::vfs::FsResult<V>,
) -> super::vfs::FsResult<V> {
    let state = state_for(node);
    with_mapping_mutation_state(&state, mutation)
}

pub(crate) fn with_mapping_mutation_state<V>(
    state: &InodeState,
    mutation: impl FnOnce() -> super::vfs::FsResult<V>,
) -> super::vfs::FsResult<V> {
    let node = state.node();
    invalidate_direct_stat_metadata(node);
    let mut guard = state.begin_mapping_mutation();
    let result = mutation();
    state.clear_cached_metadata();
    guard.commit();
    invalidate_direct_stat_metadata(node);
    result
}

/// Variant for write paths whose backend compatibility API reports a byte
/// count instead of `FsResult`. A non-empty write attempt conservatively bumps
/// the mapping epoch because it may allocate, extend, or convert extents.
pub(crate) fn with_mapping_mutation_value_state<V>(
    state: &InodeState,
    mutation: impl FnOnce() -> V,
) -> V {
    let node = state.node();
    invalidate_direct_stat_metadata(node);
    let mut guard = state.begin_mapping_mutation();
    let result = mutation();
    state.clear_cached_metadata();
    guard.commit();
    invalidate_direct_stat_metadata(node);
    result
}

pub(crate) fn with_directory_mutation<V>(
    parent: VfsNodeId,
    mutation: impl FnOnce() -> super::vfs::FsResult<V>,
) -> super::vfs::FsResult<V> {
    invalidate_direct_stat_cache();
    invalidate_direct_stat_metadata(parent);
    let state = state_for(parent);
    let mut guard = state.begin_directory_mutation();
    let result = mutation();
    state.clear_cached_metadata();
    guard.commit();
    invalidate_direct_stat_metadata(parent);
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
    if first == second {
        return with_directory_mutation(first, mutation);
    }
    invalidate_direct_stat_cache();
    invalidate_direct_stat_metadata(first);
    invalidate_direct_stat_metadata(second);
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
    invalidate_direct_stat_metadata(first);
    invalidate_direct_stat_metadata(second);
    invalidate_direct_stat_cache();
    result
}

pub(crate) fn with_metadata_read<V>(node: VfsNodeId, read: impl FnOnce() -> V) -> V {
    let state = state_for(node);
    with_metadata_read_state(&state, read)
}

pub(crate) fn with_metadata_read_state<V>(state: &InodeState, read: impl FnOnce() -> V) -> V {
    let _lease = state.metadata_version.read();
    read()
}

pub(crate) fn with_mapping_read<V>(node: VfsNodeId, read: impl FnOnce() -> V) -> V {
    let state = state_for(node);
    with_mapping_read_state(&state, read)
}

pub(crate) fn with_mapping_read_state<V>(state: &InodeState, read: impl FnOnce() -> V) -> V {
    let _lease = state.mapping_version.read();
    read()
}

pub(crate) fn with_directory_read<V>(node: VfsNodeId, read: impl FnOnce(usize) -> V) -> V {
    let state = state_for(node);
    with_directory_read_state(&state, read)
}

pub(crate) fn with_directory_read_state<V>(state: &InodeState, read: impl FnOnce(usize) -> V) -> V {
    let _lease = state.directory_version.read();
    let version = state
        .directory_version()
        .expect("directory version unstable under shared lease");
    read(version)
}

fn metadata_or_load(
    state: &InodeState,
    full: bool,
    load: impl FnOnce() -> super::vfs::FsResult<FileStat>,
) -> super::vfs::FsResult<FileStat> {
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
    let state = state_for(node);
    stat_basic_or_load_state(&state, load)
}

pub(crate) fn stat_basic_or_load_state(
    state: &InodeState,
    load: impl FnOnce() -> super::vfs::FsResult<FileStat>,
) -> super::vfs::FsResult<FileStat> {
    metadata_or_load(state, false, load)
}

pub(crate) fn stat_full_or_load(
    node: VfsNodeId,
    load: impl FnOnce() -> super::vfs::FsResult<FileStat>,
) -> super::vfs::FsResult<FileStat> {
    let state = state_for(node);
    stat_full_or_load_state(&state, load)
}

pub(crate) fn stat_full_or_load_state(
    state: &InodeState,
    load: impl FnOnce() -> super::vfs::FsResult<FileStat>,
) -> super::vfs::FsResult<FileStat> {
    metadata_or_load(state, true, load)
}

/// O(1) generation invalidation for dentry entries keyed by this directory.
pub(crate) fn invalidate_directory(node: VfsNodeId) {
    invalidate_direct_stat_cache();
    invalidate_direct_stat_metadata(node);
    let state = state_for(node);
    let mut guard = state.begin_directory_mutation();
    state.clear_cached_metadata();
    guard.commit();
    invalidate_direct_stat_metadata(node);
    invalidate_direct_stat_cache();
}

pub(crate) fn invalidate_metadata(node: VfsNodeId) {
    invalidate_direct_stat_metadata(node);
    let state = state_for(node);
    let mut guard = state.begin_metadata_mutation();
    state.clear_cached_metadata();
    guard.commit();
    invalidate_direct_stat_metadata(node);
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
    invalidate_direct_stat_metadata(node);
    expected.retire();
    shard.states.remove(&node);
    let next = expected
        .incarnation()
        .checked_add(1)
        .expect("inode incarnation exhausted");
    shard.next_incarnations.insert(node, next);
    invalidate_direct_stat_metadata(node);
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
