use super::dirent::{
    DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK, DT_UNKNOWN, LINUX_DIRENT64_ALIGN,
    LINUX_DIRENT64_HEADER_SIZE,
};
#[cfg(feature = "perf-counters")]
use super::vfs::BackendIoSnapshot;
use super::vfs::{
    BackendReadPlan, FileSystemStat, FsError, FsNodeKind, FsResult, InodeRelease,
    LegacyFileSystemBackend,
};
use super::{FS_STATX_ATTR_FLAGS, FileStat, FileTimestamp};
use crate::drivers::block::VirtIOBlock;
use crate::perf;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::str;
#[cfg(feature = "perf-counters")]
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use lwext4_rust::ffi::{
    EEXIST, EINVAL, EIO, EISDIR, ENOENT, ENOSPC, ENOTDIR, ENOTEMPTY, ENOTSUP, EXT4_ROOT_INO,
};
use lwext4_rust::{
    BlockDevice as Ext4BlockDevice, EXT4_DEV_BSIZE, Ext4Error, Ext4Filesystem, Ext4Result,
    FsConfig, InodeType, SystemHal,
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
    fn write_blocks(&mut self, block_id: u64, buf: &[u8]) -> Ext4Result<usize> {
        if buf.len() % EXT4_DEV_BSIZE != 0 {
            return Err(Ext4Error::new(EIO as _, "unaligned block write"));
        }
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
        self.dev.read_blocks(block_id as usize, buf);
        #[cfg(feature = "perf-counters")]
        self.io_counters
            .record_read(buf.len() / EXT4_DEV_BSIZE, buf.len());
        perf::record_ext4_block_read(buf.len() / EXT4_DEV_BSIZE, buf.len());
        Ok(buf.len())
    }

    fn num_blocks(&self) -> Ext4Result<u64> {
        Ok(self.dev.num_blocks())
    }
}

type KernelExt4Fs = Ext4Filesystem<KernelHal, KernelDisk>;

const EXT4_CONFIG: FsConfig = FsConfig { bcache_size: 256 };
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
    #[cfg(feature = "perf-counters")]
    io_counters: Arc<Ext4IoCounters>,
    inode_runtime: BTreeMap<u32, Ext4InodeRuntimeState>,
}

#[derive(Default)]
struct Ext4InodeRuntimeState {
    open_count: usize,
    pending_unlink: bool,
    special_rdev: Option<u64>,
}

// SAFETY: the FFI core and its raw pointers move only as one `Ext4Mount`.
// Every dereference happens behind `SerializedBackend.state`; the core itself
// is intentionally not `Sync` and must never be entered by two callers.
unsafe impl Send for Ext4Mount {}

impl Ext4Mount {
    pub(super) fn open(device: Arc<VirtIOBlock>) -> Result<Self, Ext4Error> {
        #[cfg(feature = "perf-counters")]
        let io_counters = Arc::new(Ext4IoCounters::default());
        Ok(Self {
            fs: KernelExt4Fs::new(
                KernelDisk {
                    dev: device.clone(),
                    #[cfg(feature = "perf-counters")]
                    io_counters: io_counters.clone(),
                },
                EXT4_CONFIG,
            )?,
            device,
            #[cfg(feature = "perf-counters")]
            io_counters,
            inode_runtime: BTreeMap::new(),
        })
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
            rdev: self
                .inode_runtime
                .get(&ino)
                .and_then(|state| state.special_rdev)
                .unwrap_or(0),
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

struct Ext4DeviceReadRun {
    buffer_start: usize,
    byte_len: usize,
    device_block: Option<usize>,
}

struct Ext4DeviceReadPlan {
    device: Arc<VirtIOBlock>,
    read_len: usize,
    runs: Vec<Ext4DeviceReadRun>,
}

impl BackendReadPlan for Ext4DeviceReadPlan {
    fn execute(self: Box<Self>, buf: &mut [u8]) -> usize {
        if buf.len() < self.read_len {
            return 0;
        }
        for run in &self.runs {
            let run_buf = &mut buf[run.buffer_start..run.buffer_start + run.byte_len];
            if let Some(device_block) = run.device_block {
                let io = self
                    .device
                    .read_blocks_versioned_fill_for_file_plan(device_block, run_buf);
                perf::record_ext4_read_plan_direct_io(
                    io.device_calls,
                    io.device_blocks,
                    io.device_blocks * EXT4_DEV_BSIZE,
                );
            } else {
                run_buf.fill(0);
            }
        }
        perf::record_ext4_read_plan_executed(self.read_len);
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
            self.inode_runtime.entry(ino).or_default().special_rdev = Some(rdev);
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
        let defer_free = self
            .inode_runtime
            .get(&child_ino)
            .is_some_and(|state| state.open_count > 0);
        drop(lookup);

        // UNFINISHED: Linux also keeps opened directories alive across unlink.
        // This ext4 path currently defers final free only for non-directory
        // inodes, which is enough for mkstemp/unlink/fstat file workloads.
        let deferred = if defer_free {
            self.fs
                .unlink_defer_free(parent_ino, leaf_name)
                .map_err(map_ext4_error)?
        } else {
            self.fs
                .unlink(parent_ino, leaf_name)
                .map_err(map_ext4_error)?;
            let mut attr = lwext4_rust::FileAttr::default();
            if self.fs.get_attr(child_ino, &mut attr).is_err() {
                self.inode_runtime.remove(&child_ino);
            }
            None
        };
        if let Some(ino) = deferred {
            self.inode_runtime.entry(ino).or_default().pending_unlink = true;
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
        self.inode_runtime.entry(ino).or_default().open_count += 1;
        Ok(())
    }

    fn release_inode(&mut self, ino: u32) -> FsResult<InodeRelease> {
        let Some(state) = self.inode_runtime.get(&ino) else {
            return Ok(InodeRelease::Retained);
        };
        if state.open_count == 0 {
            return Ok(InodeRelease::Retained);
        }
        if state.open_count > 1 {
            self.inode_runtime
                .get_mut(&ino)
                .expect("ext4 inode runtime state disappeared")
                .open_count -= 1;
            return Ok(InodeRelease::Retained);
        }
        // The final open reference is the point where an unlinked-but-open
        // inode can be physically freed from the ext4 backend.
        if state.pending_unlink {
            self.fs.free_unlinked_inode(ino).map_err(map_ext4_error)?;
            self.inode_runtime.remove(&ino);
            return Ok(InodeRelease::Freed);
        }
        let state = self
            .inode_runtime
            .get_mut(&ino)
            .expect("ext4 inode runtime state disappeared");
        state.open_count = 0;
        if state.special_rdev.is_none() {
            self.inode_runtime.remove(&ino);
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

    fn prepare_read_plan(
        &mut self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Option<Box<dyn BackendReadPlan>> {
        perf::record_ext4_read_plan_attempt();
        let Ok(Some(plan)) = self.fs.plan_aligned_read(ino, len, offset) else {
            perf::record_ext4_read_plan_fallback();
            return None;
        };
        if plan.block_size % EXT4_DEV_BSIZE != 0 {
            perf::record_ext4_read_plan_fallback();
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
                perf::record_ext4_read_plan_fallback();
                return None;
            };
            let Some(byte_len) = run.block_count.checked_mul(plan.block_size) else {
                perf::record_ext4_read_plan_fallback();
                return None;
            };
            if buffer_start
                .checked_add(byte_len)
                .is_none_or(|end| end > plan.read_len)
            {
                perf::record_ext4_read_plan_fallback();
                return None;
            }
            let device_block = if let Some(fs_block) = run.fs_block {
                let Some(device_block) = fs_block
                    .checked_mul(device_blocks_per_fs_block as u64)
                    .and_then(|block| usize::try_from(block).ok())
                else {
                    perf::record_ext4_read_plan_fallback();
                    return None;
                };
                let device_blocks = run.block_count * device_blocks_per_fs_block;
                if device_block
                    .checked_add(device_blocks)
                    .is_none_or(|end| end > self.device.num_blocks() as usize)
                {
                    perf::record_ext4_read_plan_fallback();
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
        perf::record_ext4_read_plan_prepared(data_runs, data_blocks, zero_runs, zero_blocks);
        Some(Box::new(Ext4DeviceReadPlan {
            device: self.device.clone(),
            read_len: plan.read_len,
            runs,
        }))
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
