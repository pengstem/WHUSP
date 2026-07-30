use super::super::{FileStat, FileTimestamp};
use super::super::{
    align_up,
    dirent::{LINUX_DIRENT64_ALIGN, LINUX_DIRENT64_HEADER_SIZE},
};
use super::FsError;
use super::FsResult;
use super::VfsNodeId;
use crate::perf;
use crate::sync::SleepMutex;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::str;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub(crate) enum BackendOp {
    Lookup = 0,
    StatBasic,
    StatFull,
    ReadPlan,
    ReadFallback,
    Readlink,
    Readdir,
    Write,
    TruncateAllocate,
    NamespaceMutation,
    InodeLifetime,
    Sync,
}

#[cfg(feature = "perf-counters")]
impl BackendOp {
    pub(crate) const COUNT: usize = 12;

    pub(crate) const ALL: [Self; Self::COUNT] = [
        Self::Lookup,
        Self::StatBasic,
        Self::StatFull,
        Self::ReadPlan,
        Self::ReadFallback,
        Self::Readlink,
        Self::Readdir,
        Self::Write,
        Self::TruncateAllocate,
        Self::NamespaceMutation,
        Self::InodeLifetime,
        Self::Sync,
    ];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Lookup => "lookup",
            Self::StatBasic => "stat_basic",
            Self::StatFull => "stat_full",
            Self::ReadPlan => "read_plan",
            Self::ReadFallback => "read_fallback",
            Self::Readlink => "readlink",
            Self::Readdir => "readdir",
            Self::Write => "write",
            Self::TruncateAllocate => "truncate_allocate",
            Self::NamespaceMutation => "namespace_mutation",
            Self::InodeLifetime => "inode_lifetime",
            Self::Sync => "sync",
        }
    }

    pub(crate) const fn holds_data_io(self) -> bool {
        matches!(
            self,
            Self::ReadFallback | Self::Readlink | Self::Write | Self::TruncateAllocate
        )
    }
}

#[cfg(feature = "perf-counters")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct BackendIoSnapshot {
    pub(crate) read_calls: usize,
    pub(crate) read_blocks: usize,
    pub(crate) read_bytes: usize,
    pub(crate) write_calls: usize,
    pub(crate) write_blocks: usize,
    pub(crate) write_bytes: usize,
}

#[cfg(feature = "perf-counters")]
impl BackendIoSnapshot {
    pub(crate) fn delta_since(self, before: Self) -> Self {
        Self {
            read_calls: self.read_calls.saturating_sub(before.read_calls),
            read_blocks: self.read_blocks.saturating_sub(before.read_blocks),
            read_bytes: self.read_bytes.saturating_sub(before.read_bytes),
            write_calls: self.write_calls.saturating_sub(before.write_calls),
            write_blocks: self.write_blocks.saturating_sub(before.write_blocks),
            write_bytes: self.write_bytes.saturating_sub(before.write_bytes),
        }
    }
}

pub(crate) trait BackendReadPlan: Send {
    // The caller must validate the VFS content generation before publishing the
    // result because executing a plan intentionally does not hold the backend lock.
    fn execute(self: Box<Self>, buf: &mut [u8]) -> usize;
}

pub(crate) trait BackendWritePlan: Send {
    // The caller must keep the inode mapping-mutation lease until this object
    // is executed or dropped. Implementations may own short integer-LBA
    // reservations, but never a backend/core guard or an FFI pointer.
    fn execute(self: Box<Self>, buf: &[u8]) -> usize;
}

pub(crate) struct BackendDirectoryEntry {
    pub(crate) offset: u64,
    pub(crate) ino: u32,
    pub(crate) d_type: u8,
    pub(crate) name_start: usize,
    pub(crate) name_len: usize,
}

pub(crate) struct BackendDirectorySnapshot {
    pub(crate) entries: Vec<BackendDirectoryEntry>,
    pub(crate) end_offset: u64,
    pub(crate) storage: Vec<u8>,
}

impl BackendDirectorySnapshot {
    fn entry_name(&self, entry: &BackendDirectoryEntry) -> FsResult<&[u8]> {
        let name_end = entry
            .name_start
            .checked_add(entry.name_len)
            .ok_or(FsError::Io)?;
        self.storage
            .get(entry.name_start..name_end)
            .ok_or(FsError::Io)
    }

    pub(crate) fn read_dirent64(&self, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        if offset == self.end_offset {
            return Ok((0, offset));
        }
        let Some(start) = self.entries.iter().position(|entry| entry.offset == offset) else {
            return Err(FsError::InvalidInput);
        };
        let mut written = 0usize;
        let mut next_offset = offset;
        for (index, entry) in self.entries.iter().enumerate().skip(start) {
            let name = self.entry_name(entry)?;
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
            next_offset = self
                .entries
                .get(index + 1)
                .map_or(self.end_offset, |next| next.offset);
            let entry_buf = &mut buf[written..written + d_reclen];
            entry_buf.fill(0);
            entry_buf[0..8].copy_from_slice(&(entry.ino as u64).to_ne_bytes());
            entry_buf[8..16].copy_from_slice(&(next_offset as i64).to_ne_bytes());
            entry_buf[16..18].copy_from_slice(&(d_reclen as u16).to_ne_bytes());
            entry_buf[18] = entry.d_type;
            entry_buf[LINUX_DIRENT64_HEADER_SIZE..LINUX_DIRENT64_HEADER_SIZE + name.len()]
                .copy_from_slice(name);
            written += d_reclen;
        }
        Ok((written, next_offset))
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter_map(|entry| {
                let name = str::from_utf8(self.entry_name(entry).ok()?).unwrap_or("<invalid>");
                (name != "." && name != "..").then(|| name.to_string())
            })
            .collect()
    }
}

pub(crate) trait BackendDirectoryReadPlan: Send {
    fn execute(self: Box<Self>) -> FsResult<BackendDirectorySnapshot>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FsNodeKind {
    Directory,
    RegularFile,
    Symlink,
    Fifo,
    CharacterDevice,
    BlockDevice,
    Socket,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InodeRelease {
    Retained,
    Freed,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FileSystemStat {
    pub(crate) magic: i64,
    pub(crate) block_size: u64,
    pub(crate) blocks: u64,
    pub(crate) free_blocks: u64,
    pub(crate) available_blocks: u64,
    pub(crate) files: u64,
    pub(crate) free_files: u64,
    pub(crate) max_name_len: u64,
    pub(crate) flags: u64,
}

pub(crate) trait LegacyFileSystemBackend: Send {
    #[cfg(feature = "perf-counters")]
    fn io_snapshot(&self) -> BackendIoSnapshot {
        BackendIoSnapshot::default()
    }

    fn root_ino(&self) -> u32 {
        2
    }

    fn overlay_real_node(&mut self, _ino: u32) -> Option<VfsNodeId> {
        None
    }

    fn statfs(&mut self) -> FileSystemStat {
        FileSystemStat {
            magic: 0,
            block_size: 4096,
            blocks: 0,
            free_blocks: 0,
            available_blocks: 0,
            files: 1024,
            free_files: 1024,
            max_name_len: 255,
            flags: 0,
        }
    }

    fn lookup_component_from(
        &mut self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)>;
    fn create_file(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult<u32>;
    fn create_node(
        &mut self,
        parent_ino: u32,
        leaf_name: &str,
        kind: FsNodeKind,
        _mode: u32,
        _rdev: u64,
    ) -> FsResult<u32> {
        match kind {
            FsNodeKind::RegularFile => self.create_file(parent_ino, leaf_name),
            _ => Err(FsError::Unsupported),
        }
    }
    fn create_dir(&mut self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32>;
    fn link(&mut self, parent_ino: u32, leaf_name: &str, child_ino: u32) -> FsResult;
    fn symlink(&mut self, parent_ino: u32, leaf_name: &str, target: &[u8]) -> FsResult;
    fn unlink(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult;
    fn rename(&mut self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult;
    fn exchange(
        &mut self,
        _src_dir: u32,
        _src_name: &str,
        _dst_dir: u32,
        _dst_name: &str,
    ) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn check_write_at(&mut self, _ino: u32, _offset: u64, _len: usize) -> FsResult {
        Ok(())
    }
    fn check_set_len(&mut self, _ino: u32, _len: u64) -> FsResult {
        Ok(())
    }
    fn set_len(&mut self, ino: u32, len: u64) -> FsResult;
    fn allocate_range(&mut self, ino: u32, offset: u64, len: u64, keep_size: bool) -> FsResult {
        let end = offset.checked_add(len).ok_or(FsError::InvalidInput)?;
        if !keep_size {
            let stat = self.stat(ino)?;
            if end > stat.size {
                self.set_len(ino, end)?;
            }
        }
        Ok(())
    }
    fn zero_range(&mut self, _ino: u32, _offset: u64, _len: u64, _keep_size: bool) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn punch_hole(&mut self, _ino: u32, _offset: u64, _len: u64) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn sync(&mut self, _ino: u32, _data_only: bool) -> FsResult {
        Ok(())
    }
    fn shutdown(&mut self) -> FsResult {
        let root_ino = self.root_ino();
        self.sync(root_ino, false)
    }
    fn set_times(
        &mut self,
        _ino: u32,
        _atime: Option<FileTimestamp>,
        _mtime: Option<FileTimestamp>,
        _ctime: FileTimestamp,
    ) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn set_mode(&mut self, _ino: u32, _mode: u32) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn set_owner(&mut self, _ino: u32, _uid: Option<u32>, _gid: Option<u32>) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn inode_flags(&mut self, _ino: u32) -> FsResult<u32> {
        Err(FsError::Unsupported)
    }
    fn set_inode_flags(&mut self, _ino: u32, _flags: u32) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn retain_inode(&mut self, ino: u32) -> FsResult {
        self.stat(ino).map(|_| ())
    }
    fn release_inode(&mut self, _ino: u32) -> FsResult<InodeRelease> {
        Ok(InodeRelease::Retained)
    }
    fn assign_cgroup_pid(&mut self, _dir_ino: u32, _pid: usize) -> FsResult {
        Err(FsError::InvalidInput)
    }
    fn stat(&mut self, ino: u32) -> FsResult<FileStat>;
    fn stat_basic(&mut self, ino: u32) -> FsResult<FileStat> {
        self.stat(ino)
    }
    fn readlink(&mut self, ino: u32, buf: &mut [u8]) -> FsResult<usize>;
    fn prepare_readlink_plan(
        &mut self,
        _ino: u32,
        _len: usize,
    ) -> Option<Box<dyn BackendReadPlan>> {
        None
    }
    fn supports_read_snapshot(&mut self, _ino: u32) -> bool {
        false
    }
    fn read_snapshot(&mut self, _ino: u32) -> Option<FsResult<Vec<u8>>> {
        None
    }
    fn prepare_read_plan(
        &mut self,
        _ino: u32,
        _offset: u64,
        _len: usize,
    ) -> Option<Box<dyn BackendReadPlan>> {
        None
    }
    fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> usize;
    fn prepare_write_plan(
        &mut self,
        _ino: u32,
        _offset: u64,
        _len: usize,
    ) -> Option<Box<dyn BackendWritePlan>> {
        None
    }
    fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> usize;
    fn prepare_directory_read_plan(
        &mut self,
        _ino: u32,
        _offset: u64,
    ) -> Option<Box<dyn BackendDirectoryReadPlan>> {
        None
    }
    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)>;
    fn list_root_names(&mut self) -> Vec<String>;
}

#[allow(dead_code)]
pub(crate) trait LookupOps: Send + Sync {
    fn root_ino(&self) -> u32;
    fn overlay_real_node(&self, ino: u32) -> Option<VfsNodeId>;
    fn lookup_component_from(
        &self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)>;
    fn readlink(&self, ino: u32, buf: &mut [u8]) -> FsResult<usize>;
    fn prepare_readlink_plan(&self, ino: u32, len: usize) -> Option<Box<dyn BackendReadPlan>>;
    fn prepare_directory_read_plan(
        &self,
        ino: u32,
        offset: u64,
    ) -> Option<Box<dyn BackendDirectoryReadPlan>>;
    fn read_dirent64(&self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)>;
    fn list_root_names(&self) -> Vec<String>;
}

#[allow(dead_code)]
pub(crate) trait MetadataOps: Send + Sync {
    fn statfs(&self) -> FileSystemStat;
    fn set_times(
        &self,
        ino: u32,
        atime: Option<FileTimestamp>,
        mtime: Option<FileTimestamp>,
        ctime: FileTimestamp,
    ) -> FsResult;
    fn set_mode(&self, ino: u32, mode: u32) -> FsResult;
    fn set_owner(&self, ino: u32, uid: Option<u32>, gid: Option<u32>) -> FsResult;
    fn inode_flags(&self, ino: u32) -> FsResult<u32>;
    fn set_inode_flags(&self, ino: u32, flags: u32) -> FsResult;
    fn assign_cgroup_pid(&self, dir_ino: u32, pid: usize) -> FsResult;
    fn stat(&self, ino: u32) -> FsResult<FileStat>;
    fn stat_basic(&self, ino: u32) -> FsResult<FileStat>;
}

#[allow(dead_code)]
pub(crate) trait DataOps: Send + Sync {
    fn check_write_at(&self, ino: u32, offset: u64, len: usize) -> FsResult;
    fn check_set_len(&self, ino: u32, len: u64) -> FsResult;
    fn set_len(&self, ino: u32, len: u64) -> FsResult;
    fn allocate_range(&self, ino: u32, offset: u64, len: u64, keep_size: bool) -> FsResult;
    fn zero_range(&self, ino: u32, offset: u64, len: u64, keep_size: bool) -> FsResult;
    fn punch_hole(&self, ino: u32, offset: u64, len: u64) -> FsResult;
    fn supports_read_snapshot(&self, ino: u32) -> bool;
    fn read_snapshot(&self, ino: u32) -> Option<FsResult<Vec<u8>>>;
    fn prepare_read_plan(
        &self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Option<Box<dyn BackendReadPlan>>;
    fn read_at(&self, ino: u32, buf: &mut [u8], offset: u64) -> usize;
    fn prepare_write_plan(
        &self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Option<Box<dyn BackendWritePlan>>;
    fn write_at(&self, ino: u32, buf: &[u8], offset: u64) -> usize;
}

#[allow(dead_code)]
pub(crate) trait NamespaceOps: Send + Sync {
    fn create_file(&self, parent_ino: u32, leaf_name: &str) -> FsResult<u32>;
    fn create_node(
        &self,
        parent_ino: u32,
        leaf_name: &str,
        kind: FsNodeKind,
        mode: u32,
        rdev: u64,
    ) -> FsResult<u32>;
    fn create_node_with_owner(
        &self,
        parent_ino: u32,
        leaf_name: &str,
        kind: FsNodeKind,
        mode: u32,
        rdev: u64,
        uid: u32,
        gid: u32,
    ) -> FsResult<u32>;
    fn create_dir(&self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32>;
    fn link(&self, parent_ino: u32, leaf_name: &str, child_ino: u32) -> FsResult;
    fn symlink(&self, parent_ino: u32, leaf_name: &str, target: &[u8]) -> FsResult;
    fn unlink(&self, parent_ino: u32, leaf_name: &str) -> FsResult;
    fn rename(&self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult;
    fn exchange(&self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult;
}

#[allow(dead_code)]
pub(crate) trait SyncOps: Send + Sync {
    fn sync(&self, ino: u32, data_only: bool) -> FsResult;
    fn shutdown(&self) -> FsResult;
}

#[allow(dead_code)]
pub(crate) trait InodeLifecycleOps: Send + Sync {
    fn retain_inode(&self, ino: u32) -> FsResult;
    fn release_inode(&self, ino: u32) -> FsResult<InodeRelease>;
    /// Best-effort drop-time release. `None` means that completing the release
    /// would block and the caller must enqueue it for a later blocking drain.
    fn try_release_inode(&self, ino: u32) -> Option<FsResult<InodeRelease>>;
}

/// Shared backend facade used by mounted filesystems.
///
/// The logical API takes `&self`; implementations choose their own locking.
#[allow(dead_code)]
pub(crate) trait FileSystemBackend:
    LookupOps + MetadataOps + DataOps + NamespaceOps + SyncOps + InodeLifecycleOps
{
}

impl<T> FileSystemBackend for T where
    T: LookupOps + MetadataOps + DataOps + NamespaceOps + SyncOps + InodeLifecycleOps + ?Sized
{
}

pub(crate) struct SerializedBackend {
    state: SleepMutex<Box<dyn LegacyFileSystemBackend>>,
}

impl SerializedBackend {
    pub(crate) fn new(backend: Box<dyn LegacyFileSystemBackend>) -> Self {
        Self {
            state: SleepMutex::new(backend),
        }
    }

    fn call<V>(&self, op: BackendOp, f: impl FnOnce(&mut dyn LegacyFileSystemBackend) -> V) -> V {
        let _ = op;
        #[cfg(feature = "perf-counters")]
        let mut backend = {
            perf::record_backend_op_call(op);
            match self.state.try_lock() {
                Some(backend) => backend,
                None => {
                    perf::record_mount_backend_contended_acquisition();
                    perf::record_backend_op_contended(op);
                    let wait_scope =
                        perf::time_scope(perf::ProfilePoint::MountBackendContendedWait);
                    let op_wait_scope = perf::time_backend_op_wait(op);
                    let backend = self.state.lock();
                    drop(op_wait_scope);
                    drop(wait_scope);
                    backend
                }
            }
        };
        #[cfg(not(feature = "perf-counters"))]
        let mut backend = self.state.lock();
        let hold_scope = perf::time_scope(perf::ProfilePoint::MountBackendHold);
        #[cfg(feature = "perf-counters")]
        let io_before = backend.io_snapshot();
        #[cfg(feature = "perf-counters")]
        let op_hold_scope = perf::time_backend_op_hold(op);
        let result = f(&mut **backend);
        #[cfg(feature = "perf-counters")]
        {
            drop(op_hold_scope);
            perf::record_backend_op_io(op, backend.io_snapshot().delta_since(io_before));
        }
        drop(backend);
        drop(hold_scope);
        result
    }
}

impl LookupOps for SerializedBackend {
    fn root_ino(&self) -> u32 {
        self.call(BackendOp::Lookup, |backend| backend.root_ino())
    }

    fn overlay_real_node(&self, ino: u32) -> Option<VfsNodeId> {
        self.call(BackendOp::Lookup, |backend| backend.overlay_real_node(ino))
    }

    fn lookup_component_from(
        &self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)> {
        self.call(BackendOp::Lookup, |backend| {
            backend.lookup_component_from(parent_ino, component)
        })
    }

    fn readlink(&self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        self.call(BackendOp::Readlink, |backend| backend.readlink(ino, buf))
    }

    fn prepare_readlink_plan(&self, ino: u32, len: usize) -> Option<Box<dyn BackendReadPlan>> {
        // Preparing the immutable mapping may fault inode or indirect-block
        // metadata into the backend cache, but it must not read symlink data.
        // Account it with the other metadata-only read-plan operations; the
        // returned plan performs target-data I/O after `call()` releases the
        // serialized backend core.
        self.call(BackendOp::ReadPlan, |backend| {
            backend.prepare_readlink_plan(ino, len)
        })
    }

    fn prepare_directory_read_plan(
        &self,
        ino: u32,
        offset: u64,
    ) -> Option<Box<dyn BackendDirectoryReadPlan>> {
        self.call(BackendOp::ReadPlan, |backend| {
            backend.prepare_directory_read_plan(ino, offset)
        })
    }

    fn read_dirent64(&self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        self.call(BackendOp::Readdir, |backend| {
            backend.read_dirent64(ino, offset, buf)
        })
    }

    fn list_root_names(&self) -> Vec<String> {
        self.call(BackendOp::Readdir, |backend| backend.list_root_names())
    }
}

impl MetadataOps for SerializedBackend {
    fn statfs(&self) -> FileSystemStat {
        self.call(BackendOp::StatFull, |backend| backend.statfs())
    }

    fn set_times(
        &self,
        ino: u32,
        atime: Option<FileTimestamp>,
        mtime: Option<FileTimestamp>,
        ctime: FileTimestamp,
    ) -> FsResult {
        self.call(BackendOp::NamespaceMutation, |backend| {
            backend.set_times(ino, atime, mtime, ctime)
        })
    }

    fn set_mode(&self, ino: u32, mode: u32) -> FsResult {
        self.call(BackendOp::NamespaceMutation, |backend| {
            backend.set_mode(ino, mode)
        })
    }

    fn set_owner(&self, ino: u32, uid: Option<u32>, gid: Option<u32>) -> FsResult {
        self.call(BackendOp::NamespaceMutation, |backend| {
            backend.set_owner(ino, uid, gid)
        })
    }

    fn inode_flags(&self, ino: u32) -> FsResult<u32> {
        self.call(BackendOp::StatFull, |backend| backend.inode_flags(ino))
    }

    fn set_inode_flags(&self, ino: u32, flags: u32) -> FsResult {
        self.call(BackendOp::NamespaceMutation, |backend| {
            backend.set_inode_flags(ino, flags)
        })
    }

    fn assign_cgroup_pid(&self, dir_ino: u32, pid: usize) -> FsResult {
        self.call(BackendOp::NamespaceMutation, |backend| {
            backend.assign_cgroup_pid(dir_ino, pid)
        })
    }

    fn stat(&self, ino: u32) -> FsResult<FileStat> {
        self.call(BackendOp::StatFull, |backend| backend.stat(ino))
    }

    fn stat_basic(&self, ino: u32) -> FsResult<FileStat> {
        self.call(BackendOp::StatBasic, |backend| backend.stat_basic(ino))
    }
}

impl DataOps for SerializedBackend {
    fn check_write_at(&self, ino: u32, offset: u64, len: usize) -> FsResult {
        self.call(BackendOp::Write, |backend| {
            backend.check_write_at(ino, offset, len)
        })
    }

    fn check_set_len(&self, ino: u32, len: u64) -> FsResult {
        self.call(BackendOp::TruncateAllocate, |backend| {
            backend.check_set_len(ino, len)
        })
    }

    fn set_len(&self, ino: u32, len: u64) -> FsResult {
        self.call(BackendOp::TruncateAllocate, |backend| {
            backend.set_len(ino, len)
        })
    }

    fn allocate_range(&self, ino: u32, offset: u64, len: u64, keep_size: bool) -> FsResult {
        self.call(BackendOp::TruncateAllocate, |backend| {
            backend.allocate_range(ino, offset, len, keep_size)
        })
    }

    fn zero_range(&self, ino: u32, offset: u64, len: u64, keep_size: bool) -> FsResult {
        self.call(BackendOp::TruncateAllocate, |backend| {
            backend.zero_range(ino, offset, len, keep_size)
        })
    }

    fn punch_hole(&self, ino: u32, offset: u64, len: u64) -> FsResult {
        self.call(BackendOp::TruncateAllocate, |backend| {
            backend.punch_hole(ino, offset, len)
        })
    }

    fn supports_read_snapshot(&self, ino: u32) -> bool {
        self.call(BackendOp::ReadPlan, |backend| {
            backend.supports_read_snapshot(ino)
        })
    }

    fn read_snapshot(&self, ino: u32) -> Option<FsResult<Vec<u8>>> {
        self.call(BackendOp::ReadFallback, |backend| {
            backend.read_snapshot(ino)
        })
    }

    fn prepare_read_plan(
        &self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Option<Box<dyn BackendReadPlan>> {
        self.call(BackendOp::ReadPlan, |backend| {
            backend.prepare_read_plan(ino, offset, len)
        })
    }

    fn read_at(&self, ino: u32, buf: &mut [u8], offset: u64) -> usize {
        self.call(BackendOp::ReadFallback, |backend| {
            backend.read_at(ino, buf, offset)
        })
    }

    fn prepare_write_plan(
        &self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Option<Box<dyn BackendWritePlan>> {
        self.call(BackendOp::Write, |backend| {
            backend.prepare_write_plan(ino, offset, len)
        })
    }

    fn write_at(&self, ino: u32, buf: &[u8], offset: u64) -> usize {
        self.call(BackendOp::Write, |backend| {
            backend.write_at(ino, buf, offset)
        })
    }
}

impl NamespaceOps for SerializedBackend {
    fn create_file(&self, parent_ino: u32, leaf_name: &str) -> FsResult<u32> {
        self.call(BackendOp::NamespaceMutation, |backend| {
            backend.create_file(parent_ino, leaf_name)
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
        self.call(BackendOp::NamespaceMutation, |backend| {
            backend.create_node(parent_ino, leaf_name, kind, mode, rdev)
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
        self.call(BackendOp::NamespaceMutation, |backend| {
            let parent_stat = backend.stat(parent_ino)?;
            let ino = backend.create_node(parent_ino, leaf_name, kind, mode, rdev)?;
            let gid = if parent_stat.mode & 0o2000 != 0 {
                parent_stat.gid
            } else {
                gid
            };
            backend.set_owner(ino, Some(uid), Some(gid))?;
            backend.set_mode(ino, mode)?;
            Ok(ino)
        })
    }

    fn create_dir(&self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32> {
        self.call(BackendOp::NamespaceMutation, |backend| {
            backend.create_dir(parent_ino, leaf_name, mode)
        })
    }

    fn link(&self, parent_ino: u32, leaf_name: &str, child_ino: u32) -> FsResult {
        self.call(BackendOp::NamespaceMutation, |backend| {
            backend.link(parent_ino, leaf_name, child_ino)
        })
    }

    fn symlink(&self, parent_ino: u32, leaf_name: &str, target: &[u8]) -> FsResult {
        self.call(BackendOp::NamespaceMutation, |backend| {
            backend.symlink(parent_ino, leaf_name, target)
        })
    }

    fn unlink(&self, parent_ino: u32, leaf_name: &str) -> FsResult {
        self.call(BackendOp::NamespaceMutation, |backend| {
            backend.unlink(parent_ino, leaf_name)
        })
    }

    fn rename(&self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult {
        self.call(BackendOp::NamespaceMutation, |backend| {
            backend.rename(src_dir, src_name, dst_dir, dst_name)
        })
    }

    fn exchange(&self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult {
        self.call(BackendOp::NamespaceMutation, |backend| {
            backend.exchange(src_dir, src_name, dst_dir, dst_name)
        })
    }
}

impl SyncOps for SerializedBackend {
    fn sync(&self, ino: u32, data_only: bool) -> FsResult {
        self.call(BackendOp::Sync, |backend| backend.sync(ino, data_only))
    }

    fn shutdown(&self) -> FsResult {
        self.call(BackendOp::Sync, |backend| backend.shutdown())
    }
}

impl InodeLifecycleOps for SerializedBackend {
    fn retain_inode(&self, ino: u32) -> FsResult {
        self.call(BackendOp::InodeLifetime, |backend| {
            backend.retain_inode(ino)
        })
    }

    fn release_inode(&self, ino: u32) -> FsResult<InodeRelease> {
        self.call(BackendOp::InodeLifetime, |backend| {
            backend.release_inode(ino)
        })
    }

    fn try_release_inode(&self, ino: u32) -> Option<FsResult<InodeRelease>> {
        let mut backend = self.state.try_lock()?;
        #[cfg(feature = "perf-counters")]
        {
            perf::record_backend_op_call(BackendOp::InodeLifetime);
            perf::record_backend_try_successful_call();
        }
        #[cfg(feature = "perf-counters")]
        let io_before = backend.io_snapshot();
        #[cfg(feature = "perf-counters")]
        let op_hold_scope = perf::time_backend_op_hold(BackendOp::InodeLifetime);
        let result = backend.release_inode(ino);
        #[cfg(feature = "perf-counters")]
        {
            drop(op_hold_scope);
            perf::record_backend_op_io(
                BackendOp::InodeLifetime,
                backend.io_snapshot().delta_since(io_before),
            );
        }
        Some(result)
    }
}
