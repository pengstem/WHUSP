mod io_state;

use super::dirent::{
    DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK, DT_UNKNOWN, LINUX_DIRENT64_ALIGN,
    LINUX_DIRENT64_HEADER_SIZE,
};
#[cfg(feature = "perf-counters")]
use super::vfs::BackendIoSnapshot;
use super::vfs::{
    BackendDirectoryEntry, BackendDirectoryReadPlan, BackendDirectorySnapshot, BackendOp,
    BackendReadPlan, BackendWritePlan, DataOps, FileSystemStat, FsError, FsNodeKind, FsResult,
    InodeLifecycleOps, InodeRelease, LegacyDataOps, LegacyInodeLifecycleOps, LegacyLookupOps,
    LegacyMetadataOps, LegacyNamespaceOps, LegacySyncOps, LookupOps, MetadataOps, NamespaceOps,
    SyncOps,
};
use super::{FS_STATX_ATTR_FLAGS, FileStat, FileTimestamp};
use crate::drivers::block::KernelBlockDevice;
use crate::perf;
use crate::sync::{RawSleepLock, RawSpinNoIrqLock, SleepMutex, SleepRwLock, SpinNoIrqLock};
use crate::task::suspend_current_and_run_next;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::{vec, vec::Vec};
use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::str;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use core::time::Duration;
use log::warn;
use lwext4_rust::ffi::{
    EEXIST, EINVAL, EIO, EISDIR, ENOENT, ENOSPC, ENOTDIR, ENOTEMPTY, ENOTSUP, EXT4_DE_BLKDEV,
    EXT4_DE_CHRDEV, EXT4_DE_DIR, EXT4_DE_FIFO, EXT4_DE_REG_FILE, EXT4_DE_SOCK, EXT4_DE_SYMLINK,
    EXT4_ROOT_INO,
};
use lwext4_rust::{
    BlockDevice as Ext4BlockDevice, EXT4_DEV_BSIZE,
    Ext4DirectoryReadPlan as LwExt4DirectoryReadPlan, Ext4Error, Ext4Filesystem, Ext4FlushProgress,
    Ext4MappedReadPlan, Ext4MappedWritePlan, Ext4Result, Ext4SymlinkReadPlan, FsConfig,
    InodeMetadataUpdate, InodeType, SystemHal,
};

use io_state::{Ext4BlockVersions, Ext4CacheEpoch, Ext4PhysicalLeaseTable, Ext4Sequence};

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

#[repr(align(64))]
struct Ext4BcacheIndexReaderSlot {
    active: AtomicUsize,
    restore_irqs: AtomicBool,
}

impl Ext4BcacheIndexReaderSlot {
    fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            restore_irqs: AtomicBool::new(false),
        }
    }
}

/// Scalable read admission for stable bcache index lookups.
///
/// Structural RB/LRU/dirty-list changes retain one IRQ-safe writer lock. A
/// resident lookup touches only its CPU-local reader slot and atomic buffer
/// refcount. The C callback pair cannot carry a Rust guard, so local IRQ state
/// is stored in the slot until the matching unlock on the same CPU.
struct Ext4BcacheIndexAdmission {
    writer: RawSpinNoIrqLock,
    writer_pending: AtomicBool,
    readers: Vec<Ext4BcacheIndexReaderSlot>,
}

impl Ext4BcacheIndexAdmission {
    fn new() -> Self {
        Self {
            writer: RawSpinNoIrqLock::new(),
            writer_pending: AtomicBool::new(false),
            readers: (0..crate::config::MAX_CPUS)
                .map(|_| Ext4BcacheIndexReaderSlot::new())
                .collect(),
        }
    }

    #[inline]
    fn poll_while_spinning() {
        crate::cpu::handle_remote_sync_ipi();
        #[cfg(target_arch = "loongarch64")]
        crate::arch::smp::handle_tlb_ipi();
        spin_loop();
    }

    /// Returns whether a writer gate made this reader wait or retry.
    fn read_lock(&self) -> bool {
        let restore_irqs = crate::arch::interrupt::supervisor_interrupt_enabled();
        crate::arch::interrupt::disable_supervisor_interrupt();
        let cpu = crate::cpu::current_id();
        let slot = &self.readers[cpu];
        let previous = slot.active.load(Ordering::SeqCst);
        assert_eq!(previous, 0, "recursive ext4 bcache index read callback");
        slot.restore_irqs.store(restore_irqs, Ordering::Relaxed);
        let mut contended = false;
        loop {
            while self.writer_pending.load(Ordering::SeqCst) {
                contended = true;
                Self::poll_while_spinning();
            }
            slot.active.fetch_add(1, Ordering::SeqCst);
            if !self.writer_pending.load(Ordering::SeqCst) {
                return contended;
            }
            let previous = slot.active.fetch_sub(1, Ordering::SeqCst);
            assert_eq!(previous, 1, "ext4 bcache reader retry corrupted slot");
            contended = true;
        }
    }

    unsafe fn read_unlock(&self) {
        let slot = &self.readers[crate::cpu::current_id()];
        let previous = slot.active.fetch_sub(1, Ordering::SeqCst);
        assert_eq!(previous, 1, "ext4 bcache index read callback underflow");
        if slot.restore_irqs.load(Ordering::Relaxed) {
            crate::arch::interrupt::enable_supervisor_interrupt();
        }
    }

    fn readers_active(&self) -> bool {
        self.readers
            .iter()
            .any(|slot| slot.active.load(Ordering::SeqCst) != 0)
    }

    fn write_lock(&self) {
        self.writer.lock();
        self.writer_pending.store(true, Ordering::SeqCst);
        while self.readers_active() {
            Self::poll_while_spinning();
        }
    }

    #[cfg(feature = "perf-counters")]
    fn try_write_lock(&self) -> bool {
        if !self.writer.try_lock() {
            return false;
        }
        self.writer_pending.store(true, Ordering::SeqCst);
        if self.readers_active() {
            self.writer_pending.store(false, Ordering::SeqCst);
            unsafe { self.writer.unlock() };
            return false;
        }
        true
    }

    unsafe fn write_unlock(&self) {
        self.writer_pending.store(false, Ordering::SeqCst);
        unsafe { self.writer.unlock() };
    }
}

/// Lock domain owned by exactly one lwext4 metadata cache.
///
/// The index lock covers only RB-tree/list/refcount bookkeeping. LBA shards
/// serialize cache fill, eviction, and writeback state for colliding logical
/// blocks without forcing unrelated device I/O through one global lock.
struct Ext4BcacheLocks {
    index: Ext4BcacheIndexAdmission,
    lba_shards: Vec<RawSleepLock>,
}

impl Ext4BcacheLocks {
    fn new() -> Self {
        Self {
            index: Ext4BcacheIndexAdmission::new(),
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
            let contended = !self.index.try_write_lock();
            if contended {
                self.index.write_lock();
            }
            perf::record_ext4_bcache_index_lock(contended);
        }
        #[cfg(not(feature = "perf-counters"))]
        self.index.write_lock();
    }

    /// Releases one matching [`Self::lock_index`] acquisition.
    ///
    /// # Safety
    ///
    /// The current task must own the index lock exactly once.
    #[inline]
    unsafe fn unlock_index(&self) {
        unsafe { self.index.write_unlock() };
    }

    #[inline]
    fn lock_index_read(&self) {
        let contended = self.index.read_lock();
        #[cfg(feature = "perf-counters")]
        perf::record_ext4_bcache_index_lock(contended);
        #[cfg(not(feature = "perf-counters"))]
        let _ = contended;
    }

    /// Releases one matching [`Self::lock_index_read`] acquisition.
    ///
    /// # Safety
    ///
    /// The current CPU must own exactly one index read admission.
    #[inline]
    unsafe fn unlock_index_read(&self) {
        unsafe { self.index.read_unlock() };
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
    dev: Arc<KernelBlockDevice>,
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
        self.write_sequence.read_stable(blocks, buf.len(), || {
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

    fn lock_bcache_index_read(&self) {
        self.bcache_locks.lock_index_read();
    }

    unsafe fn unlock_bcache_index_read(&self) {
        unsafe { self.bcache_locks.unlock_index_read() };
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
    device: Arc<KernelBlockDevice>,
    cache_epoch: Arc<Ext4CacheEpoch>,
    write_sequence: Arc<Ext4Sequence>,
    physical_leases: Arc<Ext4PhysicalLeaseTable>,
    block_versions: Arc<Ext4BlockVersions>,
    #[cfg(feature = "perf-counters")]
    io_counters: Arc<Ext4IoCounters>,
    inode_runtime: Arc<Ext4InodeRuntimeTable>,
    inode_metadata: Arc<Ext4InodeMetadataTable>,
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

const EXT4_METADATA_MODE: u16 = 1 << 0;
const EXT4_METADATA_UID: u16 = 1 << 1;
const EXT4_METADATA_GID: u16 = 1 << 2;
const EXT4_METADATA_ATIME: u16 = 1 << 3;
const EXT4_METADATA_MTIME: u16 = 1 << 4;
const EXT4_METADATA_CTIME: u16 = 1 << 5;
const EXT4_METADATA_FLAGS: u16 = 1 << 6;

#[derive(Default)]
struct Ext4InodeMetadataShadow {
    attr: Option<lwext4_rust::FileAttr>,
    owned_fields: u16,
    dirty_fields: u16,
    first_dirty_epoch: u64,
}

#[repr(align(64))]
struct Ext4InodeMetadataCell {
    ino: u32,
    shadow: SleepMutex<Ext4InodeMetadataShadow>,
}

impl Ext4InodeMetadataCell {
    fn new(ino: u32) -> Self {
        Self {
            ino,
            shadow: SleepMutex::new(Ext4InodeMetadataShadow::default()),
        }
    }
}

struct Ext4InodeMetadataShard {
    populated: AtomicBool,
    entries: SleepRwLock<BTreeMap<u32, Arc<Ext4InodeMetadataCell>>>,
}

impl Ext4InodeMetadataShard {
    fn new() -> Self {
        Self {
            populated: AtomicBool::new(false),
            entries: SleepRwLock::new(BTreeMap::new()),
        }
    }
}

/// Persistent in-core metadata ownership for one ext4 mount.
///
/// The shard locks protect only entry lookup/installation. All sleeping inode
/// work happens after cloning the stable cell `Arc`; independent inodes then
/// mutate disjoint cache lines even when their raw inodes share one disk block.
struct Ext4InodeMetadataTable {
    shards: Vec<Ext4InodeMetadataShard>,
    dirty_epoch: AtomicU64,
}

impl Ext4InodeMetadataTable {
    fn new() -> Self {
        Self {
            shards: (0..EXT4_INODE_RUNTIME_SHARDS)
                .map(|_| Ext4InodeMetadataShard::new())
                .collect(),
            dirty_epoch: AtomicU64::new(0),
        }
    }

    #[inline]
    fn shard(&self, ino: u32) -> &Ext4InodeMetadataShard {
        let ino = ino as usize;
        &self.shards[(ino ^ (ino >> 5)) % self.shards.len()]
    }

    fn cell(&self, ino: u32) -> Arc<Ext4InodeMetadataCell> {
        let shard = self.shard(ino);
        if shard.populated.load(Ordering::Acquire)
            && let Some(cell) = shard.entries.read().get(&ino).cloned()
        {
            return cell;
        }
        let candidate = Arc::new(Ext4InodeMetadataCell::new(ino));
        let mut entries = shard.entries.write();
        let cell = entries.entry(ino).or_insert(candidate).clone();
        shard.populated.store(true, Ordering::Release);
        cell
    }

    fn existing_cell(&self, ino: u32) -> Option<Arc<Ext4InodeMetadataCell>> {
        let shard = self.shard(ino);
        if !shard.populated.load(Ordering::Acquire) {
            return None;
        }
        shard.entries.read().get(&ino).cloned()
    }

    fn mark_dirty(&self, shadow: &mut Ext4InodeMetadataShadow) {
        if shadow.first_dirty_epoch == 0 {
            shadow.first_dirty_epoch = self.dirty_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        }
    }

    fn dirty_ticket(&self) -> u64 {
        self.dirty_epoch.load(Ordering::Acquire)
    }

    fn cells_snapshot(&self) -> Vec<Arc<Ext4InodeMetadataCell>> {
        let mut cells = Vec::new();
        for shard in &self.shards {
            if shard.populated.load(Ordering::Acquire) {
                cells.extend(shard.entries.read().values().cloned());
            }
        }
        cells
    }

    fn remove(&self, ino: u32) {
        let shard = self.shard(ino);
        if !shard.populated.load(Ordering::Acquire) {
            return;
        }
        let mut entries = shard.entries.write();
        entries.remove(&ino);
        if entries.is_empty() {
            shard.populated.store(false, Ordering::Release);
        }
    }

    fn overlay_attr(&self, ino: u32, attr: &mut lwext4_rust::FileAttr) {
        let Some(cell) = self.existing_cell(ino) else {
            return;
        };
        let shadow = cell.shadow.lock();
        let Some(cached) = shadow.attr.as_ref() else {
            return;
        };
        let fields = shadow.owned_fields;
        if fields & EXT4_METADATA_MODE != 0 {
            attr.mode = cached.mode;
        }
        if fields & EXT4_METADATA_UID != 0 {
            attr.uid = cached.uid;
        }
        if fields & EXT4_METADATA_GID != 0 {
            attr.gid = cached.gid;
        }
        if fields & EXT4_METADATA_ATIME != 0 {
            attr.atime = cached.atime;
        }
        if fields & EXT4_METADATA_MTIME != 0 {
            attr.mtime = cached.mtime;
        }
        if fields & EXT4_METADATA_CTIME != 0 {
            attr.ctime = cached.ctime;
        }
        if fields & EXT4_METADATA_FLAGS != 0 {
            attr.flags = cached.flags;
        }
    }

    fn cached_flags(&self, ino: u32) -> Option<u32> {
        let cell = self.existing_cell(ino)?;
        let shadow = cell.shadow.lock();
        (shadow.owned_fields & EXT4_METADATA_FLAGS != 0).then(|| {
            shadow
                .attr
                .as_ref()
                .expect("owned inode flags without shadow")
                .flags
        })
    }
}

impl Ext4InodeMetadataShadow {
    fn apply(&mut self, mutation: Ext4InodeMetadataMutation) -> FsResult {
        let attr = self
            .attr
            .as_mut()
            .expect("ext4 metadata shadow mutated before initialization");
        let fields = match mutation {
            Ext4InodeMetadataMutation::Times {
                atime,
                mtime,
                ctime,
            } => {
                let mut fields = EXT4_METADATA_CTIME;
                if let Some(atime) = atime {
                    attr.atime = atime.to_duration();
                    fields |= EXT4_METADATA_ATIME;
                }
                if let Some(mtime) = mtime {
                    attr.mtime = mtime.to_duration();
                    fields |= EXT4_METADATA_MTIME;
                }
                attr.ctime = ctime.to_duration();
                fields
            }
            Ext4InodeMetadataMutation::Mode(mode) => {
                attr.mode = (attr.mode & !0o7777) | (mode & 0o7777);
                EXT4_METADATA_MODE
            }
            Ext4InodeMetadataMutation::Owner { uid, gid } => {
                if uid.is_some_and(|uid| uid > u16::MAX as u32)
                    || gid.is_some_and(|gid| gid > u16::MAX as u32)
                {
                    // UNFINISHED: The wrapper currently exposes only the low
                    // 16-bit ext4 uid/gid fields.
                    return Err(FsError::InvalidInput);
                }
                let mut fields = 0;
                if let Some(uid) = uid {
                    attr.uid = uid;
                    fields |= EXT4_METADATA_UID;
                }
                if let Some(gid) = gid {
                    attr.gid = gid;
                    fields |= EXT4_METADATA_GID;
                }
                fields
            }
            Ext4InodeMetadataMutation::Flags(flags) => {
                attr.flags = flags;
                EXT4_METADATA_FLAGS
            }
        };
        self.owned_fields |= fields;
        self.dirty_fields |= fields;
        Ok(())
    }

    fn writeback_update(&self) -> InodeMetadataUpdate {
        let attr = self
            .attr
            .as_ref()
            .expect("dirty ext4 metadata fields without a shadow");
        let dirty = self.dirty_fields;
        InodeMetadataUpdate {
            mode: (dirty & EXT4_METADATA_MODE != 0).then_some(attr.mode),
            uid: (dirty & EXT4_METADATA_UID != 0).then_some(attr.uid as u16),
            gid: (dirty & EXT4_METADATA_GID != 0).then_some(attr.gid as u16),
            atime: (dirty & EXT4_METADATA_ATIME != 0).then_some(attr.atime),
            mtime: (dirty & EXT4_METADATA_MTIME != 0).then_some(attr.mtime),
            ctime: (dirty & EXT4_METADATA_CTIME != 0).then_some(attr.ctime),
            flags: (dirty & EXT4_METADATA_FLAGS != 0).then_some(attr.flags),
        }
    }
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
// Concurrent access is exposed only by `SharedExt4WriteCore`, whose narrower
// Sync proof audits the callable operations and their external lock domains.
unsafe impl Send for Ext4Mount {}

impl Ext4Mount {
    fn open(
        device: Arc<KernelBlockDevice>,
        cache_epoch: Arc<Ext4CacheEpoch>,
        write_sequence: Arc<Ext4Sequence>,
        physical_leases: Arc<Ext4PhysicalLeaseTable>,
        block_versions: Arc<Ext4BlockVersions>,
    ) -> Result<Self, Ext4Error> {
        #[cfg(feature = "perf-counters")]
        let io_counters = Arc::new(Ext4IoCounters::default());
        let inode_runtime = Arc::new(Ext4InodeRuntimeTable::new());
        let inode_metadata = Arc::new(Ext4InodeMetadataTable::new());
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
            inode_metadata,
        })
    }

    fn flush_all(&mut self) -> FsResult {
        self.fs.flush().map_err(map_ext4_error)
    }

    fn dirty_ticket(&self) -> u64 {
        self.fs.dirty_ticket()
    }

    fn flush_through(&self, ticket: u64) -> FsResult<Ext4FlushProgress> {
        self.fs.flush_through(ticket).map_err(map_ext4_error)
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
        let mut fs_blocks = Vec::new();
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
                let Some(device_blocks) = run.block_count.checked_mul(device_blocks_per_fs_block)
                else {
                    record_fallback();
                    return None;
                };
                if device_block
                    .checked_add(device_blocks)
                    .is_none_or(|end| end > self.device.num_blocks() as usize)
                {
                    record_fallback();
                    return None;
                }
                data_runs += 1;
                data_blocks += run.block_count;
                for delta in 0..run.block_count {
                    let Some(fs_block) = fs_block.checked_add(delta as u64) else {
                        record_fallback();
                        return None;
                    };
                    fs_blocks.push(fs_block);
                }
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
        if !self.fs.device_snapshot_blocks_are_clean(&fs_blocks) {
            record_fallback();
            return None;
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
        self.inode_metadata.remove(ino);
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

#[repr(align(64))]
struct Ext4CoreReaderSlot {
    active: AtomicUsize,
}

impl Ext4CoreReaderSlot {
    fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
        }
    }
}

/// Per-CPU shared-core admission with a rare-writer drain protocol.
///
/// Shared callers update only their selected cache-line-aligned counter. A
/// legacy writer closes the global read gate, then waits for every slot to
/// drain. SeqCst ordering makes the reader recheck and writer scan a simple
/// total-order proof instead of relying on architecture-specific fences.
struct Ext4CoreAdmission {
    writer_lock: RawSleepLock,
    writer_pending: AtomicBool,
    waiting_writers: AtomicUsize,
    readers: Vec<Ext4CoreReaderSlot>,
}

struct Ext4CoreReadGuard<'a> {
    admission: &'a Ext4CoreAdmission,
    slot: usize,
}

struct Ext4CoreWriteGuard<'a> {
    admission: &'a Ext4CoreAdmission,
}

impl Ext4CoreAdmission {
    fn new() -> Self {
        Self {
            writer_lock: RawSleepLock::new(),
            writer_pending: AtomicBool::new(false),
            waiting_writers: AtomicUsize::new(0),
            readers: (0..crate::config::MAX_CPUS)
                .map(|_| Ext4CoreReaderSlot::new())
                .collect(),
        }
    }

    fn try_read(&self) -> Option<Ext4CoreReadGuard<'_>> {
        if self.writer_pending.load(Ordering::SeqCst) {
            return None;
        }
        let slot = crate::cpu::current_id();
        self.readers[slot].active.fetch_add(1, Ordering::SeqCst);
        if self.writer_pending.load(Ordering::SeqCst) {
            let previous = self.readers[slot].active.fetch_sub(1, Ordering::SeqCst);
            assert!(previous > 0, "ext4 core reader admission underflow");
            return None;
        }
        Some(Ext4CoreReadGuard {
            admission: self,
            slot,
        })
    }

    fn read(&self) -> Ext4CoreReadGuard<'_> {
        loop {
            if let Some(guard) = self.try_read() {
                return guard;
            }
            perf::record_ext4_core_reader_wait_yield();
            suspend_current_and_run_next();
        }
    }

    fn readers_active(&self) -> bool {
        self.readers
            .iter()
            .any(|slot| slot.active.load(Ordering::SeqCst) != 0)
    }

    fn register_writer(&self) {
        self.waiting_writers.fetch_add(1, Ordering::SeqCst);
        // Every registrant publishes closed. This also covers the race where
        // the first registrant is descheduled between increment and store.
        self.writer_pending.store(true, Ordering::SeqCst);
    }

    fn unregister_writer(&self) {
        let previous = self.waiting_writers.fetch_sub(1, Ordering::SeqCst);
        assert!(previous > 0, "ext4 core writer admission underflow");
        if previous == 1 {
            self.writer_pending.store(false, Ordering::SeqCst);
        }
    }

    fn try_write(&self) -> Option<Ext4CoreWriteGuard<'_>> {
        if !self.writer_lock.try_lock() {
            return None;
        }
        self.register_writer();
        if self.readers_active() {
            self.unregister_writer();
            unsafe { self.writer_lock.unlock() };
            return None;
        }
        Some(Ext4CoreWriteGuard { admission: self })
    }

    fn write(&self) -> Ext4CoreWriteGuard<'_> {
        self.register_writer();
        self.writer_lock.lock();
        while self.readers_active() {
            perf::record_ext4_core_writer_wait_yield();
            suspend_current_and_run_next();
        }
        Ext4CoreWriteGuard { admission: self }
    }
}

impl Drop for Ext4CoreReadGuard<'_> {
    fn drop(&mut self) {
        let previous = self.admission.readers[self.slot]
            .active
            .fetch_sub(1, Ordering::SeqCst);
        assert!(previous > 0, "ext4 core reader guard underflow");
    }
}

impl Drop for Ext4CoreWriteGuard<'_> {
    fn drop(&mut self) {
        self.admission.unregister_writer();
        unsafe { self.admission.writer_lock.unlock() };
    }
}

/// One writable lwext4 core with a deliberately narrow shared-entry proof.
///
/// `Ext4CoreAdmission` controls which accessor may be used. Shared guards are
/// restricted to audited metadata reads, create, and per-inode metadata
/// mutations; every remaining legacy mutation, cache invalidation, and
/// shutdown requires an exclusive guard.
struct SharedExt4WriteCore {
    mount: Box<UnsafeCell<Ext4Mount>>,
}

// SAFETY: `shared()` is called only while a core-admission reader is active and
// only for audited shared reads/mutations. Their caller-local
// inode and directory refs operate on VFS-leased objects; allocator and bcache
// shared state use the C callbacks installed on the canonical writer.
// `exclusive()` requires core-admission writer ownership after all reader
// shards drain, excluding every shared call.
unsafe impl Sync for SharedExt4WriteCore {}

impl SharedExt4WriteCore {
    fn new(mount: Ext4Mount) -> Self {
        Self {
            mount: Box::new(UnsafeCell::new(mount)),
        }
    }

    fn flush_handle(&self) -> Ext4FlushHandle {
        Ext4FlushHandle {
            mount: self.mount.get(),
        }
    }

    fn shared(&self) -> &Ext4Mount {
        // SAFETY: the wrapper's public protocol permits only audited shared
        // metadata reads and mutation operations through this reference.
        unsafe { &*self.mount.get() }
    }

    fn exclusive(&self) -> &mut Ext4Mount {
        // SAFETY: callers invoke this only while holding the core-admission
        // write guard, after its gate has excluded and drained shared callers.
        unsafe { &mut *self.mount.get() }
    }
}

/// Stable access to the canonical bcache writeback engine after the writer
/// core guard has been released. The pointed-to mount is heap allocated and
/// outlives this handle as a field of the same backend.
struct Ext4FlushHandle {
    mount: *const Ext4Mount,
}

// SAFETY: only `flush_through` is exposed. The kernel serializes handle users
// with `flush_coordinator`; concurrent metadata callers are protected by the
// bcache's index/LBA/refcount protocol and only zero-reference payloads can be
// submitted. Backend destruction requires exclusive ownership, so it cannot
// overlap an in-flight method call.
unsafe impl Send for Ext4FlushHandle {}
unsafe impl Sync for Ext4FlushHandle {}

impl Ext4FlushHandle {
    fn flush_through(&self, ticket: u64) -> FsResult<Ext4FlushProgress> {
        // SAFETY: `SharedExt4WriteCore` stores the mount in a stable Box and
        // remains alive for every call through this sibling backend field.
        unsafe { &*self.mount }.flush_through(ticket)
    }

    #[cfg(feature = "perf-counters")]
    fn io_snapshot(&self) -> BackendIoSnapshot {
        // SAFETY: the counter fields are atomic and have the same stable-mount
        // lifetime as `flush_through` above.
        unsafe { &*self.mount }.io_counters.snapshot()
    }
}

/// One canonical lwext4 core with audited concurrent entry and detached
/// ticketed writeback.
pub(super) struct ConcurrentExt4Backend {
    writer: SharedExt4WriteCore,
    core_admission: Ext4CoreAdmission,
    flush_handle: Ext4FlushHandle,
    flush_coordinator: SleepMutex<()>,
    completed_flush_ticket: AtomicU64,
    inode_runtime: Arc<Ext4InodeRuntimeTable>,
    inode_metadata: Arc<Ext4InodeMetadataTable>,
    write_leases: Arc<Ext4WriteLeaseTable>,
}

impl ConcurrentExt4Backend {
    pub(super) fn open(device: Arc<KernelBlockDevice>) -> Result<Self, Ext4Error> {
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
        let inode_runtime = writer.inode_runtime.clone();
        let inode_metadata = writer.inode_metadata.clone();
        let writer = SharedExt4WriteCore::new(writer);
        let flush_handle = writer.flush_handle();
        Ok(Self {
            writer,
            core_admission: Ext4CoreAdmission::new(),
            flush_handle,
            flush_coordinator: SleepMutex::new(()),
            completed_flush_ticket: AtomicU64::new(0),
            inode_runtime,
            inode_metadata,
            write_leases: Arc::new(Ext4WriteLeaseTable::new()),
        })
    }

    fn lock_writer_exclusive(&self, op: BackendOp) -> Ext4CoreWriteGuard<'_> {
        let _ = op;
        match self.core_admission.try_write() {
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
                let core = self.core_admission.write();
                #[cfg(feature = "perf-counters")]
                drop(op_wait_scope);
                drop(wait_scope);
                core
            }
        }
    }

    fn lock_writer_shared(&self, op: BackendOp) -> Ext4CoreReadGuard<'_> {
        let _ = op;
        match self.core_admission.try_read() {
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
                let core = self.core_admission.read();
                #[cfg(feature = "perf-counters")]
                drop(op_wait_scope);
                drop(wait_scope);
                core
            }
        }
    }

    fn flush_through_ticket(&self, ticket: u64) -> FsResult {
        let mut waits = 0usize;
        loop {
            if self.completed_flush_ticket.load(Ordering::Acquire) >= ticket {
                return Ok(());
            }
            if let Some(_coordinator) = self.flush_coordinator.try_lock() {
                if self.completed_flush_ticket.load(Ordering::Acquire) >= ticket {
                    return Ok(());
                }
                let progress = self.flush_handle.flush_through(ticket)?;
                if progress.complete {
                    self.completed_flush_ticket
                        .fetch_max(ticket, Ordering::AcqRel);
                    return Ok(());
                }
                waits += 1;
                if waits == 4096 || waits.is_multiple_of(1 << 20) {
                    warn!(
                        "ext4 flush ticket stalled: ticket={} completed={} pending_lba={} pending_refs={} waits={}",
                        ticket,
                        self.completed_flush_ticket.load(Ordering::Acquire),
                        progress.pending_lba,
                        progress.pending_refs,
                        waits
                    );
                }
            }
            // A covered buffer is still referenced by another metadata
            // caller, or a group-commit leader is already flushing it. Avoid
            // a FIFO mutex convoy: followers yield and all observe the same
            // completed-ticket publication when the leader finishes.
            perf::record_ext4_flush_ticket_wait_yield();
            suspend_current_and_run_next();
        }
    }

    fn with_reader<V>(&self, op: BackendOp, _ino: u32, f: impl FnOnce(&Ext4Mount) -> V) -> V {
        let _ = op;
        #[cfg(feature = "perf-counters")]
        perf::record_backend_op_call(op);
        let writer = self.lock_writer_shared(op);
        let mount = self.writer.shared();
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

    fn mutate_inode_metadata(&self, ino: u32, mutation: Ext4InodeMetadataMutation) -> FsResult {
        let op = BackendOp::NamespaceMutation;
        perf::record_ext4_metadata_transaction_attempt();
        #[cfg(feature = "perf-counters")]
        perf::record_backend_op_call(op);
        let writer = self.lock_writer_shared(op);
        let mount = self.writer.shared();
        let hold_scope = perf::time_scope(perf::ProfilePoint::MountBackendHold);
        #[cfg(feature = "perf-counters")]
        let io_before = mount.io_counters.snapshot();
        #[cfg(feature = "perf-counters")]
        let op_hold_scope = perf::time_backend_op_hold(op);
        let cell = self.inode_metadata.cell(ino);
        perf::record_ext4_metadata_transaction_begin();
        let result = (|| {
            let mut shadow = cell.shadow.lock();
            if shadow.attr.is_none() {
                let mut attr = lwext4_rust::FileAttr::default();
                mount.fs.get_attr(ino, &mut attr).map_err(map_ext4_error)?;
                shadow.attr = Some(attr);
            }
            shadow.apply(mutation)?;
            if shadow.dirty_fields != 0 {
                self.inode_metadata.mark_dirty(&mut shadow);
            }
            Ok(())
        })();
        perf::record_ext4_metadata_transaction_end();
        #[cfg(feature = "perf-counters")]
        {
            drop(op_hold_scope);
            perf::record_backend_op_io(op, mount.io_counters.snapshot().delta_since(io_before));
        }
        drop(writer);
        drop(hold_scope);
        if result.is_ok() {
            // Per-inode shadows merge into the canonical cache at sync. The
            // retired private-core transaction fields remain zero.
            perf::record_ext4_metadata_transaction_commit(0, 0, 0, 0, 0);
        } else {
            perf::record_ext4_metadata_transaction_fallback();
        }
        result
    }

    fn materialize_metadata_shadows(&self, mount: &Ext4Mount, ticket: u64) -> FsResult {
        for cell in self.inode_metadata.cells_snapshot() {
            let mut shadow = cell.shadow.lock();
            if shadow.first_dirty_epoch == 0 || shadow.first_dirty_epoch > ticket {
                continue;
            }
            debug_assert_ne!(shadow.dirty_fields, 0);
            mount
                .fs
                .apply_inode_metadata(cell.ino, shadow.writeback_update())
                .map_err(map_ext4_error)?;
            shadow.dirty_fields = 0;
            shadow.first_dirty_epoch = 0;
        }
        Ok(())
    }

    fn with_writer_read<V>(&self, op: BackendOp, f: impl FnOnce(&mut Ext4Mount) -> V) -> V {
        let _ = op;
        #[cfg(feature = "perf-counters")]
        perf::record_backend_op_call(op);
        let writer = self.lock_writer_exclusive(op);
        let mount = self.writer.exclusive();
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
        // Legacy/finalization operations still require exclusive core entry.
        // Serialize them against the detached flush handle so the canonical
        // bcache cannot be finalized or mutated by a legacy path mid-writeback.
        let _coordinator = self.flush_coordinator.lock();
        let writer = self.lock_writer_exclusive(op);
        let mount = self.writer.exclusive();
        let hold_scope = perf::time_scope(perf::ProfilePoint::MountBackendHold);
        #[cfg(feature = "perf-counters")]
        let io_before = mount.io_counters.snapshot();
        #[cfg(feature = "perf-counters")]
        let op_hold_scope = perf::time_backend_op_hold(op);
        let result = f(mount);
        let ticket = mount.dirty_ticket();
        let visible = mount.flush_all();
        if visible.is_ok() {
            self.completed_flush_ticket
                .fetch_max(ticket, Ordering::AcqRel);
        }
        #[cfg(feature = "perf-counters")]
        {
            drop(op_hold_scope);
            perf::record_backend_op_io(op, mount.io_counters.snapshot().delta_since(io_before));
        }
        drop(writer);
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
        let hold_scope = perf::time_scope(perf::ProfilePoint::MountBackendHold);

        let writer = self.lock_writer_shared(op);
        let mount = self.writer.shared();
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

    fn sync_current(&self) -> FsResult {
        let op = BackendOp::Sync;
        #[cfg(feature = "perf-counters")]
        {
            perf::record_backend_op_call(op);
        }
        #[cfg(feature = "perf-counters")]
        let io_before = self.flush_handle.io_snapshot();

        // First capture the in-core inode boundary. Covered shadows are merged
        // into the canonical bcache while legacy inode removal is excluded;
        // device writeback and all waits still happen after core admission is
        // released.
        let shadow_ticket = self.inode_metadata.dirty_ticket();
        let writer = self.lock_writer_shared(op);
        let hold_scope = perf::time_scope(perf::ProfilePoint::MountBackendHold);
        #[cfg(feature = "perf-counters")]
        let op_hold_scope = perf::time_backend_op_hold(op);
        if let Err(err) = self.materialize_metadata_shadows(self.writer.shared(), shadow_ticket) {
            #[cfg(feature = "perf-counters")]
            drop(op_hold_scope);
            drop(writer);
            drop(hold_scope);
            #[cfg(feature = "perf-counters")]
            perf::record_backend_op_io(op, self.flush_handle.io_snapshot().delta_since(io_before));
            return Err(err);
        }
        let ticket = self.writer.shared().dirty_ticket();
        #[cfg(feature = "perf-counters")]
        drop(op_hold_scope);
        drop(writer);
        drop(hold_scope);

        let result = self.flush_through_ticket(ticket);
        #[cfg(feature = "perf-counters")]
        perf::record_backend_op_io(op, self.flush_handle.io_snapshot().delta_since(io_before));
        result
    }

    fn try_mutate<T>(
        &self,
        op: BackendOp,
        f: impl FnOnce(&mut Ext4Mount) -> FsResult<T>,
    ) -> Option<FsResult<T>> {
        let _ = op;
        let _coordinator = self.flush_coordinator.try_lock()?;
        let writer = self.core_admission.try_write()?;
        let mount = self.writer.exclusive();
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
        let ticket = mount.dirty_ticket();
        let visible = mount.flush_all();
        if visible.is_ok() {
            self.completed_flush_ticket
                .fetch_max(ticket, Ordering::AcqRel);
        }
        #[cfg(feature = "perf-counters")]
        {
            drop(op_hold_scope);
            perf::record_backend_op_io(op, mount.io_counters.snapshot().delta_since(io_before));
        }
        drop(writer);
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
    device: Arc<KernelBlockDevice>,
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
    device: Arc<KernelBlockDevice>,
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
    device: Arc<KernelBlockDevice>,
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
                self.write_sequence.read_stable(
                    run.byte_len / EXT4_DEV_BSIZE,
                    run.byte_len,
                    || {
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
                    },
                );
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
                let io = self.write_sequence.read_stable(
                    run.byte_len / EXT4_DEV_BSIZE,
                    run.byte_len,
                    || {
                        self.device
                            .read_blocks_versioned_fill_for_file_plan(run.device_block, run_buf)
                    },
                );
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
        if let Some(flags) = self.inode_metadata.cached_flags(ino) {
            return Ok(flags);
        }
        self.fs.inode_flags(ino).map_err(map_ext4_error)
    }

    fn stat_shared(&self, ino: u32) -> FsResult<FileStat> {
        let mut attr = lwext4_rust::FileAttr::default();
        {
            let _profile_scope = perf::time_scope(perf::ProfilePoint::Ext4StatGetAttr);
            self.fs.get_attr(ino, &mut attr).map_err(map_ext4_error)?;
        }
        self.inode_metadata.overlay_attr(ino, &mut attr);
        let inode_flags = attr.flags;
        Ok(self.stat_from_attr(ino, attr, inode_flags, FS_STATX_ATTR_FLAGS))
    }

    fn stat_basic_shared(&self, ino: u32) -> FsResult<FileStat> {
        let mut attr = lwext4_rust::FileAttr::default();
        {
            let _profile_scope = perf::time_scope(perf::ProfilePoint::Ext4StatGetAttr);
            self.fs.get_attr(ino, &mut attr).map_err(map_ext4_error)?;
        }
        self.inode_metadata.overlay_attr(ino, &mut attr);
        Ok(self.stat_from_attr(ino, attr, 0, 0))
    }

    /// Audited shared mutation used only through `SharedExt4WriteCore`.
    /// The caller owns the parent-directory VFS mutation lease; allocator and
    /// bcache state are protected by the canonical writer's callbacks.
    fn create_file_shared(&self, parent_ino: u32, leaf_name: &str) -> FsResult<u32> {
        let ino = self
            .fs
            .create(parent_ino, leaf_name, InodeType::RegularFile, 0o644)
            .map_err(map_ext4_error)?;
        self.inode_metadata.remove(ino);
        Ok(ino)
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
        self.inode_metadata.remove(ino);
        if matches!(kind, FsNodeKind::CharacterDevice | FsNodeKind::BlockDevice) {
            // UNFINISHED: The vendored wrapper does not expose persistent
            // device major/minor payloads yet. Runtime state is independently
            // sharded and safe for different newly allocated inodes.
            self.inode_runtime.set_special_rdev(ino, rdev);
        }
        Ok(ino)
    }

    fn create_dir_shared(&self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32> {
        let ino = self
            .fs
            .create(parent_ino, leaf_name, InodeType::Directory, mode)
            .map_err(map_ext4_error)?;
        self.inode_metadata.remove(ino);
        Ok(ino)
    }

    fn set_mode_shared(&self, ino: u32, mode: u32) -> FsResult {
        self.fs.set_mode(ino, mode).map_err(map_ext4_error)
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

impl LegacyLookupOps for Ext4Mount {
    fn lookup_component_from(
        &mut self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)> {
        self.lookup_component_from_shared(parent_ino, component)
    }

    fn readlink(&mut self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        self.readlink_shared(ino, buf)
    }

    fn prepare_readlink_plan(&mut self, ino: u32, len: usize) -> Option<Box<dyn BackendReadPlan>> {
        self.prepare_readlink_plan_shared(ino, len)
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

impl LegacyMetadataOps for Ext4Mount {
    fn statfs(&mut self) -> FileSystemStat {
        self.statfs_shared()
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

    fn stat(&mut self, ino: u32) -> FsResult<FileStat> {
        self.stat_shared(ino)
    }

    fn stat_basic(&mut self, ino: u32) -> FsResult<FileStat> {
        self.stat_basic_shared(ino)
    }
}

impl LegacyDataOps for Ext4Mount {
    #[cfg(feature = "perf-counters")]
    fn io_snapshot(&self) -> BackendIoSnapshot {
        self.io_counters.snapshot()
    }

    fn set_len(&mut self, ino: u32, len: u64) -> FsResult {
        self.fs.set_len(ino, len).map_err(map_ext4_error)
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
}

impl LegacyNamespaceOps for Ext4Mount {
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
        self.inode_metadata.remove(ino);
        match self.fs.set_symlink(ino, target).map_err(map_ext4_error) {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = self.fs.unlink(parent_ino, leaf_name);
                self.inode_metadata.remove(ino);
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
            if !inode_exists {
                self.inode_metadata.remove(child_ino);
            }
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
}

impl LegacySyncOps for Ext4Mount {
    fn sync(&mut self, _ino: u32, _data_only: bool) -> FsResult {
        self.fs.flush().map_err(map_ext4_error)
    }

    fn shutdown(&mut self) -> FsResult {
        self.fs.shutdown_clean().map_err(map_ext4_error)
    }
}

impl LegacyInodeLifecycleOps for Ext4Mount {
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
}

impl LookupOps for ConcurrentExt4Backend {
    fn root_ino(&self) -> u32 {
        EXT4_ROOT_INO
    }

    #[cfg(any(feature = "fanotify", feature = "inotify"))]
    fn overlay_real_node(&self, _ino: u32) -> Option<super::vfs::VfsNodeId> {
        None
    }

    fn lookup_component_from(
        &self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)> {
        self.with_reader(BackendOp::Lookup, parent_ino, |reader| {
            reader.lookup_component_from_shared(parent_ino, component)
        })
    }

    fn readlink(&self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        self.with_reader(BackendOp::Readlink, ino, |reader| {
            reader.readlink_shared(ino, buf)
        })
    }

    fn prepare_readlink_plan(&self, ino: u32, len: usize) -> Option<Box<dyn BackendReadPlan>> {
        // This phase reads only inode/mapping metadata. External target data
        // is fetched by the returned pointer-free plan after the reader core
        // lock has been released.
        self.with_reader(BackendOp::ReadPlan, ino, |reader| {
            reader.prepare_readlink_plan_shared(ino, len)
        })
    }

    fn prepare_directory_read_plan(
        &self,
        ino: u32,
        offset: u64,
    ) -> Option<Box<dyn BackendDirectoryReadPlan>> {
        self.with_reader(BackendOp::ReadPlan, ino, |reader| {
            reader.prepare_directory_read_plan_shared(ino, offset)
        })
    }

    fn read_dirent64(&self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        self.with_reader(BackendOp::Readdir, ino, |reader| {
            reader.read_dirent64_shared(ino, offset, buf)
        })
    }

    fn list_root_names(&self) -> Vec<String> {
        if let Some(plan) = self.with_reader(BackendOp::ReadPlan, EXT4_ROOT_INO, |reader| {
            reader.prepare_directory_read_plan_shared(EXT4_ROOT_INO, 0)
        }) {
            return plan
                .execute()
                .expect("failed to execute ext4 root directory snapshot")
                .names();
        }
        self.with_reader(BackendOp::Readdir, EXT4_ROOT_INO, |reader| {
            reader.list_root_names_shared()
        })
    }
}

impl MetadataOps for ConcurrentExt4Backend {
    fn statfs(&self) -> FileSystemStat {
        // Allocator counters are maintained by the canonical writable core;
        // statfs therefore uses the remaining exclusive superblock accessor.
        self.with_writer_read(BackendOp::StatFull, |writer| writer.statfs_shared())
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
        self.with_reader(BackendOp::StatFull, ino, |reader| {
            reader.inode_flags_shared(ino)
        })
    }

    fn set_inode_flags(&self, ino: u32, flags: u32) -> FsResult {
        self.mutate_inode_metadata(ino, Ext4InodeMetadataMutation::Flags(flags))
    }

    fn stat(&self, ino: u32) -> FsResult<FileStat> {
        self.with_reader(BackendOp::StatFull, ino, |reader| reader.stat_shared(ino))
    }

    fn stat_basic(&self, ino: u32) -> FsResult<FileStat> {
        self.with_reader(BackendOp::StatBasic, ino, |reader| {
            reader.stat_basic_shared(ino)
        })
    }
}

impl DataOps for ConcurrentExt4Backend {
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
        self.with_reader(BackendOp::ReadPlan, ino, |reader| {
            reader.prepare_read_plan_shared(ino, offset, len)
        })
    }

    fn read_at(&self, ino: u32, buf: &mut [u8], offset: u64) -> usize {
        self.with_reader(BackendOp::ReadFallback, ino, |reader| {
            reader.read_at_shared(ino, buf, offset)
        })
    }

    fn prepare_write_plan(
        &self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Option<Box<dyn BackendWritePlan>> {
        // Mapping lookup and canonical-cache alias invalidation are short
        // control operations. The returned pointer-free plan owns physical
        // LBA leases while device I/O runs after the core guard is released.
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
}

impl NamespaceOps for ConcurrentExt4Backend {
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
}

impl SyncOps for ConcurrentExt4Backend {
    fn sync(&self, _ino: u32, _data_only: bool) -> FsResult {
        self.sync_current()
    }

    fn shutdown(&self) -> FsResult {
        self.sync_current()?;
        self.mutate(BackendOp::Sync, |writer| writer.shutdown())
    }
}

impl InodeLifecycleOps for ConcurrentExt4Backend {
    fn retain_inode(&self, ino: u32) -> FsResult {
        self.with_reader(BackendOp::InodeLifetime, ino, |reader| {
            let mut attr = lwext4_rust::FileAttr::default();
            reader.fs.get_attr(ino, &mut attr).map_err(map_ext4_error)?;
            if attr.nlink == 0 || !self.inode_runtime.retain(ino) {
                return Err(FsError::NotFound);
            }
            Ok(())
        })
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
}
