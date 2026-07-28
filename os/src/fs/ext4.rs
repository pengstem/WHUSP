use super::dirent::{
    DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK, DT_UNKNOWN, LINUX_DIRENT64_ALIGN,
    LINUX_DIRENT64_HEADER_SIZE,
};
#[cfg(feature = "perf-counters")]
use super::vfs::BackendIoSnapshot;
use super::vfs::{
    BackendOp, BackendReadPlan, ConcurrentFileSystemBackend, FileSystemStat, FsError, FsNodeKind,
    FsResult, InodeRelease, LegacyFileSystemBackend,
};
use super::{FS_STATX_ATTR_FLAGS, FileStat, FileTimestamp};
use crate::config::MAX_CPUS;
use crate::drivers::block::VirtIOBlock;
use crate::perf;
use crate::sync::{SleepMutex, SleepMutexGuard};
use crate::task::suspend_current_and_run_next;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::{vec, vec::Vec};
use core::str;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use lwext4_rust::ffi::{
    EEXIST, EINVAL, EIO, EISDIR, ENOENT, ENOSPC, ENOTDIR, ENOTEMPTY, ENOTSUP, EXT4_ROOT_INO,
};
use lwext4_rust::{
    BlockDevice as Ext4BlockDevice, EXT4_DEV_BSIZE, Ext4Error, Ext4Filesystem, Ext4MappedReadPlan,
    Ext4Result, Ext4SymlinkReadPlan, FsConfig, InodeType, SystemHal,
};

pub(super) struct KernelHal;

impl SystemHal for KernelHal {
    // UNFINISHED: Linux stat timestamps should reflect filesystem time updates;
    // this HAL currently exposes no wall-clock source to lwext4.
    fn now() -> Option<Duration> {
        None
    }
}

#[derive(Clone)]
pub(super) struct KernelDisk {
    dev: Arc<VirtIOBlock>,
    write_sequence: Arc<Ext4Sequence>,
    #[cfg(feature = "perf-counters")]
    io_counters: Arc<Ext4IoCounters>,
}

/// Sequence counter used where readers copy data into private buffers.
///
/// An uncontended read performs two atomic loads and never enters a scheduler
/// or IRQ-masked lock. A reader that actually overlaps a write discards the
/// private copy and yields until a stable even sequence is visible. There is
/// one writable lwext4 core, so writers are already serialized.
struct Ext4Sequence {
    value: AtomicUsize,
}

impl Ext4Sequence {
    fn new() -> Self {
        Self {
            value: AtomicUsize::new(0),
        }
    }

    fn begin_write(&self) -> Ext4SequenceWriteGuard<'_> {
        let previous = self.value.fetch_add(1, Ordering::AcqRel);
        assert_eq!(previous & 1, 0, "concurrent ext4 sequence writers");
        Ext4SequenceWriteGuard { sequence: self }
    }

    fn stable_value(&self) -> usize {
        loop {
            let value = self.value.load(Ordering::Acquire);
            if value & 1 == 0 {
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
        let previous = self.sequence.value.fetch_add(1, Ordering::Release);
        assert_eq!(previous & 1, 1, "ext4 sequence writer lost ownership");
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
    fn write_blocks(&mut self, block_id: u64, buf: &[u8]) -> Ext4Result<usize> {
        if buf.len() % EXT4_DEV_BSIZE != 0 {
            return Err(Ext4Error::new(EIO as _, "unaligned block write"));
        }
        let _write = self.write_sequence.begin_write();
        self.dev.write_blocks(block_id as usize, buf);
        #[cfg(feature = "perf-counters")]
        self.io_counters
            .record_write(buf.len() / EXT4_DEV_BSIZE, buf.len());
        perf::record_ext4_block_write(buf.len() / EXT4_DEV_BSIZE, buf.len());
        Ok(buf.len())
    }

    fn read_blocks(&mut self, block_id: u64, buf: &mut [u8]) -> Ext4Result<usize> {
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
    write_sequence: Arc<Ext4Sequence>,
    cache_generation: usize,
    #[cfg(feature = "perf-counters")]
    io_counters: Arc<Ext4IoCounters>,
    inode_runtime: Arc<Ext4InodeRuntimeTable>,
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
// Every dereference happens behind the owning read or writer core mutex; the
// core itself is intentionally not `Sync` and must never have two callers.
unsafe impl Send for Ext4Mount {}

impl Ext4Mount {
    pub(super) fn open(device: Arc<VirtIOBlock>) -> Result<Self, Ext4Error> {
        #[cfg(feature = "perf-counters")]
        let io_counters = Arc::new(Ext4IoCounters::default());
        let inode_runtime = Arc::new(Ext4InodeRuntimeTable::new());
        let write_sequence = Arc::new(Ext4Sequence::new());
        Ok(Self {
            fs: KernelExt4Fs::new(
                KernelDisk {
                    dev: device.clone(),
                    write_sequence: write_sequence.clone(),
                    #[cfg(feature = "perf-counters")]
                    io_counters: io_counters.clone(),
                },
                EXT4_CONFIG,
            )?,
            device,
            write_sequence,
            cache_generation: 0,
            #[cfg(feature = "perf-counters")]
            io_counters,
            inode_runtime,
        })
    }

    pub(super) fn open_read_replica(&self) -> Result<Self, Ext4Error> {
        #[cfg(feature = "perf-counters")]
        let io_counters = Arc::new(Ext4IoCounters::default());
        Ok(Self {
            fs: KernelExt4Fs::new_read_only(
                KernelDisk {
                    dev: self.device.clone(),
                    write_sequence: self.write_sequence.clone(),
                    #[cfg(feature = "perf-counters")]
                    io_counters: io_counters.clone(),
                },
                EXT4_CONFIG,
            )?,
            device: self.device.clone(),
            write_sequence: self.write_sequence.clone(),
            cache_generation: self.cache_generation,
            #[cfg(feature = "perf-counters")]
            io_counters,
            inode_runtime: self.inode_runtime.clone(),
        })
    }

    pub(super) fn invalidate_read_cache(&mut self) {
        self.fs.invalidate_clean_cache();
    }

    fn flush_for_replica_visibility(&mut self) -> FsResult {
        self.fs.flush().map_err(map_ext4_error)
    }

    fn mapped_read_plan(
        &self,
        plan: Ext4MappedReadPlan,
        record_regular: bool,
    ) -> Option<Box<dyn BackendReadPlan>> {
        if plan.block_size % EXT4_DEV_BSIZE != 0 {
            if record_regular {
                perf::record_ext4_read_plan_fallback();
            }
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
                if record_regular {
                    perf::record_ext4_read_plan_fallback();
                }
                return None;
            };
            let Some(byte_len) = run.block_count.checked_mul(plan.block_size) else {
                if record_regular {
                    perf::record_ext4_read_plan_fallback();
                }
                return None;
            };
            if buffer_start
                .checked_add(byte_len)
                .is_none_or(|end| end > plan.buffer_len)
            {
                if record_regular {
                    perf::record_ext4_read_plan_fallback();
                }
                return None;
            }
            let device_block = if let Some(fs_block) = run.fs_block {
                let Some(device_block) = fs_block
                    .checked_mul(device_blocks_per_fs_block as u64)
                    .and_then(|block| usize::try_from(block).ok())
                else {
                    if record_regular {
                        perf::record_ext4_read_plan_fallback();
                    }
                    return None;
                };
                let device_blocks = run.block_count * device_blocks_per_fs_block;
                if device_block
                    .checked_add(device_blocks)
                    .is_none_or(|end| end > self.device.num_blocks() as usize)
                {
                    if record_regular {
                        perf::record_ext4_read_plan_fallback();
                    }
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
        Some(Box::new(Ext4DeviceReadPlan {
            device: self.device.clone(),
            write_sequence: self.write_sequence.clone(),
            buffer_len: plan.buffer_len,
            read_offset: plan.read_offset,
            read_len: plan.read_len,
            runs,
            record_regular,
        }))
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

/// One writable lwext4 core plus inode-sharded read-only replicas.
///
/// Each C core owns its raw pointers and bcache and is entered by one task at a
/// time. Read operations touch only one replica lock. Mutations flush the
/// writer and advance `cache_generation`; each replica lazily invalidates its
/// own clean cache before its next operation and retries if a generation
/// changes while it was reading.
pub(super) struct ConcurrentExt4Backend {
    writer: SleepMutex<Ext4Mount>,
    readers: Vec<SleepMutex<Ext4Mount>>,
    inode_runtime: Arc<Ext4InodeRuntimeTable>,
    cache_generation: Ext4Sequence,
}

impl ConcurrentExt4Backend {
    pub(super) fn open(device: Arc<VirtIOBlock>) -> Result<Self, Ext4Error> {
        let writer = Ext4Mount::open(device)?;
        let mut readers = Vec::with_capacity(MAX_CPUS);
        for _ in 0..MAX_CPUS {
            let reader = writer.open_read_replica()?;
            readers.push(SleepMutex::new(reader));
        }
        let inode_runtime = writer.inode_runtime.clone();
        Ok(Self {
            writer: SleepMutex::new(writer),
            readers,
            inode_runtime,
            cache_generation: Ext4Sequence::new(),
        })
    }

    #[inline]
    fn reader_start(&self) -> usize {
        crate::cpu::current_id() % self.readers.len()
    }

    fn lock_core<'a>(
        core: &'a SleepMutex<Ext4Mount>,
        op: BackendOp,
    ) -> SleepMutexGuard<'a, Ext4Mount> {
        let _ = op;
        match core.try_lock() {
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
                let core = core.lock();
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
        mut f: impl FnMut(&mut Ext4Mount, usize) -> V,
    ) -> V {
        let _ = op;
        loop {
            #[cfg(feature = "perf-counters")]
            perf::record_backend_op_call(op);
            let generation = self.cache_generation.stable_value();
            let start = self.reader_start();
            let mut available = None;
            for offset in 0..self.readers.len() {
                let index = (start + offset) % self.readers.len();
                if let Some(reader) = self.readers[index].try_lock() {
                    available = Some(reader);
                    break;
                }
            }
            let mut reader = match available {
                Some(reader) => reader,
                None => {
                    #[cfg(feature = "perf-counters")]
                    {
                        perf::record_mount_backend_contended_acquisition();
                        perf::record_backend_op_contended(op);
                    }
                    let wait_scope =
                        perf::time_scope(perf::ProfilePoint::MountBackendContendedWait);
                    #[cfg(feature = "perf-counters")]
                    let op_wait_scope = perf::time_backend_op_wait(op);
                    let guard = self.readers[start].lock();
                    #[cfg(feature = "perf-counters")]
                    drop(op_wait_scope);
                    drop(wait_scope);
                    guard
                }
            };
            if self.cache_generation.value.load(Ordering::Acquire) != generation {
                drop(reader);
                continue;
            }
            if reader.cache_generation != generation {
                reader.invalidate_read_cache();
                reader.cache_generation = generation;
            }
            let hold_scope = perf::time_scope(perf::ProfilePoint::MountBackendHold);
            #[cfg(feature = "perf-counters")]
            let io_before = reader.io_counters.snapshot();
            #[cfg(feature = "perf-counters")]
            let op_hold_scope = perf::time_backend_op_hold(op);
            let result = f(&mut reader, generation);
            #[cfg(feature = "perf-counters")]
            {
                drop(op_hold_scope);
                perf::record_backend_op_io(
                    op,
                    reader.io_counters.snapshot().delta_since(io_before),
                );
            }
            drop(reader);
            drop(hold_scope);
            if op == BackendOp::InodeLifetime
                || self.cache_generation.value.load(Ordering::Acquire) == generation
            {
                return result;
            }
        }
    }

    fn with_writer_read<V>(&self, op: BackendOp, f: impl FnOnce(&mut Ext4Mount) -> V) -> V {
        let _ = op;
        #[cfg(feature = "perf-counters")]
        perf::record_backend_op_call(op);
        let mut writer = Self::lock_core(&self.writer, op);
        let hold_scope = perf::time_scope(perf::ProfilePoint::MountBackendHold);
        #[cfg(feature = "perf-counters")]
        let io_before = writer.io_counters.snapshot();
        #[cfg(feature = "perf-counters")]
        let op_hold_scope = perf::time_backend_op_hold(op);
        let result = f(&mut writer);
        #[cfg(feature = "perf-counters")]
        {
            drop(op_hold_scope);
            perf::record_backend_op_io(op, writer.io_counters.snapshot().delta_since(io_before));
        }
        drop(writer);
        drop(hold_scope);
        result
    }

    fn with_writer<V>(&self, op: BackendOp, f: impl FnOnce(&mut Ext4Mount) -> V) -> (V, FsResult) {
        let _ = op;
        #[cfg(feature = "perf-counters")]
        perf::record_backend_op_call(op);
        // The writer core serializes lwext4 transactions. Per-object VFS
        // leases protect conflicting operations. The odd generation prevents
        // a replica from publishing a result assembled across this transaction.
        let mut writer = Self::lock_core(&self.writer, op);
        let generation_write = self.cache_generation.begin_write();
        let hold_scope = perf::time_scope(perf::ProfilePoint::MountBackendHold);
        #[cfg(feature = "perf-counters")]
        let io_before = writer.io_counters.snapshot();
        #[cfg(feature = "perf-counters")]
        let op_hold_scope = perf::time_backend_op_hold(op);
        let result = f(&mut writer);
        let visible = writer.flush_for_replica_visibility();
        #[cfg(feature = "perf-counters")]
        {
            drop(op_hold_scope);
            perf::record_backend_op_io(op, writer.io_counters.snapshot().delta_since(io_before));
        }
        drop(writer);
        drop(generation_write);
        drop(hold_scope);
        (result, visible)
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
        let mut writer = self.writer.try_lock()?;
        let generation_write = self.cache_generation.begin_write();
        #[cfg(feature = "perf-counters")]
        {
            perf::record_backend_op_call(op);
            perf::record_backend_try_successful_call();
        }
        #[cfg(feature = "perf-counters")]
        let io_before = writer.io_counters.snapshot();
        #[cfg(feature = "perf-counters")]
        let op_hold_scope = perf::time_backend_op_hold(op);
        let result = f(&mut writer);
        let visible = writer.flush_for_replica_visibility();
        #[cfg(feature = "perf-counters")]
        {
            drop(op_hold_scope);
            perf::record_backend_op_io(op, writer.io_counters.snapshot().delta_since(io_before));
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

struct Ext4DeviceReadPlan {
    device: Arc<VirtIOBlock>,
    write_sequence: Arc<Ext4Sequence>,
    buffer_len: usize,
    read_offset: usize,
    read_len: usize,
    runs: Vec<Ext4DeviceReadRun>,
    record_regular: bool,
}

struct Ext4InlineReadPlan {
    data: Vec<u8>,
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
        self.read_len
    }
}

impl LegacyFileSystemBackend for Ext4Mount {
    #[cfg(feature = "perf-counters")]
    fn io_snapshot(&self) -> BackendIoSnapshot {
        self.io_counters.snapshot()
    }

    fn statfs(&mut self) -> FileSystemStat {
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

    fn lookup_component_from(
        &mut self,
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

    fn create_file(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult<u32> {
        self.fs
            .create(parent_ino, leaf_name, InodeType::RegularFile, 0o644)
            .map_err(map_ext4_error)
    }

    fn create_node(
        &mut self,
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
            // UNFINISHED: The vendored lwext4 wrapper can create special inode
            // types, but it does not yet expose persistent device major/minor
            // payloads. Keep runtime-created rdevs for stat/statx until the
            // wrapper can read/write the on-disk special inode fields.
            self.inode_runtime.set_special_rdev(ino, rdev);
        }
        Ok(ino)
    }

    fn create_dir(&mut self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32> {
        self.fs
            .create(parent_ino, leaf_name, InodeType::Directory, mode)
            .map_err(map_ext4_error)
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
        let stat = self.stat(ino)?;
        let mode = (stat.mode & !0o7777) | (mode & 0o7777);
        self.fs.set_mode(ino, mode).map_err(map_ext4_error)
    }

    fn inode_flags(&mut self, ino: u32) -> FsResult<u32> {
        self.fs.inode_flags(ino).map_err(map_ext4_error)
    }

    fn set_inode_flags(&mut self, ino: u32, flags: u32) -> FsResult {
        self.fs.set_inode_flags(ino, flags).map_err(map_ext4_error)
    }

    fn set_owner(&mut self, ino: u32, uid: Option<u32>, gid: Option<u32>) -> FsResult {
        let stat = self.stat(ino)?;
        let uid = uid.unwrap_or(stat.uid);
        let gid = gid.unwrap_or(stat.gid);
        if uid > u16::MAX as u32 || gid > u16::MAX as u32 {
            // UNFINISHED: The current lwext4 wrapper exposes only the low
            // 16-bit ext4 uid/gid fields, not the high uid/gid fields used for
            // full 32-bit Linux ids.
            return Err(FsError::InvalidInput);
        }
        self.fs
            .set_owner(ino, uid as u16, gid as u16)
            .map_err(map_ext4_error)
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

    fn stat_basic(&mut self, ino: u32) -> FsResult<FileStat> {
        let mut attr = lwext4_rust::FileAttr::default();
        {
            let _profile_scope = perf::time_scope(perf::ProfilePoint::Ext4StatGetAttr);
            self.fs.get_attr(ino, &mut attr).map_err(map_ext4_error)?;
        }
        Ok(self.stat_from_attr(ino, attr, 0, 0))
    }

    fn readlink(&mut self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        self.fs.read_at(ino, buf, 0).map_err(map_ext4_error)
    }

    fn prepare_readlink_plan(&mut self, ino: u32, len: usize) -> Option<Box<dyn BackendReadPlan>> {
        match self.fs.plan_symlink_read(ino, len).ok()?? {
            Ext4SymlinkReadPlan::Inline(data) => Some(Box::new(Ext4InlineReadPlan { data })),
            Ext4SymlinkReadPlan::Mapped(plan) => self.mapped_read_plan(plan, false),
        }
    }

    fn prepare_read_plan(
        &mut self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Option<Box<dyn BackendReadPlan>> {
        perf::record_ext4_read_plan_attempt();
        let Ok(Some(plan)) = self.fs.plan_read(ino, len, offset) else {
            perf::record_ext4_read_plan_fallback();
            return None;
        };
        self.mapped_read_plan(plan, true)
    }

    fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> usize {
        let _profile_scope = perf::time_scope(perf::ProfilePoint::Ext4Read);
        self.fs.read_at(ino, buf, offset).expect("ext4 read failed")
    }

    fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> usize {
        self.fs
            .write_at(ino, buf, offset)
            .expect("ext4 write failed")
    }

    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
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

                // Linux getdents64 returns EINVAL when one record cannot fit; after
                // at least one record, returning a short buffer preserves the next offset.
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

    fn list_root_names(&mut self) -> Vec<String> {
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

impl ConcurrentFileSystemBackend for ConcurrentExt4Backend {
    fn root_ino(&self) -> u32 {
        EXT4_ROOT_INO
    }

    fn overlay_real_node(&self, _ino: u32) -> Option<super::vfs::VfsNodeId> {
        None
    }

    fn statfs(&self) -> FileSystemStat {
        self.with_writer_read(BackendOp::StatFull, |writer| writer.statfs())
    }

    fn lookup_component_from(
        &self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)> {
        self.with_reader(BackendOp::Lookup, parent_ino, |reader, _| {
            reader.lookup_component_from(parent_ino, component)
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
        self.mutate(BackendOp::NamespaceMutation, |writer| {
            let parent_stat = writer.stat(parent_ino)?;
            let ino = writer.create_node(parent_ino, leaf_name, kind, mode, rdev)?;
            let gid = if parent_stat.mode & 0o2000 != 0 {
                parent_stat.gid
            } else {
                gid
            };
            writer.set_owner(ino, Some(uid), Some(gid))?;
            writer.set_mode(ino, mode)?;
            Ok(ino)
        })
    }

    fn create_file(&self, parent_ino: u32, leaf_name: &str) -> FsResult<u32> {
        self.mutate(BackendOp::NamespaceMutation, |writer| {
            writer.create_file(parent_ino, leaf_name)
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
        self.mutate(BackendOp::NamespaceMutation, |writer| {
            writer.create_node(parent_ino, leaf_name, kind, mode, rdev)
        })
    }

    fn create_dir(&self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32> {
        self.mutate(BackendOp::NamespaceMutation, |writer| {
            writer.create_dir(parent_ino, leaf_name, mode)
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
        self.mutate(BackendOp::NamespaceMutation, |writer| {
            writer.set_times(ino, atime, mtime, ctime)
        })
    }

    fn set_mode(&self, ino: u32, mode: u32) -> FsResult {
        self.mutate(BackendOp::NamespaceMutation, |writer| {
            writer.set_mode(ino, mode)
        })
    }

    fn set_owner(&self, ino: u32, uid: Option<u32>, gid: Option<u32>) -> FsResult {
        self.mutate(BackendOp::NamespaceMutation, |writer| {
            writer.set_owner(ino, uid, gid)
        })
    }

    fn inode_flags(&self, ino: u32) -> FsResult<u32> {
        self.with_reader(BackendOp::StatFull, ino, |reader, _| {
            reader.inode_flags(ino)
        })
    }

    fn set_inode_flags(&self, ino: u32, flags: u32) -> FsResult {
        self.mutate(BackendOp::NamespaceMutation, |writer| {
            writer.set_inode_flags(ino, flags)
        })
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
        self.with_reader(BackendOp::StatFull, ino, |reader, _| reader.stat(ino))
    }

    fn stat_basic(&self, ino: u32) -> FsResult<FileStat> {
        self.with_reader(BackendOp::StatBasic, ino, |reader, _| {
            reader.stat_basic(ino)
        })
    }

    fn readlink(&self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        self.with_reader(BackendOp::Readlink, ino, |reader, _| {
            reader.readlink(ino, buf)
        })
    }

    fn prepare_readlink_plan(&self, ino: u32, len: usize) -> Option<Box<dyn BackendReadPlan>> {
        // This phase reads only inode/mapping metadata. External target data
        // is fetched by the returned pointer-free plan after the reader core
        // lock has been released.
        self.with_reader(BackendOp::ReadPlan, ino, |reader, _| {
            reader.prepare_readlink_plan(ino, len)
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
            reader.prepare_read_plan(ino, offset, len)
        })
    }

    fn read_at(&self, ino: u32, buf: &mut [u8], offset: u64) -> usize {
        self.with_reader(BackendOp::ReadFallback, ino, |reader, _| {
            reader.read_at(ino, buf, offset)
        })
    }

    fn write_at(&self, ino: u32, buf: &[u8], offset: u64) -> usize {
        let (written, visible) =
            self.with_writer(BackendOp::Write, |writer| writer.write_at(ino, buf, offset));
        if visible.is_ok() { written } else { 0 }
    }

    fn read_dirent64(&self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        self.with_reader(BackendOp::Readdir, ino, |reader, _| {
            reader.read_dirent64(ino, offset, buf)
        })
    }

    fn list_root_names(&self) -> Vec<String> {
        self.with_reader(BackendOp::Readdir, EXT4_ROOT_INO, |reader, _| {
            reader.list_root_names()
        })
    }
}
