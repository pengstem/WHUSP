use super::super::{FileStat, FileTimestamp};
use super::FsError;
use super::FsResult;
use super::VfsNodeId;
use crate::perf;
use crate::sync::SleepMutex;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

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
    fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> usize;
    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)>;
    fn list_root_names(&mut self) -> Vec<String>;
}

/// Shared backend facade used by mounted filesystems.
///
/// The logical API takes `&self`; implementations choose their own locking.
/// `SerializedBackend` keeps every legacy backend fully serialized while the
/// VFS migrates away from the compatibility execution bridge below.
#[allow(dead_code)]
pub(crate) trait ConcurrentFileSystemBackend: Send + Sync {
    #[cfg(feature = "perf-counters")]
    fn io_snapshot(&self) -> BackendIoSnapshot;

    fn root_ino(&self) -> u32;
    fn overlay_real_node(&self, ino: u32) -> Option<VfsNodeId>;
    fn statfs(&self) -> FileSystemStat;
    fn lookup_component_from(
        &self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)>;
    fn create_file(&self, parent_ino: u32, leaf_name: &str) -> FsResult<u32>;
    fn create_node(
        &self,
        parent_ino: u32,
        leaf_name: &str,
        kind: FsNodeKind,
        mode: u32,
        rdev: u64,
    ) -> FsResult<u32>;
    fn create_dir(&self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32>;
    fn link(&self, parent_ino: u32, leaf_name: &str, child_ino: u32) -> FsResult;
    fn symlink(&self, parent_ino: u32, leaf_name: &str, target: &[u8]) -> FsResult;
    fn unlink(&self, parent_ino: u32, leaf_name: &str) -> FsResult;
    fn rename(&self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult;
    fn exchange(&self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult;
    fn check_write_at(&self, ino: u32, offset: u64, len: usize) -> FsResult;
    fn check_set_len(&self, ino: u32, len: u64) -> FsResult;
    fn set_len(&self, ino: u32, len: u64) -> FsResult;
    fn allocate_range(&self, ino: u32, offset: u64, len: u64, keep_size: bool) -> FsResult;
    fn zero_range(&self, ino: u32, offset: u64, len: u64, keep_size: bool) -> FsResult;
    fn punch_hole(&self, ino: u32, offset: u64, len: u64) -> FsResult;
    fn sync(&self, ino: u32, data_only: bool) -> FsResult;
    fn shutdown(&self) -> FsResult;
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
    fn retain_inode(&self, ino: u32) -> FsResult;
    fn release_inode(&self, ino: u32) -> FsResult<InodeRelease>;
    fn assign_cgroup_pid(&self, dir_ino: u32, pid: usize) -> FsResult;
    fn stat(&self, ino: u32) -> FsResult<FileStat>;
    fn stat_basic(&self, ino: u32) -> FsResult<FileStat>;
    fn readlink(&self, ino: u32, buf: &mut [u8]) -> FsResult<usize>;
    fn supports_read_snapshot(&self, ino: u32) -> bool;
    fn read_snapshot(&self, ino: u32) -> Option<FsResult<Vec<u8>>>;
    fn prepare_read_plan(
        &self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Option<Box<dyn BackendReadPlan>>;
    fn read_at(&self, ino: u32, buf: &mut [u8], offset: u64) -> usize;
    fn write_at(&self, ino: u32, buf: &[u8], offset: u64) -> usize;
    fn read_dirent64(&self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)>;
    fn list_root_names(&self) -> Vec<String>;

    /// Compatibility bridge for the existing VFS closure call sites. It keeps
    /// pending-release drain and operation timing inside one legacy lock hold.
    fn execute_serialized(
        &self,
        _op: BackendOp,
        before_operation: &mut dyn FnMut(&mut dyn LegacyFileSystemBackend),
        operation: &mut dyn FnMut(&mut dyn LegacyFileSystemBackend),
    );

    /// Nonblocking variant used only by drop-time inode cleanup.
    fn try_execute_serialized(
        &self,
        op: BackendOp,
        before_operation: &mut dyn FnMut(&mut dyn LegacyFileSystemBackend),
        operation: &mut dyn FnMut(&mut dyn LegacyFileSystemBackend),
    ) -> bool;
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

    #[allow(dead_code)]
    fn call<V>(&self, op: BackendOp, f: impl FnOnce(&mut dyn LegacyFileSystemBackend) -> V) -> V {
        let mut f = Some(f);
        let mut result = None;
        self.execute_serialized(op, &mut |_| {}, &mut |backend| {
            result = Some(f.take().expect("serialized backend operation called twice")(backend));
        });
        result.expect("serialized backend operation was not called")
    }
}

impl ConcurrentFileSystemBackend for SerializedBackend {
    #[cfg(feature = "perf-counters")]
    fn io_snapshot(&self) -> BackendIoSnapshot {
        self.state.lock().io_snapshot()
    }

    fn root_ino(&self) -> u32 {
        self.call(BackendOp::Lookup, |backend| backend.root_ino())
    }

    fn overlay_real_node(&self, ino: u32) -> Option<VfsNodeId> {
        self.call(BackendOp::Lookup, |backend| backend.overlay_real_node(ino))
    }

    fn statfs(&self) -> FileSystemStat {
        self.call(BackendOp::StatFull, |backend| backend.statfs())
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

    fn sync(&self, ino: u32, data_only: bool) -> FsResult {
        self.call(BackendOp::Sync, |backend| backend.sync(ino, data_only))
    }

    fn shutdown(&self) -> FsResult {
        self.call(BackendOp::Sync, |backend| backend.shutdown())
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

    fn readlink(&self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        self.call(BackendOp::Readlink, |backend| backend.readlink(ino, buf))
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

    fn write_at(&self, ino: u32, buf: &[u8], offset: u64) -> usize {
        self.call(BackendOp::Write, |backend| {
            backend.write_at(ino, buf, offset)
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

    fn execute_serialized(
        &self,
        _op: BackendOp,
        before_operation: &mut dyn FnMut(&mut dyn LegacyFileSystemBackend),
        operation: &mut dyn FnMut(&mut dyn LegacyFileSystemBackend),
    ) {
        #[cfg(feature = "perf-counters")]
        let mut backend = {
            perf::record_backend_op_call(_op);
            match self.state.try_lock() {
                Some(backend) => backend,
                None => {
                    perf::record_mount_backend_contended_acquisition();
                    perf::record_backend_op_contended(_op);
                    let wait_scope =
                        perf::time_scope(perf::ProfilePoint::MountBackendContendedWait);
                    let op_wait_scope = perf::time_backend_op_wait(_op);
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
        before_operation(&mut **backend);
        #[cfg(feature = "perf-counters")]
        let io_before = backend.io_snapshot();
        #[cfg(feature = "perf-counters")]
        let op_hold_scope = perf::time_backend_op_hold(_op);
        operation(&mut **backend);
        #[cfg(feature = "perf-counters")]
        {
            drop(op_hold_scope);
            perf::record_backend_op_io(_op, backend.io_snapshot().delta_since(io_before));
        }
        drop(backend);
        drop(hold_scope);
    }

    fn try_execute_serialized(
        &self,
        _op: BackendOp,
        before_operation: &mut dyn FnMut(&mut dyn LegacyFileSystemBackend),
        operation: &mut dyn FnMut(&mut dyn LegacyFileSystemBackend),
    ) -> bool {
        let Some(mut backend) = self.state.try_lock() else {
            return false;
        };
        #[cfg(feature = "perf-counters")]
        {
            perf::record_backend_op_call(_op);
            perf::record_backend_try_successful_call();
        }
        before_operation(&mut **backend);
        #[cfg(feature = "perf-counters")]
        let io_before = backend.io_snapshot();
        #[cfg(feature = "perf-counters")]
        let op_hold_scope = perf::time_backend_op_hold(_op);
        operation(&mut **backend);
        #[cfg(feature = "perf-counters")]
        {
            drop(op_hold_scope);
            perf::record_backend_op_io(_op, backend.io_snapshot().delta_since(io_before));
        }
        true
    }
}
