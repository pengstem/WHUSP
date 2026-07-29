use super::dirent::{
    DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK, DT_UNKNOWN, LINUX_DIRENT64_ALIGN,
    LINUX_DIRENT64_HEADER_SIZE,
};
#[cfg(feature = "perf-counters")]
use super::vfs::BackendIoSnapshot;
use super::vfs::{
    BackendDirectoryEntry, BackendDirectoryReadPlan, BackendDirectorySnapshot, BackendOp,
    BackendReadPlan, BackendWritePlan, ConcurrentFileSystemBackend, FileSystemStat, FsError,
    FsNodeKind, FsResult, InodeRelease, LegacyFileSystemBackend,
};
use super::{FS_STATX_ATTR_FLAGS, FileStat, FileTimestamp};
use crate::drivers::block::VirtIOBlock;
use crate::perf;
use crate::sync::{
    RawSleepLock, RawSpinNoIrqLock, SleepMutex, SleepRwLock, SleepRwLockReadGuard,
    SleepRwLockWriteGuard, SpinNoIrqLock,
};
use crate::task::suspend_current_and_run_next;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::{vec, vec::Vec};
use core::cell::UnsafeCell;
use core::str;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use lwext4_rust::ffi::{
    EEXIST, EINVAL, EIO, EISDIR, ENOENT, ENOSPC, ENOTDIR, ENOTEMPTY, ENOTSUP, EXT4_DE_BLKDEV,
    EXT4_DE_CHRDEV, EXT4_DE_DIR, EXT4_DE_FIFO, EXT4_DE_REG_FILE, EXT4_DE_SOCK, EXT4_DE_SYMLINK,
    EXT4_ROOT_INO,
};
use lwext4_rust::{
    BlockDevice as Ext4BlockDevice, EXT4_DEV_BSIZE,
    Ext4DirectoryReadPlan as LwExt4DirectoryReadPlan, Ext4Error, Ext4Filesystem,
    Ext4MappedReadPlan, Ext4MappedWritePlan, Ext4Result, Ext4SymlinkReadPlan, FsConfig, InodeType,
    SystemHal,
};

pub(super) struct KernelHal;

impl SystemHal for KernelHal {
    // UNFINISHED: Linux stat timestamps should reflect filesystem time updates;
    // this HAL currently exposes no wall-clock source to lwext4.
    fn now() -> Option<Duration> {
        None
    }
}

const EXT4_BCACHE_LBA_LOCK_SHARDS: usize = 256;
const EXT4_ALLOCATOR_GROUP_LOCK_SHARDS: usize = 128;

/// Lock domain owned by exactly one lwext4 metadata cache.
///
/// The index lock covers only RB-tree/list/refcount bookkeeping. LBA shards
/// serialize cache fill, eviction, and writeback state for colliding logical
/// blocks without forcing unrelated device I/O through one global lock.
struct Ext4BcacheLocks {
    index: RawSpinNoIrqLock,
    lba_shards: Vec<RawSleepLock>,
}

impl Ext4BcacheLocks {
    fn new() -> Self {
        Self {
            index: RawSpinNoIrqLock::new(),
            lba_shards: (0..EXT4_BCACHE_LBA_LOCK_SHARDS)
                .map(|_| RawSleepLock::new())
                .collect(),
        }
    }

    #[inline]
    fn lba(&self, lba: u64) -> &RawSleepLock {
        let mixed = lba ^ (lba >> 17) ^ (lba >> 37);
        &self.lba_shards[mixed as usize % self.lba_shards.len()]
    }

    #[inline]
    fn lock_lba(&self, lba: u64) {
        let lock = self.lba(lba);
        #[cfg(feature = "perf-counters")]
        {
            let contended = !lock.try_lock();
            if contended {
                lock.lock();
            }
            perf::record_ext4_bcache_lba_lock(contended);
        }
        #[cfg(not(feature = "perf-counters"))]
        lock.lock();
    }

    /// Releases one matching [`Self::lock_lba`] acquisition.
    ///
    /// # Safety
    ///
    /// The current task must own the shard selected by `lba` exactly once.
    #[inline]
    unsafe fn unlock_lba(&self, lba: u64) {
        unsafe { self.lba(lba).unlock() };
    }

    #[inline]
    fn lock_index(&self) {
        #[cfg(feature = "perf-counters")]
        {
            let contended = !self.index.try_lock();
            if contended {
                self.index.lock();
            }
            perf::record_ext4_bcache_index_lock(contended);
        }
        #[cfg(not(feature = "perf-counters"))]
        self.index.lock();
    }

    /// Releases one matching [`Self::lock_index`] acquisition.
    ///
    /// # Safety
    ///
    /// The current task must own the index lock exactly once.
    #[inline]
    unsafe fn unlock_index(&self) {
        unsafe { self.index.unlock() };
    }
}

/// Allocation ownership shared by every writable caller of one lwext4 core.
///
/// Group shards may sleep across bitmap/GDT metadata I/O. The global lock is
/// reserved for the in-memory superblock free counters and allocation cursor;
/// C never holds it while acquiring a group, bcache, or device lock.
struct Ext4AllocatorLocks {
    groups: Vec<RawSleepLock>,
    global: RawSleepLock,
}

impl Ext4AllocatorLocks {
    fn new() -> Self {
        Self {
            groups: (0..EXT4_ALLOCATOR_GROUP_LOCK_SHARDS)
                .map(|_| RawSleepLock::new())
                .collect(),
            global: RawSleepLock::new(),
        }
    }

    #[inline]
    fn group(&self, bgid: u32) -> &RawSleepLock {
        &self.groups[bgid as usize % self.groups.len()]
    }
}

#[derive(Clone)]
pub(super) struct KernelDisk {
    dev: Arc<VirtIOBlock>,
    concurrent_bcache: bool,
    versioned_bcache: bool,
    concurrent_metadata: bool,
    cache_epoch: Arc<Ext4CacheEpoch>,
    write_sequence: Arc<Ext4Sequence>,
    physical_leases: Arc<Ext4PhysicalLeaseTable>,
    block_versions: Arc<Ext4BlockVersions>,
    bcache_locks: Arc<Ext4BcacheLocks>,
    allocator_locks: Arc<Ext4AllocatorLocks>,
    #[cfg(feature = "perf-counters")]
    io_counters: Arc<Ext4IoCounters>,
}

/// Multi-writer sequence used where readers copy data into private buffers.
///
/// An uncontended read performs two atomic loads and never enters a scheduler
/// or IRQ-masked lock. Low bits count active writers; the last writer advances
/// the epoch. A reader that overlaps any writer discards its private copy and
/// yields until the active count returns to zero. Writers do not serialize on
/// this counter, which keeps the sequence compatible with future independent
/// mapped-overwrite plans.
struct Ext4Sequence {
    value: AtomicUsize,
}

const EXT4_SEQUENCE_WRITER_BITS: usize = 16;
const EXT4_SEQUENCE_WRITER_LIMIT: usize = 1 << EXT4_SEQUENCE_WRITER_BITS;
const EXT4_SEQUENCE_WRITER_MASK: usize = EXT4_SEQUENCE_WRITER_LIMIT - 1;

/// Generation observed by the one shared read-only metadata cache.
///
/// Writers advance it only after publishing device bytes. A stale buffer is
/// refilled in place when unowned, or detached and retired after its last old
/// reader when still referenced. This gives new readers a separate payload
/// without a mount-wide cache-flush writer phase.
struct Ext4CacheEpoch {
    value: AtomicUsize,
}

impl Ext4CacheEpoch {
    fn new() -> Self {
        Self {
            value: AtomicUsize::new(1),
        }
    }

    fn current(&self) -> u64 {
        self.value.load(Ordering::Acquire) as u64
    }

    fn advance(&self) {
        let previous = self.value.fetch_add(1, Ordering::AcqRel);
        assert_ne!(previous, usize::MAX, "ext4 cache epoch wrapped");
    }
}

impl Ext4Sequence {
    fn new() -> Self {
        Self {
            value: AtomicUsize::new(0),
        }
    }

    fn begin_write(&self) -> Ext4SequenceWriteGuard<'_> {
        let previous = self.value.fetch_add(1, Ordering::AcqRel);
        assert_ne!(
            previous & EXT4_SEQUENCE_WRITER_MASK,
            EXT4_SEQUENCE_WRITER_MASK,
            "ext4 sequence active-writer count exhausted"
        );
        Ext4SequenceWriteGuard { sequence: self }
    }

    fn stable_value(&self) -> usize {
        loop {
            let value = self.value.load(Ordering::Acquire);
            if value & EXT4_SEQUENCE_WRITER_MASK == 0 {
                return value;
            }
            // The writer may be asleep in VirtIO I/O. Yielding here avoids
            // turning a real read/write conflict into an SMP spin convoy.
            suspend_current_and_run_next();
        }
    }

    fn read_stable<V>(&self, mut read: impl FnMut() -> V) -> V {
        loop {
            let before = self.stable_value();
            let result = read();
            if self.value.load(Ordering::Acquire) == before {
                return result;
            }
        }
    }
}

struct Ext4SequenceWriteGuard<'a> {
    sequence: &'a Ext4Sequence,
}

impl Drop for Ext4SequenceWriteGuard<'_> {
    fn drop(&mut self) {
        // Adding `2^N - 1` decrements the low-bit writer count and advances
        // the high-bit epoch in one atomic RMW. This remains correct when
        // writers overlap: readers keep waiting for a zero low-bit count, and
        // every completed writer changes the stable epoch they validate.
        let previous = self
            .sequence
            .value
            .fetch_add(EXT4_SEQUENCE_WRITER_LIMIT - 1, Ordering::Release);
        assert_ne!(
            previous & EXT4_SEQUENCE_WRITER_MASK,
            0,
            "ext4 sequence writer lost ownership"
        );
    }
}

/// Exact physical-sector ownership shared by every EXT4 write path.
///
/// Callers reserve sorted integer LBAs only. No FFI pointer, bcache guard, or
/// filesystem-core guard may be held while waiting for a conflicting range.
struct Ext4PhysicalLeaseTable {
    blocks: SpinNoIrqLock<BTreeSet<u64>>,
}

impl Ext4PhysicalLeaseTable {
    fn new() -> Self {
        Self {
            blocks: SpinNoIrqLock::new(BTreeSet::new()),
        }
    }

    fn reserve_wait<I>(self: &Arc<Self>, blocks: I) -> Ext4PhysicalLease
    where
        I: IntoIterator<Item = u64>,
    {
        let unique = blocks.into_iter().collect::<BTreeSet<_>>();
        loop {
            let mut reserved = self.blocks.lock();
            if unique.iter().all(|block| !reserved.contains(block)) {
                reserved.extend(unique.iter().copied());
                drop(reserved);
                return Ext4PhysicalLease {
                    table: self.clone(),
                    blocks: unique.iter().copied().collect(),
                };
            }
            drop(reserved);
            suspend_current_and_run_next();
        }
    }
}

struct Ext4PhysicalLease {
    table: Arc<Ext4PhysicalLeaseTable>,
    blocks: Vec<u64>,
}

impl Drop for Ext4PhysicalLease {
    fn drop(&mut self) {
        let mut reserved = self.table.blocks.lock();
        for block in self.blocks.drain(..) {
            assert!(
                reserved.remove(&block),
                "ext4 physical LBA lease lost ownership"
            );
        }
    }
}

#[derive(Default)]
struct Ext4BlockVersionState {
    next: usize,
    versions: BTreeMap<u64, usize>,
}

/// Per-physical-sector commit versions used by direct data write plans.
struct Ext4BlockVersions {
    state: SpinNoIrqLock<Ext4BlockVersionState>,
}

impl Ext4BlockVersions {
    fn new() -> Self {
        Self {
            state: SpinNoIrqLock::new(Ext4BlockVersionState::default()),
        }
    }

    fn bump_range(&self, first: u64, count: usize) {
        if count == 0 {
            return;
        }
        let mut state = self.state.lock();
        state.next = state.next.wrapping_add(1);
        assert_ne!(state.next, 0, "ext4 block version counter wrapped");
        let version = state.next;
        for delta in 0..count {
            state.versions.insert(first + delta as u64, version);
        }
    }
}

#[cfg(feature = "perf-counters")]
#[derive(Default)]
struct Ext4IoCounters {
    read_calls: AtomicUsize,
    read_blocks: AtomicUsize,
    read_bytes: AtomicUsize,
    write_calls: AtomicUsize,
    write_blocks: AtomicUsize,
    write_bytes: AtomicUsize,
}

#[cfg(feature = "perf-counters")]
impl Ext4IoCounters {
    fn record_read(&self, blocks: usize, bytes: usize) {
        self.read_calls.fetch_add(1, Ordering::Relaxed);
        self.read_blocks.fetch_add(blocks, Ordering::Relaxed);
        self.read_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn record_write(&self, blocks: usize, bytes: usize) {
        self.write_calls.fetch_add(1, Ordering::Relaxed);
        self.write_blocks.fetch_add(blocks, Ordering::Relaxed);
        self.write_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn snapshot(&self) -> BackendIoSnapshot {
        BackendIoSnapshot {
            read_calls: self.read_calls.load(Ordering::Relaxed),
            read_blocks: self.read_blocks.load(Ordering::Relaxed),
            read_bytes: self.read_bytes.load(Ordering::Relaxed),
            write_calls: self.write_calls.load(Ordering::Relaxed),
            write_blocks: self.write_blocks.load(Ordering::Relaxed),
            write_bytes: self.write_bytes.load(Ordering::Relaxed),
        }
    }
}

impl Ext4BlockDevice for KernelDisk {
    fn concurrent_bcache(&self) -> bool {
        self.concurrent_bcache
    }

    fn versioned_bcache(&self) -> bool {
        self.versioned_bcache
    }

    fn bcache_generation(&self) -> u64 {
        self.cache_epoch.current()
    }

    fn concurrent_metadata(&self) -> bool {
        self.concurrent_metadata
    }

    fn write_blocks(&self, block_id: u64, buf: &[u8]) -> Ext4Result<usize> {
        if buf.len() % EXT4_DEV_BSIZE != 0 {
            return Err(Ext4Error::new(EIO as _, "unaligned block write"));
        }
        let blocks = buf.len() / EXT4_DEV_BSIZE;
        let _physical = self
            .physical_leases
            .reserve_wait(block_id..block_id + blocks as u64);
        let _write = self.write_sequence.begin_write();
        self.dev.write_blocks(block_id as usize, buf);
        self.block_versions.bump_range(block_id, blocks);
        self.cache_epoch.advance();
        #[cfg(feature = "perf-counters")]
        self.io_counters.record_write(blocks, buf.len());
        perf::record_ext4_block_write(blocks, buf.len());
        Ok(buf.len())
    }

    fn read_blocks(&self, block_id: u64, buf: &mut [u8]) -> Ext4Result<usize> {
        if buf.len() % EXT4_DEV_BSIZE != 0 {
            return Err(Ext4Error::new(EIO as _, "unaligned block read"));
        }
        let blocks = buf.len() / EXT4_DEV_BSIZE;
        self.write_sequence.read_stable(|| {
            self.dev.read_blocks(block_id as usize, buf);
            #[cfg(feature = "perf-counters")]
            self.io_counters.record_read(blocks, buf.len());
            perf::record_ext4_block_read(blocks, buf.len());
        });
        Ok(buf.len())
    }

    fn num_blocks(&self) -> Ext4Result<u64> {
        Ok(self.dev.num_blocks())
    }

    fn lock_bcache_index(&self) {
        self.bcache_locks.lock_index();
    }

    unsafe fn unlock_bcache_index(&self) {
        unsafe { self.bcache_locks.unlock_index() };
    }

    fn lock_bcache_lba(&self, lba: u64) {
        self.bcache_locks.lock_lba(lba);
    }

    unsafe fn unlock_bcache_lba(&self, lba: u64) {
        unsafe { self.bcache_locks.unlock_lba(lba) };
    }

    fn lock_metadata_group(&self, bgid: u32) {
        self.allocator_locks.group(bgid).lock();
    }

    unsafe fn unlock_metadata_group(&self, bgid: u32) {
        unsafe { self.allocator_locks.group(bgid).unlock() };
    }

    fn lock_metadata_global(&self) {
        self.allocator_locks.global.lock();
    }

    unsafe fn unlock_metadata_global(&self) {
        unsafe { self.allocator_locks.global.unlock() };
    }
}

type KernelExt4Fs = Ext4Filesystem<KernelHal, KernelDisk>;

const EXT4_CONFIG: FsConfig = FsConfig { bcache_size: 256 };
const EXT4_INODE_RUNTIME_SHARDS: usize = 32;
// lwext4_rust::ffi does not export ENAMETOOLONG; define it locally.
const ENAMETOOLONG: u32 = 36;

fn into_node_kind(kind: InodeType) -> FsNodeKind {
    match kind {
        InodeType::Directory => FsNodeKind::Directory,
        InodeType::RegularFile => FsNodeKind::RegularFile,
        InodeType::Symlink => FsNodeKind::Symlink,
        InodeType::Fifo => FsNodeKind::Fifo,
        InodeType::CharacterDevice => FsNodeKind::CharacterDevice,
        InodeType::BlockDevice => FsNodeKind::BlockDevice,
        InodeType::Socket => FsNodeKind::Socket,
        _ => FsNodeKind::Other,
    }
}

fn into_linux_dtype(kind: InodeType) -> u8 {
    match kind {
        InodeType::Directory => DT_DIR,
        InodeType::RegularFile => DT_REG,
        InodeType::Symlink => DT_LNK,
        InodeType::Fifo => DT_FIFO,
        InodeType::CharacterDevice => DT_CHR,
        InodeType::BlockDevice => DT_BLK,
        InodeType::Socket => DT_SOCK,
        _ => DT_UNKNOWN,
    }
}

use super::align_up;

fn map_ext4_error(err: Ext4Error) -> FsError {
    // UNFINISHED: lwext4 exposes raw errno values that are not all mapped
    // into this kernel's VFS error model yet; unmapped codes fall back to Io.
    let code = err.code as u32;
    match code {
        ENOENT => FsError::NotFound,
        ENOTDIR => FsError::NotDir,
        EISDIR => FsError::IsDir,
        EEXIST => FsError::AlreadyExists,
        EINVAL => FsError::InvalidInput,
        ENOTEMPTY => FsError::NotEmpty,
        ENAMETOOLONG => FsError::NameTooLong,
        EIO => FsError::Io,
        ENOSPC => FsError::NoSpace,
        ENOTSUP => FsError::Unsupported,
        _ => FsError::Io,
    }
}

pub(super) struct Ext4Mount {
    fs: KernelExt4Fs,
    device: Arc<VirtIOBlock>,
    cache_epoch: Arc<Ext4CacheEpoch>,
    write_sequence: Arc<Ext4Sequence>,
    physical_leases: Arc<Ext4PhysicalLeaseTable>,
    block_versions: Arc<Ext4BlockVersions>,
    #[cfg(feature = "perf-counters")]
    io_counters: Arc<Ext4IoCounters>,
    inode_runtime: Arc<Ext4InodeRuntimeTable>,
}

#[derive(Clone, Copy)]
enum Ext4InodeMetadataMutation {
    Times {
        atime: Option<FileTimestamp>,
        mtime: Option<FileTimestamp>,
        ctime: FileTimestamp,
    },
    Mode(u32),
    Owner {
        uid: Option<u32>,
        gid: Option<u32>,
    },
    Flags(u32),
}

#[derive(Default)]
struct Ext4InodeRuntimeState {
    open_count: usize,
    unlinking: bool,
    pending_unlink: bool,
    special_rdev: Option<u64>,
}

struct Ext4InodeRuntimeTable {
    shards: Vec<SleepMutex<BTreeMap<u32, Ext4InodeRuntimeState>>>,
}

impl Ext4InodeRuntimeTable {
    fn new() -> Self {
        Self {
            shards: (0..EXT4_INODE_RUNTIME_SHARDS)
                .map(|_| SleepMutex::new(BTreeMap::new()))
                .collect(),
        }
    }

    #[inline]
    fn shard(&self, ino: u32) -> &SleepMutex<BTreeMap<u32, Ext4InodeRuntimeState>> {
        let ino = ino as usize;
        &self.shards[(ino ^ (ino >> 5)) % self.shards.len()]
    }

    fn special_rdev(&self, ino: u32) -> Option<u64> {
        self.shard(ino)
            .lock()
            .get(&ino)
            .and_then(|state| state.special_rdev)
    }

    fn set_special_rdev(&self, ino: u32, rdev: u64) {
        self.shard(ino).lock().entry(ino).or_default().special_rdev = Some(rdev);
    }

    fn begin_unlink(&self, ino: u32) -> bool {
        let mut runtime = self.shard(ino).lock();
        let state = runtime.entry(ino).or_default();
        debug_assert!(!state.unlinking);
        state.unlinking = true;
        state.open_count > 0
    }

    fn abort_unlink(&self, ino: u32) {
        let mut runtime = self.shard(ino).lock();
        let Some(state) = runtime.get_mut(&ino) else {
            return;
        };
        state.unlinking = false;
        if state.open_count == 0 && !state.pending_unlink && state.special_rdev.is_none() {
            runtime.remove(&ino);
        }
    }

    /// Completes the runtime half of unlink after the filesystem transaction.
    /// Returns true when the last open reference disappeared while the
    /// deferred unlink was in flight, so the caller must free the inode now.
    fn finish_unlink(&self, ino: u32, pending_unlink: bool, inode_exists: bool) -> bool {
        let mut runtime = self.shard(ino).lock();
        if !inode_exists {
            runtime.remove(&ino);
            return false;
        }
        let state = runtime.entry(ino).or_default();
        state.unlinking = false;
        state.pending_unlink |= pending_unlink;
        if state.pending_unlink {
            return state.open_count == 0;
        }
        if state.open_count == 0 && state.special_rdev.is_none() {
            runtime.remove(&ino);
        }
        false
    }

    fn remove(&self, ino: u32) {
        self.shard(ino).lock().remove(&ino);
    }

    fn retain(&self, ino: u32) -> bool {
        let mut runtime = self.shard(ino).lock();
        let state = runtime.entry(ino).or_default();
        if state.unlinking || state.pending_unlink {
            return false;
        }
        state.open_count += 1;
        true
    }

    fn retain_at_generation(&self, ino: u32, generation: usize, sequence: &Ext4Sequence) -> bool {
        let mut runtime = self.shard(ino).lock();
        // Check the writer epoch while holding the same inode-runtime shard
        // that begin_unlink() must acquire. If a writer starts after this
        // check, it necessarily observes the incremented open_count; if it
        // started earlier, the odd or changed generation rejects this retain.
        if sequence.value.load(Ordering::Acquire) != generation {
            return false;
        }
        let state = runtime.entry(ino).or_default();
        if state.unlinking || state.pending_unlink {
            return false;
        }
        state.open_count += 1;
        true
    }

    fn prepare_release(&self, ino: u32) -> Ext4RuntimeRelease {
        let mut runtime = self.shard(ino).lock();
        prepare_runtime_release_locked(&mut runtime, ino)
    }

    fn try_prepare_release(&self, ino: u32) -> Option<Ext4RuntimeRelease> {
        let mut runtime = self.shard(ino).try_lock()?;
        Some(prepare_runtime_release_locked(&mut runtime, ino))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ext4RuntimeRelease {
    Retained,
    FreeUnlinked,
}

fn prepare_runtime_release_locked(
    runtime: &mut BTreeMap<u32, Ext4InodeRuntimeState>,
    ino: u32,
) -> Ext4RuntimeRelease {
    let Some(state) = runtime.get_mut(&ino) else {
        return Ext4RuntimeRelease::Retained;
    };
    if state.open_count > 1 {
        state.open_count -= 1;
        return Ext4RuntimeRelease::Retained;
    }
    if state.open_count == 1 {
        state.open_count = 0;
    }
    if state.unlinking {
        // unlink() has already sampled the open count and will either install
        // pending_unlink or remove this state when its filesystem transaction
        // completes. Preserve the zero-count record until that handoff.
        return Ext4RuntimeRelease::Retained;
    }
    if state.pending_unlink {
        // Keep the record until the physical free succeeds. A failed
        // drop-time try can then be retried from the per-mount release queue.
        return Ext4RuntimeRelease::FreeUnlinked;
    }
    if state.special_rdev.is_none() {
        runtime.remove(&ino);
    }
    Ext4RuntimeRelease::Retained
}

// SAFETY: the FFI core and its raw pointers move only as one `Ext4Mount`.
// Writable instances are entered behind their owning core mutex. The one
// shared read-only instance is exposed only by `SharedExt4ReadCore`, whose
// narrower Sync proof audits its callable operations.
unsafe impl Send for Ext4Mount {}

impl Ext4Mount {
    fn open(
        device: Arc<VirtIOBlock>,
        cache_epoch: Arc<Ext4CacheEpoch>,
        write_sequence: Arc<Ext4Sequence>,
        physical_leases: Arc<Ext4PhysicalLeaseTable>,
        block_versions: Arc<Ext4BlockVersions>,
    ) -> Result<Self, Ext4Error> {
        #[cfg(feature = "perf-counters")]
        let io_counters = Arc::new(Ext4IoCounters::default());
        let inode_runtime = Arc::new(Ext4InodeRuntimeTable::new());
        Ok(Self {
            fs: KernelExt4Fs::new(
                KernelDisk {
                    dev: device.clone(),
                    concurrent_bcache: true,
                    versioned_bcache: false,
                    concurrent_metadata: true,
                    cache_epoch: cache_epoch.clone(),
                    write_sequence: write_sequence.clone(),
                    physical_leases: physical_leases.clone(),
                    block_versions: block_versions.clone(),
                    bcache_locks: Arc::new(Ext4BcacheLocks::new()),
                    allocator_locks: Arc::new(Ext4AllocatorLocks::new()),
                    #[cfg(feature = "perf-counters")]
                    io_counters: io_counters.clone(),
                },
                EXT4_CONFIG,
            )?,
            device,
            cache_epoch,
            write_sequence,
            physical_leases,
            block_versions,
            #[cfg(feature = "perf-counters")]
            io_counters,
            inode_runtime,
        })
    }

    pub(super) fn open_shared_reader(&self) -> Result<Self, Ext4Error> {
        #[cfg(feature = "perf-counters")]
        let io_counters = Arc::new(Ext4IoCounters::default());
        Ok(Self {
            fs: KernelExt4Fs::new_read_only(
                KernelDisk {
                    dev: self.device.clone(),
                    concurrent_bcache: true,
                    versioned_bcache: true,
                    concurrent_metadata: false,
                    cache_epoch: self.cache_epoch.clone(),
                    write_sequence: self.write_sequence.clone(),
                    physical_leases: self.physical_leases.clone(),
                    block_versions: self.block_versions.clone(),
                    bcache_locks: Arc::new(Ext4BcacheLocks::new()),
                    allocator_locks: Arc::new(Ext4AllocatorLocks::new()),
                    #[cfg(feature = "perf-counters")]
                    io_counters: io_counters.clone(),
                },
                EXT4_CONFIG,
            )?,
            device: self.device.clone(),
            cache_epoch: self.cache_epoch.clone(),
            write_sequence: self.write_sequence.clone(),
            physical_leases: self.physical_leases.clone(),
            block_versions: self.block_versions.clone(),
            #[cfg(feature = "perf-counters")]
            io_counters,
            inode_runtime: self.inode_runtime.clone(),
        })
    }

    fn flush_for_replica_visibility(&mut self) -> FsResult {
        self.fs.flush().map_err(map_ext4_error)
    }

    fn mapped_read_plan(
        &self,
        plan: Ext4MappedReadPlan,
        record_regular: bool,
        record_directory: bool,
    ) -> Option<Box<dyn BackendReadPlan>> {
        let record_fallback = || {
            if record_regular {
                perf::record_ext4_read_plan_fallback();
            }
            if record_directory {
                perf::record_ext4_directory_plan_fallback();
            }
        };
        if plan.block_size % EXT4_DEV_BSIZE != 0 {
            record_fallback();
            return None;
        }
        let device_blocks_per_fs_block = plan.block_size / EXT4_DEV_BSIZE;
        let mut runs = Vec::with_capacity(plan.runs.len());
        let mut data_runs = 0usize;
        let mut data_blocks = 0usize;
        let mut zero_runs = 0usize;
        let mut zero_blocks = 0usize;
        for run in plan.runs {
            let Some(buffer_start) = run.buffer_block.checked_mul(plan.block_size) else {
                record_fallback();
                return None;
            };
            let Some(byte_len) = run.block_count.checked_mul(plan.block_size) else {
                record_fallback();
                return None;
            };
            if buffer_start
                .checked_add(byte_len)
                .is_none_or(|end| end > plan.buffer_len)
            {
                record_fallback();
                return None;
            }
            let device_block = if let Some(fs_block) = run.fs_block {
                let Some(device_block) = fs_block
                    .checked_mul(device_blocks_per_fs_block as u64)
                    .and_then(|block| usize::try_from(block).ok())
                else {
                    record_fallback();
                    return None;
                };
                let device_blocks = run.block_count * device_blocks_per_fs_block;
                if device_block
                    .checked_add(device_blocks)
                    .is_none_or(|end| end > self.device.num_blocks() as usize)
                {
                    record_fallback();
                    return None;
                }
                data_runs += 1;
                data_blocks += run.block_count;
                Some(device_block)
            } else {
                zero_runs += 1;
                zero_blocks += run.block_count;
                None
            };
            runs.push(Ext4DeviceReadRun {
                buffer_start,
                byte_len,
                device_block,
            });
        }
        if record_regular {
            perf::record_ext4_read_plan_prepared(data_runs, data_blocks, zero_runs, zero_blocks);
        }
        if record_directory {
            perf::record_ext4_directory_plan_prepared(data_runs, data_blocks);
        }
        Some(Box::new(Ext4DeviceReadPlan {
            device: self.device.clone(),
            write_sequence: self.write_sequence.clone(),
            buffer_len: plan.buffer_len,
            read_offset: plan.read_offset,
            read_len: plan.read_len,
            runs,
            record_regular,
            record_directory,
        }))
    }

    fn prepare_mapped_write_plan(
        &mut self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Option<Ext4PreparedWritePlan> {
        let Ext4MappedWritePlan {
            block_size,
            buffer_len,
            write_offset,
            write_len,
            runs: mapped_runs,
        } = self.fs.plan_mapped_overwrite(ino, len, offset).ok()??;
        if block_size % EXT4_DEV_BSIZE != 0 {
            return None;
        }
        let device_blocks_per_fs_block = block_size / EXT4_DEV_BSIZE;
        let mut runs = Vec::with_capacity(mapped_runs.len());
        let mut fs_blocks = Vec::new();
        for run in mapped_runs {
            let buffer_start = run.buffer_block.checked_mul(block_size)?;
            let byte_len = run.block_count.checked_mul(block_size)?;
            if buffer_start
                .checked_add(byte_len)
                .is_none_or(|end| end > buffer_len)
            {
                return None;
            }
            let device_block = run
                .fs_block
                .checked_mul(device_blocks_per_fs_block as u64)
                .and_then(|block| usize::try_from(block).ok())?;
            let device_blocks = run.block_count.checked_mul(device_blocks_per_fs_block)?;
            if device_block
                .checked_add(device_blocks)
                .is_none_or(|end| end > self.device.num_blocks() as usize)
            {
                return None;
            }
            for delta in 0..run.block_count {
                fs_blocks.push(run.fs_block.checked_add(delta as u64)?);
            }
            runs.push(Ext4DeviceWriteRun {
                buffer_start,
                byte_len,
                device_block,
            });
        }
        Some(Ext4PreparedWritePlan {
            device: self.device.clone(),
            cache_epoch: self.cache_epoch.clone(),
            write_sequence: self.write_sequence.clone(),
            physical_leases: self.physical_leases.clone(),
            block_versions: self.block_versions.clone(),
            block_size,
            buffer_len,
            write_offset,
            write_len,
            runs,
            fs_blocks,
        })
    }

    fn invalidate_mapped_write_aliases(&mut self, fs_blocks: &[u64]) -> Option<usize> {
        self.fs.invalidate_clean_unreferenced_blocks(fs_blocks)
    }

    fn free_unlinked_inode(&mut self, ino: u32) -> FsResult<InodeRelease> {
        self.fs.free_unlinked_inode(ino).map_err(map_ext4_error)?;
        self.inode_runtime.remove(ino);
        Ok(InodeRelease::Freed)
    }

    fn stat_from_attr(
        &self,
        ino: u32,
        attr: lwext4_rust::FileAttr,
        inode_flags: u32,
        inode_flags_supported: u32,
    ) -> FileStat {
        FileStat {
            dev: attr.device,
            ino: attr.ino as u64,
            mode: attr.mode,
            nlink: attr.nlink as u32,
            uid: attr.uid,
            gid: attr.gid,
            rdev: self.inode_runtime.special_rdev(ino).unwrap_or(0),
            inode_flags,
            inode_flags_supported,
            size: attr.size,
            blksize: attr.block_size as u32,
            blocks: attr.blocks,
            atime_sec: attr.atime.as_secs(),
            atime_nsec: attr.atime.subsec_nanos(),
            btime_sec: attr.btime.as_secs(),
            btime_nsec: attr.btime.subsec_nanos(),
            mtime_sec: attr.mtime.as_secs(),
            mtime_nsec: attr.mtime.subsec_nanos(),
            ctime_sec: attr.ctime.as_secs(),
            ctime_nsec: attr.ctime.subsec_nanos(),
        }
    }
}

struct Ext4WriteLeaseTable {
    blocks: SpinNoIrqLock<BTreeSet<u64>>,
}

impl Ext4WriteLeaseTable {
    fn new() -> Self {
        Self {
            blocks: SpinNoIrqLock::new(BTreeSet::new()),
        }
    }

    fn reserve(self: &Arc<Self>, blocks: &[u64]) -> Option<Ext4WriteLbaLease> {
        let mut unique = BTreeSet::new();
        for &block in blocks {
            if !unique.insert(block) {
                return None;
            }
        }
        let mut reserved = self.blocks.lock();
        if unique.iter().any(|block| reserved.contains(block)) {
            return None;
        }
        reserved.extend(unique.iter().copied());
        drop(reserved);
        Some(Ext4WriteLbaLease {
            table: self.clone(),
            blocks: unique.into_iter().collect(),
        })
    }
}

struct Ext4WriteLbaLease {
    table: Arc<Ext4WriteLeaseTable>,
    blocks: Vec<u64>,
}

impl Drop for Ext4WriteLbaLease {
    fn drop(&mut self) {
        let mut reserved = self.table.blocks.lock();
        for block in self.blocks.drain(..) {
            assert!(
                reserved.remove(&block),
                "ext4 write LBA lease lost ownership"
            );
        }
    }
}

/// One read-only lwext4 core whose audited metadata operations may run in
/// parallel.
///
/// `Ext4Mount` intentionally remains non-`Sync`: writable instances still
/// require unique entry.  This wrapper is used only for a core opened with
/// `read_only = true` and concurrent bcache ownership callbacks.  Its shared
/// methods never mutate the fs/superblock, inode or directory payload; cache
/// bookkeeping and generation retirement are synchronized inside lwext4.
struct SharedExt4ReadCore {
    mount: Ext4Mount,
}

// SAFETY: shared access is restricted to the audited read-only methods above.
// The contained bcache owns its index/LBA sleeping locks, block-device counters
// are atomic, and every caller owns its C inode/directory reference objects.
// Backend destruction can occur only after the last Arc caller is gone, so
// drop-time mutable cleanup cannot overlap a shared method.
unsafe impl Sync for SharedExt4ReadCore {}

impl SharedExt4ReadCore {
    fn new(mount: Ext4Mount) -> Self {
        Self { mount }
    }

    fn mount(&self) -> &Ext4Mount {
        &self.mount
    }
}

/// One writable lwext4 core with a deliberately narrow shared-entry proof.
///
/// The containing `SleepRwLock` controls which accessor may be used. Shared
/// guards are restricted to audited create and per-inode metadata mutations;
/// every legacy method, cache invalidation, flush, and shutdown requires an
/// exclusive guard.
struct SharedExt4WriteCore {
    mount: UnsafeCell<Ext4Mount>,
}

// SAFETY: `shared()` is called only while the containing writer-core rwlock is
// read-held and only for audited shared mutations. Their caller-local inode and
// directory refs operate on VFS-leased objects; allocator and bcache shared
// state use the C callbacks installed on the canonical writer. `exclusive()`
// requires the rwlock write guard, excluding every shared call.
unsafe impl Sync for SharedExt4WriteCore {}

impl SharedExt4WriteCore {
    fn new(mount: Ext4Mount) -> Self {
        Self {
            mount: UnsafeCell::new(mount),
        }
    }

    fn shared(&self) -> &Ext4Mount {
        // SAFETY: the wrapper's public protocol permits only audited shared
        // mutation operations through this reference.
        unsafe { &*self.mount.get() }
    }

    fn exclusive(&mut self) -> &mut Ext4Mount {
        // SAFETY: callers can obtain `&mut self` only through the containing
        // `SleepRwLock` write guard, which excludes every `shared()` caller.
        unsafe { &mut *self.mount.get() }
    }
}

/// One selectively shared writable core and one lock-free-entry read-only core.
/// Cache generations retire stale reader buffers without a mount-wide reader
/// lifecycle writer phase.
pub(super) struct ConcurrentExt4Backend {
    writer: SleepRwLock<SharedExt4WriteCore>,
    shared_reader: SharedExt4ReadCore,
    inode_runtime: Arc<Ext4InodeRuntimeTable>,
    cache_generation: Ext4Sequence,
    write_leases: Arc<Ext4WriteLeaseTable>,
}

impl ConcurrentExt4Backend {
    pub(super) fn open(device: Arc<VirtIOBlock>) -> Result<Self, Ext4Error> {
        let cache_epoch = Arc::new(Ext4CacheEpoch::new());
        let write_sequence = Arc::new(Ext4Sequence::new());
        let physical_leases = Arc::new(Ext4PhysicalLeaseTable::new());
        let block_versions = Arc::new(Ext4BlockVersions::new());
        let writer = Ext4Mount::open(
            device.clone(),
            cache_epoch.clone(),
            write_sequence.clone(),
            physical_leases.clone(),
            block_versions.clone(),
        )?;
        let shared_reader = writer.open_shared_reader()?;
        let inode_runtime = writer.inode_runtime.clone();
        Ok(Self {
            writer: SleepRwLock::new(SharedExt4WriteCore::new(writer)),
            shared_reader: SharedExt4ReadCore::new(shared_reader),
            inode_runtime,
            cache_generation: Ext4Sequence::new(),
            write_leases: Arc::new(Ext4WriteLeaseTable::new()),
        })
    }

    fn lock_writer_exclusive(
        &self,
        op: BackendOp,
    ) -> SleepRwLockWriteGuard<'_, SharedExt4WriteCore> {
        let _ = op;
        match self.writer.try_write() {
            Some(core) => core,
            None => {
                #[cfg(feature = "perf-counters")]
                {
                    perf::record_mount_backend_contended_acquisition();
                    perf::record_backend_op_contended(op);
                }
                let wait_scope = perf::time_scope(perf::ProfilePoint::MountBackendContendedWait);
                #[cfg(feature = "perf-counters")]
                let op_wait_scope = perf::time_backend_op_wait(op);
                let core = self.writer.write();
                #[cfg(feature = "perf-counters")]
                drop(op_wait_scope);
                drop(wait_scope);
                core
            }
        }
    }

    fn lock_writer_shared(&self, op: BackendOp) -> SleepRwLockReadGuard<'_, SharedExt4WriteCore> {
        let _ = op;
        match self.writer.try_read() {
            Some(core) => core,
            None => {
                #[cfg(feature = "perf-counters")]
                {
                    perf::record_mount_backend_contended_acquisition();
                    perf::record_backend_op_contended(op);
                }
                let wait_scope = perf::time_scope(perf::ProfilePoint::MountBackendContendedWait);
                #[cfg(feature = "perf-counters")]
                let op_wait_scope = perf::time_backend_op_wait(op);
                let core = self.writer.read();
                #[cfg(feature = "perf-counters")]
                drop(op_wait_scope);
                drop(wait_scope);
                core
            }
        }
    }

    fn with_reader<V>(
        &self,
        op: BackendOp,
        _ino: u32,
        mut f: impl FnMut(&Ext4Mount, usize) -> V,
    ) -> V {
        let _ = op;
        loop {
            #[cfg(feature = "perf-counters")]
            perf::record_backend_op_call(op);
            let generation = self.cache_generation.stable_value();
            if self.cache_generation.value.load(Ordering::Acquire) != generation {
                continue;
            }
            let mount = self.shared_reader.mount();
            let hold_scope = perf::time_scope(perf::ProfilePoint::MountBackendHold);
            #[cfg(feature = "perf-counters")]
            let io_before = mount.io_counters.snapshot();
            #[cfg(feature = "perf-counters")]
            let op_hold_scope = perf::time_backend_op_hold(op);
            let result = f(mount, generation);
            #[cfg(feature = "perf-counters")]
            {
                drop(op_hold_scope);
                perf::record_backend_op_io(op, mount.io_counters.snapshot().delta_since(io_before));
            }
            drop(hold_scope);
            if op == BackendOp::InodeLifetime
                || self.cache_generation.value.load(Ordering::Acquire) == generation
            {
                return result;
            }
        }
    }

    fn mutate_inode_metadata(&self, ino: u32, mutation: Ext4InodeMetadataMutation) -> FsResult {
        let op = BackendOp::NamespaceMutation;
        perf::record_ext4_metadata_transaction_attempt();
        let result = self.mutate_shared(op, |writer| {
            // Count only callers that entered the canonical shared core, not
            // tasks sleeping on the rwlock or the cohort flush below.
            perf::record_ext4_metadata_transaction_begin();
            let result = writer.mutate_inode_metadata_shared(ino, mutation);
            perf::record_ext4_metadata_transaction_end();
            result
        });
        if result.is_ok() {
            // The canonical cache owns writeback accounting. These legacy
            // transaction fields remain zero until the flush-ticket counters
            // replace them in the next checkpoint.
            perf::record_ext4_metadata_transaction_commit(0, 0, 0, 0, 0);
        } else {
            perf::record_ext4_metadata_transaction_fallback();
        }
        result
    }

    fn with_writer_read<V>(&self, op: BackendOp, f: impl FnOnce(&mut Ext4Mount) -> V) -> V {
        let _ = op;
        #[cfg(feature = "perf-counters")]
        perf::record_backend_op_call(op);
        let mut writer = self.lock_writer_exclusive(op);
        let mount = writer.exclusive();
        let hold_scope = perf::time_scope(perf::ProfilePoint::MountBackendHold);
        #[cfg(feature = "perf-counters")]
        let io_before = mount.io_counters.snapshot();
        #[cfg(feature = "perf-counters")]
        let op_hold_scope = perf::time_backend_op_hold(op);
        let result = f(mount);
        #[cfg(feature = "perf-counters")]
        {
            drop(op_hold_scope);
            perf::record_backend_op_io(op, mount.io_counters.snapshot().delta_since(io_before));
        }
        drop(writer);
        drop(hold_scope);
        result
    }

    fn with_writer<V>(&self, op: BackendOp, f: impl FnOnce(&mut Ext4Mount) -> V) -> (V, FsResult) {
        let _ = op;
        #[cfg(feature = "perf-counters")]
        perf::record_backend_op_call(op);
        let mut writer = self.lock_writer_exclusive(op);
        let mount = writer.exclusive();
        let generation_write = self.cache_generation.begin_write();
        let hold_scope = perf::time_scope(perf::ProfilePoint::MountBackendHold);
        #[cfg(feature = "perf-counters")]
        let io_before = mount.io_counters.snapshot();
        #[cfg(feature = "perf-counters")]
        let op_hold_scope = perf::time_backend_op_hold(op);
        let result = f(mount);
        let visible = mount.flush_for_replica_visibility();
        #[cfg(feature = "perf-counters")]
        {
            drop(op_hold_scope);
            perf::record_backend_op_io(op, mount.io_counters.snapshot().delta_since(io_before));
        }
        drop(writer);
        drop(generation_write);
        drop(hold_scope);
        (result, visible)
    }

    fn mutate_shared<T>(
        &self,
        op: BackendOp,
        f: impl FnOnce(&Ext4Mount) -> FsResult<T>,
    ) -> FsResult<T> {
        let _ = op;
        #[cfg(feature = "perf-counters")]
        perf::record_backend_op_call(op);
        let generation_write = self.cache_generation.begin_write();
        let hold_scope = perf::time_scope(perf::ProfilePoint::MountBackendHold);

        let writer = self.lock_writer_shared(op);
        let mount = writer.shared();
        #[cfg(feature = "perf-counters")]
        let io_before = mount.io_counters.snapshot();
        #[cfg(feature = "perf-counters")]
        let op_hold_scope = perf::time_backend_op_hold(op);
        let result = f(mount);
        drop(writer);

        // The phase-fair write acquisition waits for every shared mutation
        // that already entered the shared phase. One caller can therefore
        // flush the complete ready dirty set for the cohort; later writers
        // observe an empty list instead of repeating device I/O.
        let mut writer = self.lock_writer_exclusive(op);
        let mount = writer.exclusive();
        let visible = mount.flush_for_replica_visibility();
        #[cfg(feature = "perf-counters")]
        {
            drop(op_hold_scope);
            perf::record_backend_op_io(op, mount.io_counters.snapshot().delta_since(io_before));
        }
        drop(writer);
        drop(generation_write);
        drop(hold_scope);
        match result {
            Ok(value) => visible.map(|()| value),
            Err(err) => Err(err),
        }
    }

    fn mutate<T>(
        &self,
        op: BackendOp,
        f: impl FnOnce(&mut Ext4Mount) -> FsResult<T>,
    ) -> FsResult<T> {
        let (result, visible) = self.with_writer(op, f);
        match result {
            Ok(value) => visible.map(|()| value),
            Err(err) => Err(err),
        }
    }

    fn try_mutate<T>(
        &self,
        op: BackendOp,
        f: impl FnOnce(&mut Ext4Mount) -> FsResult<T>,
    ) -> Option<FsResult<T>> {
        let _ = op;
        let mut writer = self.writer.try_write()?;
        let mount = writer.exclusive();
        let generation_write = self.cache_generation.begin_write();
        #[cfg(feature = "perf-counters")]
        {
            perf::record_backend_op_call(op);
            perf::record_backend_try_successful_call();
        }
        #[cfg(feature = "perf-counters")]
        let io_before = mount.io_counters.snapshot();
        #[cfg(feature = "perf-counters")]
        let op_hold_scope = perf::time_backend_op_hold(op);
        let result = f(mount);
        let visible = mount.flush_for_replica_visibility();
        #[cfg(feature = "perf-counters")]
        {
            drop(op_hold_scope);
            perf::record_backend_op_io(op, mount.io_counters.snapshot().delta_since(io_before));
        }
        drop(writer);
        drop(generation_write);
        Some(match result {
            Ok(value) => visible.map(|()| value),
            Err(err) => Err(err),
        })
    }
}

struct Ext4DeviceReadRun {
    buffer_start: usize,
    byte_len: usize,
    device_block: Option<usize>,
}

struct Ext4DeviceWriteRun {
    buffer_start: usize,
    byte_len: usize,
    device_block: usize,
}

struct Ext4PreparedWritePlan {
    device: Arc<VirtIOBlock>,
    cache_epoch: Arc<Ext4CacheEpoch>,
    write_sequence: Arc<Ext4Sequence>,
    physical_leases: Arc<Ext4PhysicalLeaseTable>,
    block_versions: Arc<Ext4BlockVersions>,
    block_size: usize,
    buffer_len: usize,
    write_offset: usize,
    write_len: usize,
    runs: Vec<Ext4DeviceWriteRun>,
    fs_blocks: Vec<u64>,
}

impl Ext4PreparedWritePlan {
    fn publish(self, lease: Ext4WriteLbaLease) -> Ext4DeviceWritePlan {
        Ext4DeviceWritePlan {
            device: self.device,
            cache_epoch: self.cache_epoch,
            write_sequence: self.write_sequence,
            physical_leases: self.physical_leases,
            block_versions: self.block_versions,
            block_size: self.block_size,
            buffer_len: self.buffer_len,
            write_offset: self.write_offset,
            write_len: self.write_len,
            runs: self.runs,
            _lease: lease,
        }
    }
}

struct Ext4DeviceWritePlan {
    device: Arc<VirtIOBlock>,
    cache_epoch: Arc<Ext4CacheEpoch>,
    write_sequence: Arc<Ext4Sequence>,
    physical_leases: Arc<Ext4PhysicalLeaseTable>,
    block_versions: Arc<Ext4BlockVersions>,
    block_size: usize,
    buffer_len: usize,
    write_offset: usize,
    write_len: usize,
    runs: Vec<Ext4DeviceWriteRun>,
    _lease: Ext4WriteLbaLease,
}

struct Ext4DeviceReadPlan {
    device: Arc<VirtIOBlock>,
    write_sequence: Arc<Ext4Sequence>,
    buffer_len: usize,
    read_offset: usize,
    read_len: usize,
    runs: Vec<Ext4DeviceReadRun>,
    record_regular: bool,
    record_directory: bool,
}

struct Ext4InlineReadPlan {
    data: Vec<u8>,
}

struct Ext4DirectoryReadPlan {
    raw_plan: Box<dyn BackendReadPlan>,
    raw_len: usize,
    start_offset: u64,
    block_size: usize,
    has_file_type: bool,
}

impl Ext4DirectoryReadPlan {
    fn execute_snapshot(self: Box<Self>) -> FsResult<BackendDirectorySnapshot> {
        let Self {
            raw_plan,
            raw_len,
            start_offset,
            block_size,
            has_file_type,
        } = *self;
        let mut raw = vec![0u8; raw_len];
        if raw_plan.execute(&mut raw) != raw_len {
            return Err(FsError::Io);
        }
        let mut entries = Vec::new();
        let mut cursor = 0usize;
        let mut absolute = start_offset;
        while cursor < raw.len() {
            let offset_in_block = absolute as usize % block_size;
            if offset_in_block % 4 != 0
                || offset_in_block > block_size.saturating_sub(8)
                || raw.len() - cursor < 8
            {
                return Err(FsError::Io);
            }
            let ino = u32::from_le_bytes(raw[cursor..cursor + 4].try_into().unwrap());
            let record_len =
                u16::from_le_bytes(raw[cursor + 4..cursor + 6].try_into().unwrap()) as usize;
            let name_len = if has_file_type {
                raw[cursor + 6] as usize
            } else {
                raw[cursor + 6] as usize | ((raw[cursor + 7] as usize) << 8)
            };
            if record_len < 8
                || record_len % 4 != 0
                || offset_in_block
                    .checked_add(record_len)
                    .is_none_or(|end| end > block_size)
                || cursor
                    .checked_add(record_len)
                    .is_none_or(|end| end > raw.len())
                || name_len > record_len - 8
            {
                return Err(FsError::Io);
            }
            if ino != 0 {
                let d_type = if has_file_type {
                    match raw[cursor + 7] as u32 {
                        EXT4_DE_DIR => DT_DIR,
                        EXT4_DE_REG_FILE => DT_REG,
                        EXT4_DE_SYMLINK => DT_LNK,
                        EXT4_DE_CHRDEV => DT_CHR,
                        EXT4_DE_BLKDEV => DT_BLK,
                        EXT4_DE_FIFO => DT_FIFO,
                        EXT4_DE_SOCK => DT_SOCK,
                        _ => DT_UNKNOWN,
                    }
                } else {
                    DT_UNKNOWN
                };
                let name_start = cursor + 8;
                perf::record_ext4_dirent_name(name_len, false);
                entries.push(BackendDirectoryEntry {
                    offset: absolute,
                    ino,
                    d_type,
                    name_start,
                    name_len,
                });
            }
            cursor += record_len;
            absolute = absolute.checked_add(record_len as u64).ok_or(FsError::Io)?;
        }
        Ok(BackendDirectorySnapshot {
            entries,
            end_offset: absolute,
            storage: raw,
        })
    }
}

impl BackendDirectoryReadPlan for Ext4DirectoryReadPlan {
    fn execute(self: Box<Self>) -> FsResult<BackendDirectorySnapshot> {
        self.execute_snapshot()
    }
}

impl BackendReadPlan for Ext4InlineReadPlan {
    fn execute(self: Box<Self>, buf: &mut [u8]) -> usize {
        let read_len = self.data.len().min(buf.len());
        buf[..read_len].copy_from_slice(&self.data[..read_len]);
        read_len
    }
}

impl BackendReadPlan for Ext4DeviceReadPlan {
    fn execute(self: Box<Self>, buf: &mut [u8]) -> usize {
        if buf.len() < self.read_len {
            return 0;
        }
        let mut bounce = (self.read_offset != 0 || self.buffer_len != self.read_len)
            .then(|| vec![0u8; self.buffer_len]);
        let plan_buf = match bounce.as_deref_mut() {
            Some(bounce) => bounce,
            None => &mut buf[..self.buffer_len],
        };
        for run in &self.runs {
            let run_buf = &mut plan_buf[run.buffer_start..run.buffer_start + run.byte_len];
            if let Some(device_block) = run.device_block {
                self.write_sequence.read_stable(|| {
                    let io = self
                        .device
                        .read_blocks_versioned_fill_for_file_plan(device_block, run_buf);
                    if self.record_regular {
                        perf::record_ext4_read_plan_direct_io(
                            io.device_calls,
                            io.device_blocks,
                            io.device_blocks * EXT4_DEV_BSIZE,
                        );
                    }
                    if self.record_directory {
                        perf::record_ext4_directory_plan_direct_io(
                            io.device_calls,
                            io.device_blocks,
                            io.device_blocks * EXT4_DEV_BSIZE,
                        );
                    }
                });
            } else {
                run_buf.fill(0);
            }
        }
        if let Some(bounce) = bounce {
            buf[..self.read_len]
                .copy_from_slice(&bounce[self.read_offset..self.read_offset + self.read_len]);
        }
        if self.record_regular {
            perf::record_ext4_read_plan_executed(self.read_len);
        }
        if self.record_directory {
            perf::record_ext4_directory_plan_executed(self.read_len);
        }
        self.read_len
    }
}

impl BackendWritePlan for Ext4DeviceWritePlan {
    fn execute(self: Box<Self>, buf: &[u8]) -> usize {
        if buf.len() != self.write_len
            || self.block_size == 0
            || self.buffer_len % self.block_size != 0
        {
            return 0;
        }

        let physical_blocks = self.runs.iter().flat_map(|run| {
            let blocks = run.byte_len / EXT4_DEV_BSIZE;
            run.device_block as u64..run.device_block as u64 + blocks as u64
        });
        let _physical = self.physical_leases.reserve_wait(physical_blocks);

        let mut bounce = (self.write_offset != 0 || self.buffer_len != self.write_len)
            .then(|| vec![0u8; self.buffer_len]);
        let mut rmw_calls = 0usize;
        let mut rmw_blocks = 0usize;
        if let Some(bounce) = bounce.as_deref_mut() {
            for run in &self.runs {
                let run_buf = &mut bounce[run.buffer_start..run.buffer_start + run.byte_len];
                let io = self.write_sequence.read_stable(|| {
                    self.device
                        .read_blocks_versioned_fill_for_file_plan(run.device_block, run_buf)
                });
                rmw_calls += io.device_calls;
                rmw_blocks += io.device_blocks;
            }
            bounce[self.write_offset..self.write_offset + self.write_len].copy_from_slice(buf);
        }
        perf::record_ext4_write_plan_rmw_read(rmw_calls, rmw_blocks, rmw_blocks * EXT4_DEV_BSIZE);

        // One sequence epoch covers every fragmented physical run. Clean read
        // plans that overlap this overwrite either finish before the epoch or
        // discard their private copy and retry after all runs are durable.
        let _write = self.write_sequence.begin_write();
        let plan_buf = bounce.as_deref().unwrap_or(buf);
        let mut direct_calls = 0usize;
        let mut direct_blocks = 0usize;
        for run in &self.runs {
            let io = self.device.write_blocks_for_file_plan(
                run.device_block,
                &plan_buf[run.buffer_start..run.buffer_start + run.byte_len],
            );
            self.block_versions
                .bump_range(run.device_block as u64, run.byte_len / EXT4_DEV_BSIZE);
            direct_calls += io.device_calls;
            direct_blocks += io.device_blocks;
        }
        if direct_blocks != 0 {
            self.cache_epoch.advance();
        }
        perf::record_ext4_write_plan_executed(
            self.write_len,
            direct_calls,
            direct_blocks,
            direct_blocks * EXT4_DEV_BSIZE,
        );
        self.write_len
    }
}

/// Audited operations available through the shared read-only lwext4 core.
///
/// Every C inode, directory iterator, extent path, and lookup result created
/// below is caller-local.  The only shared mutation is bcache ownership
/// bookkeeping, which is protected by the callbacks installed on this core.
impl Ext4Mount {
    fn statfs_shared(&self) -> FileSystemStat {
        match self.fs.stat() {
            Ok(st) => FileSystemStat {
                magic: 0xEF53,
                block_size: st.block_size as u64,
                blocks: st.blocks_count,
                free_blocks: st.free_blocks_count,
                available_blocks: st.free_blocks_count,
                files: st.inodes_count as u64,
                free_files: st.free_inodes_count as u64,
                max_name_len: 255,
                flags: 0,
            },
            Err(_) => FileSystemStat {
                magic: 0xEF53,
                block_size: 4096,
                blocks: 0,
                free_blocks: 0,
                available_blocks: 0,
                files: 4096,
                free_files: 2048,
                max_name_len: 255,
                flags: 0,
            },
        }
    }

    fn lookup_component_from_shared(
        &self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)> {
        let mut result = self
            .fs
            .lookup(parent_ino, component)
            .map_err(map_ext4_error)?;
        let entry = result.entry();
        Ok((entry.ino(), into_node_kind(entry.inode_type())))
    }

    fn inode_flags_shared(&self, ino: u32) -> FsResult<u32> {
        self.fs.inode_flags(ino).map_err(map_ext4_error)
    }

    fn stat_shared(&self, ino: u32) -> FsResult<FileStat> {
        let mut attr = lwext4_rust::FileAttr::default();
        {
            let _profile_scope = perf::time_scope(perf::ProfilePoint::Ext4StatGetAttr);
            self.fs.get_attr(ino, &mut attr).map_err(map_ext4_error)?;
        }
        let inode_flags = {
            let _profile_scope = perf::time_scope(perf::ProfilePoint::Ext4StatInodeFlags);
            self.fs.inode_flags(ino).map_err(map_ext4_error)?
        };
        Ok(self.stat_from_attr(ino, attr, inode_flags, FS_STATX_ATTR_FLAGS))
    }

    fn stat_basic_shared(&self, ino: u32) -> FsResult<FileStat> {
        let mut attr = lwext4_rust::FileAttr::default();
        {
            let _profile_scope = perf::time_scope(perf::ProfilePoint::Ext4StatGetAttr);
            self.fs.get_attr(ino, &mut attr).map_err(map_ext4_error)?;
        }
        Ok(self.stat_from_attr(ino, attr, 0, 0))
    }

    /// Audited shared mutation used only through `SharedExt4WriteCore`.
    /// The caller owns the parent-directory VFS mutation lease; allocator and
    /// bcache state are protected by the canonical writer's callbacks.
    fn create_file_shared(&self, parent_ino: u32, leaf_name: &str) -> FsResult<u32> {
        self.fs
            .create(parent_ino, leaf_name, InodeType::RegularFile, 0o644)
            .map_err(map_ext4_error)
    }

    fn create_node_shared(
        &self,
        parent_ino: u32,
        leaf_name: &str,
        kind: FsNodeKind,
        mode: u32,
        rdev: u64,
    ) -> FsResult<u32> {
        let inode_type = match kind {
            FsNodeKind::RegularFile => InodeType::RegularFile,
            FsNodeKind::Fifo => InodeType::Fifo,
            FsNodeKind::CharacterDevice => InodeType::CharacterDevice,
            FsNodeKind::BlockDevice => InodeType::BlockDevice,
            FsNodeKind::Socket => InodeType::Socket,
            _ => return Err(FsError::InvalidInput),
        };
        let ino = self
            .fs
            .create(parent_ino, leaf_name, inode_type, mode)
            .map_err(map_ext4_error)?;
        if matches!(kind, FsNodeKind::CharacterDevice | FsNodeKind::BlockDevice) {
            // UNFINISHED: The vendored wrapper does not expose persistent
            // device major/minor payloads yet. Runtime state is independently
            // sharded and safe for different newly allocated inodes.
            self.inode_runtime.set_special_rdev(ino, rdev);
        }
        Ok(ino)
    }

    fn create_dir_shared(&self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32> {
        self.fs
            .create(parent_ino, leaf_name, InodeType::Directory, mode)
            .map_err(map_ext4_error)
    }

    fn set_mode_shared(&self, ino: u32, mode: u32) -> FsResult {
        let mut attr = lwext4_rust::FileAttr::default();
        self.fs.get_attr(ino, &mut attr).map_err(map_ext4_error)?;
        self.fs
            .set_mode(ino, (attr.mode & !0o7777) | (mode & 0o7777))
            .map_err(map_ext4_error)
    }

    fn set_owner_shared(&self, ino: u32, uid: Option<u32>, gid: Option<u32>) -> FsResult {
        let mut attr = lwext4_rust::FileAttr::default();
        self.fs.get_attr(ino, &mut attr).map_err(map_ext4_error)?;
        let uid = uid.unwrap_or(attr.uid);
        let gid = gid.unwrap_or(attr.gid);
        if uid > u16::MAX as u32 || gid > u16::MAX as u32 {
            // UNFINISHED: The wrapper currently exposes only the low 16-bit
            // ext4 uid/gid fields, not Linux's complete 32-bit ids.
            return Err(FsError::InvalidInput);
        }
        self.fs
            .set_owner(ino, uid as u16, gid as u16)
            .map_err(map_ext4_error)
    }

    /// Audited per-inode mutation used through `SharedExt4WriteCore` while the
    /// VFS owns that inode's metadata-write lease. Distinct inodes may enter
    /// concurrently; inode-table blocks and cache entries use the canonical
    /// writer's narrow lwext4 callback locks.
    fn mutate_inode_metadata_shared(
        &self,
        ino: u32,
        mutation: Ext4InodeMetadataMutation,
    ) -> FsResult {
        match mutation {
            Ext4InodeMetadataMutation::Times {
                atime,
                mtime,
                ctime,
            } => self
                .fs
                .set_times(
                    ino,
                    atime.map(FileTimestamp::to_duration),
                    mtime.map(FileTimestamp::to_duration),
                    Some(ctime.to_duration()),
                )
                .map_err(map_ext4_error),
            Ext4InodeMetadataMutation::Mode(mode) => self.set_mode_shared(ino, mode),
            Ext4InodeMetadataMutation::Owner { uid, gid } => self.set_owner_shared(ino, uid, gid),
            Ext4InodeMetadataMutation::Flags(flags) => {
                self.fs.set_inode_flags(ino, flags).map_err(map_ext4_error)
            }
        }
    }

    fn create_node_with_owner_shared(
        &self,
        parent_ino: u32,
        leaf_name: &str,
        kind: FsNodeKind,
        mode: u32,
        rdev: u64,
        uid: u32,
        gid: u32,
    ) -> FsResult<u32> {
        let parent = self.stat_basic_shared(parent_ino)?;
        let ino = self.create_node_shared(parent_ino, leaf_name, kind, mode, rdev)?;
        let gid = if parent.mode & 0o2000 != 0 {
            parent.gid
        } else {
            gid
        };
        self.set_owner_shared(ino, Some(uid), Some(gid))?;
        self.set_mode_shared(ino, mode)?;
        Ok(ino)
    }

    fn readlink_shared(&self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        self.fs.read_at(ino, buf, 0).map_err(map_ext4_error)
    }

    fn prepare_readlink_plan_shared(
        &self,
        ino: u32,
        len: usize,
    ) -> Option<Box<dyn BackendReadPlan>> {
        match self.fs.plan_symlink_read(ino, len).ok()?? {
            Ext4SymlinkReadPlan::Inline(data) => Some(Box::new(Ext4InlineReadPlan { data })),
            Ext4SymlinkReadPlan::Mapped(plan) => self.mapped_read_plan(plan, false, false),
        }
    }

    fn prepare_read_plan_shared(
        &self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Option<Box<dyn BackendReadPlan>> {
        perf::record_ext4_read_plan_attempt();
        let Ok(Some(plan)) = self.fs.plan_read(ino, len, offset) else {
            perf::record_ext4_read_plan_fallback();
            return None;
        };
        self.mapped_read_plan(plan, true, false)
    }

    fn read_at_shared(&self, ino: u32, buf: &mut [u8], offset: u64) -> usize {
        let _profile_scope = perf::time_scope(perf::ProfilePoint::Ext4Read);
        self.fs.read_at(ino, buf, offset).expect("ext4 read failed")
    }

    fn prepare_directory_read_plan_shared(
        &self,
        ino: u32,
        offset: u64,
    ) -> Option<Box<dyn BackendDirectoryReadPlan>> {
        perf::record_ext4_directory_plan_attempt();
        let LwExt4DirectoryReadPlan {
            mapped,
            start_offset,
            has_file_type,
        } = match self.fs.plan_directory_read(ino, offset) {
            Ok(Some(plan)) => plan,
            _ => {
                perf::record_ext4_directory_plan_fallback();
                return None;
            }
        };
        let raw_len = mapped.read_len;
        let block_size = mapped.block_size;
        let raw_plan = self.mapped_read_plan(mapped, false, true)?;
        Some(Box::new(Ext4DirectoryReadPlan {
            raw_plan,
            raw_len,
            start_offset,
            block_size,
            has_file_type,
        }))
    }

    fn read_dirent64_shared(
        &self,
        ino: u32,
        offset: u64,
        buf: &mut [u8],
    ) -> FsResult<(usize, u64)> {
        let mut reader = self.fs.read_dir(ino, offset).map_err(map_ext4_error)?;
        let mut written = 0usize;
        let mut next_offset = offset;

        loop {
            let (entry_start, d_reclen) = {
                let Some(entry) = reader.current() else {
                    break;
                };
                let d_ino = entry.ino() as u64;
                let d_type = into_linux_dtype(entry.inode_type());
                let name = entry.name();
                perf::record_ext4_dirent_name(name.len(), false);
                let d_reclen = align_up(
                    LINUX_DIRENT64_HEADER_SIZE + name.len() + 1,
                    LINUX_DIRENT64_ALIGN,
                );

                if d_reclen > buf.len().saturating_sub(written) {
                    if written == 0 {
                        return Err(FsError::InvalidInput);
                    }
                    break;
                }

                let entry_start = written;
                let entry_buf = &mut buf[entry_start..entry_start + d_reclen];
                entry_buf.fill(0);
                entry_buf[0..8].copy_from_slice(&d_ino.to_ne_bytes());
                entry_buf[16..18].copy_from_slice(&(d_reclen as u16).to_ne_bytes());
                entry_buf[18] = d_type;
                entry_buf[LINUX_DIRENT64_HEADER_SIZE..LINUX_DIRENT64_HEADER_SIZE + name.len()]
                    .copy_from_slice(name);

                (entry_start, d_reclen)
            };

            reader.step().map_err(map_ext4_error)?;
            next_offset = reader.offset();
            buf[entry_start + 8..entry_start + 16]
                .copy_from_slice(&(next_offset as i64).to_ne_bytes());
            written += d_reclen;
        }

        Ok((written, next_offset))
    }

    fn list_root_names_shared(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut reader = self
            .fs
            .read_dir(EXT4_ROOT_INO, 0)
            .expect("failed to iterate ext4 root directory");
        while let Some(entry) = reader.current() {
            let name = str::from_utf8(entry.name()).unwrap_or("<invalid>");
            if name != "." && name != ".." {
                names.push(name.to_string());
            }
            reader.step().expect("failed to advance ext4 dir iterator");
        }
        names
    }
}

impl LegacyFileSystemBackend for Ext4Mount {
    #[cfg(feature = "perf-counters")]
    fn io_snapshot(&self) -> BackendIoSnapshot {
        self.io_counters.snapshot()
    }

    fn statfs(&mut self) -> FileSystemStat {
        self.statfs_shared()
    }

    fn lookup_component_from(
        &mut self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)> {
        self.lookup_component_from_shared(parent_ino, component)
    }

    fn create_file(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult<u32> {
        self.create_file_shared(parent_ino, leaf_name)
    }

    fn create_node(
        &mut self,
        parent_ino: u32,
        leaf_name: &str,
        kind: FsNodeKind,
        mode: u32,
        rdev: u64,
    ) -> FsResult<u32> {
        self.create_node_shared(parent_ino, leaf_name, kind, mode, rdev)
    }

    fn create_dir(&mut self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32> {
        self.create_dir_shared(parent_ino, leaf_name, mode)
    }

    fn link(&mut self, parent_ino: u32, leaf_name: &str, child_ino: u32) -> FsResult {
        self.fs
            .link(parent_ino, leaf_name, child_ino)
            .map_err(map_ext4_error)
    }

    fn symlink(&mut self, parent_ino: u32, leaf_name: &str, target: &[u8]) -> FsResult {
        let ino = self
            .fs
            .create(parent_ino, leaf_name, InodeType::Symlink, 0o777)
            .map_err(map_ext4_error)?;
        match self.fs.set_symlink(ino, target).map_err(map_ext4_error) {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = self.fs.unlink(parent_ino, leaf_name);
                Err(err)
            }
        }
    }

    fn unlink(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult {
        let mut lookup = self
            .fs
            .lookup(parent_ino, leaf_name)
            .map_err(map_ext4_error)?;
        let child_ino = lookup.entry().ino();
        // Publish the unlink-in-progress state before dropping the lookup.
        // A concurrent VfsFile retain must either be counted in defer_free or
        // fail with ENOENT; it must never pin an inode after the decision and
        // then race with its physical removal.
        let defer_free = self.inode_runtime.begin_unlink(child_ino);
        drop(lookup);

        // UNFINISHED: Linux also keeps opened directories alive across unlink.
        // This ext4 path currently defers final free only for non-directory
        // inodes, which is enough for mkstemp/unlink/fstat file workloads.
        if defer_free {
            let deferred = match self.fs.unlink_defer_free(parent_ino, leaf_name) {
                Ok(deferred) => deferred,
                Err(err) => {
                    self.inode_runtime.abort_unlink(child_ino);
                    return Err(map_ext4_error(err));
                }
            };
            if let Some(ino) = deferred {
                let free_now = self.inode_runtime.finish_unlink(ino, true, true);
                if free_now {
                    self.free_unlinked_inode(ino)?;
                }
            } else {
                // Another hard link still owns the inode.
                self.inode_runtime.finish_unlink(child_ino, false, true);
            }
        } else {
            if let Err(err) = self.fs.unlink(parent_ino, leaf_name) {
                self.inode_runtime.abort_unlink(child_ino);
                return Err(map_ext4_error(err));
            }
            let mut attr = lwext4_rust::FileAttr::default();
            let inode_exists = self.fs.get_attr(child_ino, &mut attr).is_ok();
            self.inode_runtime
                .finish_unlink(child_ino, false, inode_exists);
        }
        Ok(())
    }

    fn rename(&mut self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult {
        self.fs
            .rename(src_dir, src_name, dst_dir, dst_name)
            .map_err(map_ext4_error)
    }

    fn exchange(&mut self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult {
        self.fs
            .exchange(src_dir, src_name, dst_dir, dst_name)
            .map_err(map_ext4_error)
    }

    fn set_len(&mut self, ino: u32, len: u64) -> FsResult {
        self.fs.set_len(ino, len).map_err(map_ext4_error)
    }

    fn sync(&mut self, _ino: u32, _data_only: bool) -> FsResult {
        self.fs.flush().map_err(map_ext4_error)
    }

    fn shutdown(&mut self) -> FsResult {
        self.fs.shutdown_clean().map_err(map_ext4_error)
    }

    fn set_times(
        &mut self,
        ino: u32,
        atime: Option<FileTimestamp>,
        mtime: Option<FileTimestamp>,
        ctime: FileTimestamp,
    ) -> FsResult {
        self.fs
            .set_times(
                ino,
                atime.map(FileTimestamp::to_duration),
                mtime.map(FileTimestamp::to_duration),
                Some(ctime.to_duration()),
            )
            .map_err(map_ext4_error)
    }

    fn set_mode(&mut self, ino: u32, mode: u32) -> FsResult {
        self.set_mode_shared(ino, mode)
    }

    fn inode_flags(&mut self, ino: u32) -> FsResult<u32> {
        self.inode_flags_shared(ino)
    }

    fn set_inode_flags(&mut self, ino: u32, flags: u32) -> FsResult {
        self.fs.set_inode_flags(ino, flags).map_err(map_ext4_error)
    }

    fn set_owner(&mut self, ino: u32, uid: Option<u32>, gid: Option<u32>) -> FsResult {
        self.set_owner_shared(ino, uid, gid)
    }

    fn retain_inode(&mut self, ino: u32) -> FsResult {
        // VfsFile open lifetime pins the inode after unlink. The backend checks
        // existence here so stale VfsNodeId values do not create open counts.
        let mut attr = lwext4_rust::FileAttr::default();
        self.fs.get_attr(ino, &mut attr).map_err(map_ext4_error)?;
        if attr.nlink == 0 {
            return Err(FsError::NotFound);
        }
        if self.inode_runtime.retain(ino) {
            Ok(())
        } else {
            Err(FsError::NotFound)
        }
    }

    fn release_inode(&mut self, ino: u32) -> FsResult<InodeRelease> {
        if self.inode_runtime.prepare_release(ino) == Ext4RuntimeRelease::FreeUnlinked {
            // The final open reference is the point where an unlinked-but-open
            // inode can be physically freed from the ext4 backend. Do not
            // retain the Rust runtime lock across metadata or device I/O.
            return self.free_unlinked_inode(ino);
        }
        Ok(InodeRelease::Retained)
    }

    fn stat(&mut self, ino: u32) -> FsResult<FileStat> {
        self.stat_shared(ino)
    }

    fn stat_basic(&mut self, ino: u32) -> FsResult<FileStat> {
        self.stat_basic_shared(ino)
    }

    fn readlink(&mut self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        self.readlink_shared(ino, buf)
    }

    fn prepare_readlink_plan(&mut self, ino: u32, len: usize) -> Option<Box<dyn BackendReadPlan>> {
        self.prepare_readlink_plan_shared(ino, len)
    }

    fn prepare_read_plan(
        &mut self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Option<Box<dyn BackendReadPlan>> {
        self.prepare_read_plan_shared(ino, offset, len)
    }

    fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> usize {
        self.read_at_shared(ino, buf, offset)
    }

    fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> usize {
        self.fs
            .write_at(ino, buf, offset)
            .expect("ext4 write failed")
    }

    fn prepare_directory_read_plan(
        &mut self,
        ino: u32,
        offset: u64,
    ) -> Option<Box<dyn BackendDirectoryReadPlan>> {
        self.prepare_directory_read_plan_shared(ino, offset)
    }

    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        self.read_dirent64_shared(ino, offset, buf)
    }

    fn list_root_names(&mut self) -> Vec<String> {
        self.list_root_names_shared()
    }
}

impl ConcurrentFileSystemBackend for ConcurrentExt4Backend {
    fn root_ino(&self) -> u32 {
        EXT4_ROOT_INO
    }

    fn overlay_real_node(&self, _ino: u32) -> Option<super::vfs::VfsNodeId> {
        None
    }

    fn statfs(&self) -> FileSystemStat {
        // The read-only core's in-memory superblock is a mount-time snapshot;
        // free inode/block counters are maintained by the writable core.
        self.with_writer_read(BackendOp::StatFull, |writer| writer.statfs_shared())
    }

    fn lookup_component_from(
        &self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)> {
        self.with_reader(BackendOp::Lookup, parent_ino, |reader, _| {
            reader.lookup_component_from_shared(parent_ino, component)
        })
    }

    fn create_node_with_owner(
        &self,
        parent_ino: u32,
        leaf_name: &str,
        kind: FsNodeKind,
        mode: u32,
        rdev: u64,
        uid: u32,
        gid: u32,
    ) -> FsResult<u32> {
        self.mutate_shared(BackendOp::NamespaceMutation, |writer| {
            writer.create_node_with_owner_shared(parent_ino, leaf_name, kind, mode, rdev, uid, gid)
        })
    }

    fn create_file(&self, parent_ino: u32, leaf_name: &str) -> FsResult<u32> {
        self.mutate_shared(BackendOp::NamespaceMutation, |writer| {
            writer.create_file_shared(parent_ino, leaf_name)
        })
    }

    fn create_node(
        &self,
        parent_ino: u32,
        leaf_name: &str,
        kind: FsNodeKind,
        mode: u32,
        rdev: u64,
    ) -> FsResult<u32> {
        self.mutate_shared(BackendOp::NamespaceMutation, |writer| {
            writer.create_node_shared(parent_ino, leaf_name, kind, mode, rdev)
        })
    }

    fn create_dir(&self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32> {
        self.mutate_shared(BackendOp::NamespaceMutation, |writer| {
            writer.create_dir_shared(parent_ino, leaf_name, mode)
        })
    }

    fn link(&self, parent_ino: u32, leaf_name: &str, child_ino: u32) -> FsResult {
        self.mutate(BackendOp::NamespaceMutation, |writer| {
            writer.link(parent_ino, leaf_name, child_ino)
        })
    }

    fn symlink(&self, parent_ino: u32, leaf_name: &str, target: &[u8]) -> FsResult {
        self.mutate(BackendOp::NamespaceMutation, |writer| {
            writer.symlink(parent_ino, leaf_name, target)
        })
    }

    fn unlink(&self, parent_ino: u32, leaf_name: &str) -> FsResult {
        self.mutate(BackendOp::NamespaceMutation, |writer| {
            writer.unlink(parent_ino, leaf_name)
        })
    }

    fn rename(&self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult {
        self.mutate(BackendOp::NamespaceMutation, |writer| {
            writer.rename(src_dir, src_name, dst_dir, dst_name)
        })
    }

    fn exchange(&self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult {
        self.mutate(BackendOp::NamespaceMutation, |writer| {
            writer.exchange(src_dir, src_name, dst_dir, dst_name)
        })
    }

    fn check_write_at(&self, _ino: u32, _offset: u64, _len: usize) -> FsResult {
        Ok(())
    }

    fn check_set_len(&self, _ino: u32, _len: u64) -> FsResult {
        Ok(())
    }

    fn set_len(&self, ino: u32, len: u64) -> FsResult {
        self.mutate(BackendOp::TruncateAllocate, |writer| {
            writer.set_len(ino, len)
        })
    }

    fn allocate_range(&self, ino: u32, offset: u64, len: u64, keep_size: bool) -> FsResult {
        self.mutate(BackendOp::TruncateAllocate, |writer| {
            writer.allocate_range(ino, offset, len, keep_size)
        })
    }

    fn zero_range(&self, ino: u32, offset: u64, len: u64, keep_size: bool) -> FsResult {
        self.mutate(BackendOp::TruncateAllocate, |writer| {
            writer.zero_range(ino, offset, len, keep_size)
        })
    }

    fn punch_hole(&self, ino: u32, offset: u64, len: u64) -> FsResult {
        self.mutate(BackendOp::TruncateAllocate, |writer| {
            writer.punch_hole(ino, offset, len)
        })
    }

    fn sync(&self, ino: u32, data_only: bool) -> FsResult {
        self.mutate(BackendOp::Sync, |writer| writer.sync(ino, data_only))
    }

    fn shutdown(&self) -> FsResult {
        self.mutate(BackendOp::Sync, |writer| writer.shutdown())
    }

    fn set_times(
        &self,
        ino: u32,
        atime: Option<FileTimestamp>,
        mtime: Option<FileTimestamp>,
        ctime: FileTimestamp,
    ) -> FsResult {
        self.mutate_inode_metadata(
            ino,
            Ext4InodeMetadataMutation::Times {
                atime,
                mtime,
                ctime,
            },
        )
    }

    fn set_mode(&self, ino: u32, mode: u32) -> FsResult {
        self.mutate_inode_metadata(ino, Ext4InodeMetadataMutation::Mode(mode))
    }

    fn set_owner(&self, ino: u32, uid: Option<u32>, gid: Option<u32>) -> FsResult {
        self.mutate_inode_metadata(ino, Ext4InodeMetadataMutation::Owner { uid, gid })
    }

    fn inode_flags(&self, ino: u32) -> FsResult<u32> {
        self.with_reader(BackendOp::StatFull, ino, |reader, _| {
            reader.inode_flags_shared(ino)
        })
    }

    fn set_inode_flags(&self, ino: u32, flags: u32) -> FsResult {
        self.mutate_inode_metadata(ino, Ext4InodeMetadataMutation::Flags(flags))
    }

    fn retain_inode(&self, ino: u32) -> FsResult {
        loop {
            let retained =
                self.with_reader(BackendOp::InodeLifetime, ino, |reader, generation| {
                    let mut attr = lwext4_rust::FileAttr::default();
                    reader.fs.get_attr(ino, &mut attr).map_err(map_ext4_error)?;
                    if attr.nlink == 0 {
                        return Err(FsError::NotFound);
                    }
                    Ok(self.inode_runtime.retain_at_generation(
                        ino,
                        generation,
                        &self.cache_generation,
                    ))
                })?;
            if retained {
                return Ok(());
            }
        }
    }

    fn release_inode(&self, ino: u32) -> FsResult<InodeRelease> {
        if self.inode_runtime.prepare_release(ino) == Ext4RuntimeRelease::Retained {
            return Ok(InodeRelease::Retained);
        }
        self.mutate(BackendOp::InodeLifetime, |writer| {
            writer.free_unlinked_inode(ino)
        })
    }

    fn try_release_inode(&self, ino: u32) -> Option<FsResult<InodeRelease>> {
        if self.inode_runtime.try_prepare_release(ino)? == Ext4RuntimeRelease::Retained {
            #[cfg(feature = "perf-counters")]
            {
                perf::record_backend_op_call(BackendOp::InodeLifetime);
                perf::record_backend_try_successful_call();
            }
            return Some(Ok(InodeRelease::Retained));
        }
        self.try_mutate(BackendOp::InodeLifetime, |writer| {
            writer.free_unlinked_inode(ino)
        })
    }

    fn assign_cgroup_pid(&self, _dir_ino: u32, _pid: usize) -> FsResult {
        Err(FsError::InvalidInput)
    }

    fn stat(&self, ino: u32) -> FsResult<FileStat> {
        self.with_reader(BackendOp::StatFull, ino, |reader, _| {
            reader.stat_shared(ino)
        })
    }

    fn stat_basic(&self, ino: u32) -> FsResult<FileStat> {
        self.with_reader(BackendOp::StatBasic, ino, |reader, _| {
            reader.stat_basic_shared(ino)
        })
    }

    fn readlink(&self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        self.with_reader(BackendOp::Readlink, ino, |reader, _| {
            reader.readlink_shared(ino, buf)
        })
    }

    fn prepare_readlink_plan(&self, ino: u32, len: usize) -> Option<Box<dyn BackendReadPlan>> {
        // This phase reads only inode/mapping metadata. External target data
        // is fetched by the returned pointer-free plan after the reader core
        // lock has been released.
        self.with_reader(BackendOp::ReadPlan, ino, |reader, _| {
            reader.prepare_readlink_plan_shared(ino, len)
        })
    }

    fn supports_read_snapshot(&self, _ino: u32) -> bool {
        false
    }

    fn read_snapshot(&self, _ino: u32) -> Option<FsResult<Vec<u8>>> {
        None
    }

    fn prepare_read_plan(
        &self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Option<Box<dyn BackendReadPlan>> {
        self.with_reader(BackendOp::ReadPlan, ino, |reader, _| {
            reader.prepare_read_plan_shared(ino, offset, len)
        })
    }

    fn read_at(&self, ino: u32, buf: &mut [u8], offset: u64) -> usize {
        self.with_reader(BackendOp::ReadFallback, ino, |reader, _| {
            reader.read_at_shared(ino, buf, offset)
        })
    }

    fn prepare_write_plan(
        &self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Option<Box<dyn BackendWritePlan>> {
        // Mapping lookup and writer-core alias invalidation are short control
        // operations. The shared read core is retired by cache epoch only
        // after plan I/O publishes new bytes, so its payload is never rewritten
        // underneath an older reader.
        perf::record_ext4_write_plan_attempt();
        let prepared = self.with_writer_read(BackendOp::Write, |writer| {
            let prepared = writer.prepare_mapped_write_plan(ino, offset, len)?;
            let lease = self.write_leases.reserve(&prepared.fs_blocks)?;
            let aliases = writer.invalidate_mapped_write_aliases(&prepared.fs_blocks)?;
            Some((prepared, lease, aliases))
        });

        let Some((prepared, lease, aliases)) = prepared else {
            perf::record_ext4_write_plan_fallback(false);
            return None;
        };
        perf::record_ext4_write_plan_prepared(
            prepared.runs.len(),
            prepared.fs_blocks.len(),
            aliases,
        );
        Some(Box::new(prepared.publish(lease)))
    }

    fn write_at(&self, ino: u32, buf: &[u8], offset: u64) -> usize {
        let (written, visible) =
            self.with_writer(BackendOp::Write, |writer| writer.write_at(ino, buf, offset));
        if visible.is_ok() { written } else { 0 }
    }

    fn prepare_directory_read_plan(
        &self,
        ino: u32,
        offset: u64,
    ) -> Option<Box<dyn BackendDirectoryReadPlan>> {
        self.with_reader(BackendOp::ReadPlan, ino, |reader, _| {
            reader.prepare_directory_read_plan_shared(ino, offset)
        })
    }

    fn read_dirent64(&self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        self.with_reader(BackendOp::Readdir, ino, |reader, _| {
            reader.read_dirent64_shared(ino, offset, buf)
        })
    }

    fn list_root_names(&self) -> Vec<String> {
        if let Some(plan) = self.with_reader(BackendOp::ReadPlan, EXT4_ROOT_INO, |reader, _| {
            reader.prepare_directory_read_plan_shared(EXT4_ROOT_INO, 0)
        }) {
            return plan
                .execute()
                .expect("failed to execute ext4 root directory snapshot")
                .names();
        }
        self.with_reader(BackendOp::Readdir, EXT4_ROOT_INO, |reader, _| {
            reader.list_root_names_shared()
        })
    }
}
