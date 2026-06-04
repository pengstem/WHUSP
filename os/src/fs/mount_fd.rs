use super::mount::{
    MountError, MountNamespaceId, create_detached_tmpfs_mount, mount_bind_at, mount_detached_fs_at,
    with_mount,
};
use super::path::WorkingDir;
use super::status_flags::StatusFlagsCell;
use super::vfs::VfsNodeId;
use super::{File, FileStat, FsError, FsResult, OpenFlags, PollEvents, S_IFREG};
use crate::mm::UserBuffer;
use crate::sync::SleepMutex;
use alloc::string::String;
use alloc::sync::Arc;
use core::any::Any;

// The fd-backed mount API can receive many fsconfig entries before fsmount().
// Cap the stored compatibility metadata so an unmounted context cannot grow
// unbounded kernel state.
const FSCONFIG_LEGACY_BUFFER_LIMIT: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FsContextStateError {
    NotCreated,
    AlreadyMounted,
}

#[derive(Clone, Debug)]
pub(crate) struct FsContextMountSpec {
    pub(crate) fs_type: String,
    pub(crate) source: String,
}

#[derive(Clone, Debug)]
struct FsContextState {
    fs_type: String,
    source: Option<String>,
    config_len: usize,
    // FSCONFIG_CMD_CREATE gates fsmount(); mounted makes the context one-shot.
    created: bool,
    mounted: bool,
}

pub(crate) struct FsContextFile {
    inner: SleepMutex<FsContextState>,
    status_flags: StatusFlagsCell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DetachedMountKind {
    Bind { recursive: bool },
    NewFilesystem,
}

#[derive(Clone, Debug)]
struct DetachedMountState {
    source: WorkingDir,
    source_path: String,
    kind: DetachedMountKind,
    attached: bool,
}

pub(crate) struct DetachedMountFile {
    inner: SleepMutex<DetachedMountState>,
    status_flags: StatusFlagsCell,
}

impl FsContextFile {
    pub(crate) fn new(fs_type: String) -> Arc<Self> {
        Arc::new(Self {
            inner: SleepMutex::new(FsContextState {
                fs_type,
                source: None,
                config_len: 0,
                created: false,
                mounted: false,
            }),
            status_flags: StatusFlagsCell::new(OpenFlags::empty()),
        })
    }

    pub(crate) fn set_flag(&self, key: &str) -> bool {
        let entry_len = key.len().saturating_add(2);
        let mut inner = self.inner.lock();
        let Some(next_len) = inner.config_len.checked_add(entry_len) else {
            return false;
        };
        if next_len > FSCONFIG_LEGACY_BUFFER_LIMIT {
            return false;
        }
        inner.config_len = next_len;
        true
    }

    pub(crate) fn set_string(&self, key: &str, value: &str) -> bool {
        let entry_len = key.len().saturating_add(value.len()).saturating_add(2);
        let mut inner = self.inner.lock();
        let Some(next_len) = inner.config_len.checked_add(entry_len) else {
            return false;
        };
        if next_len > FSCONFIG_LEGACY_BUFFER_LIMIT {
            return false;
        }
        inner.config_len = next_len;
        if key == "source" {
            inner.source = Some(String::from(value));
        }
        true
    }

    pub(crate) fn mark_created(&self) {
        self.inner.lock().created = true;
    }

    pub(crate) fn prepare_mount(&self) -> Result<FsContextMountSpec, FsContextStateError> {
        let mut inner = self.inner.lock();
        if !inner.created {
            return Err(FsContextStateError::NotCreated);
        }
        if inner.mounted {
            return Err(FsContextStateError::AlreadyMounted);
        }
        inner.mounted = true;
        Ok(FsContextMountSpec {
            fs_type: inner.fs_type.clone(),
            source: inner.source.clone().unwrap_or_else(|| String::from("none")),
        })
    }
}

impl DetachedMountFile {
    pub(crate) fn new_bind(source: WorkingDir, source_path: String, recursive: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: SleepMutex::new(DetachedMountState {
                source,
                source_path,
                kind: DetachedMountKind::Bind { recursive },
                attached: false,
            }),
            status_flags: StatusFlagsCell::new(OpenFlags::PATH),
        })
    }

    pub(crate) fn new_tmpfs(source: String, read_only: bool) -> Result<Arc<Self>, MountError> {
        let source = create_detached_tmpfs_mount(source, read_only)?;
        Ok(Arc::new(Self {
            inner: SleepMutex::new(DetachedMountState {
                source,
                source_path: String::from("<detached-tmpfs>"),
                kind: DetachedMountKind::NewFilesystem,
                attached: false,
            }),
            status_flags: StatusFlagsCell::new(OpenFlags::PATH),
        }))
    }

    pub(crate) fn attach_to(
        &self,
        namespace_id: MountNamespaceId,
        target: WorkingDir,
        target_path: &str,
    ) -> Result<(), MountError> {
        let mut inner = self.inner.lock();
        if inner.attached {
            return Err(MountError::TargetBusy);
        }
        let result = match inner.kind {
            DetachedMountKind::Bind { recursive } => mount_bind_at(
                namespace_id,
                inner.source,
                target,
                inner.source_path.as_str(),
                target_path,
                recursive,
            ),
            DetachedMountKind::NewFilesystem => mount_detached_fs_at(
                namespace_id,
                inner.source,
                target,
                inner.source_path.as_str(),
                target_path,
            ),
        };
        if result.is_ok() {
            inner.attached = true;
        }
        result
    }

    fn source_node(&self) -> VfsNodeId {
        let inner = self.inner.lock();
        VfsNodeId::new(inner.source.mount_id(), inner.source.ino())
    }
}

impl File for FsContextFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        false
    }

    fn writable(&self) -> bool {
        false
    }

    fn read(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn write(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn stat(&self) -> FsResult<FileStat> {
        Ok(FileStat::with_mode(S_IFREG | 0o600))
    }

    fn poll(&self, _events: PollEvents) -> PollEvents {
        PollEvents::empty()
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }
}

impl File for DetachedMountFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        false
    }

    fn writable(&self) -> bool {
        false
    }

    fn read(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn write(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn stat(&self) -> FsResult<FileStat> {
        let node = self.source_node();
        with_mount(node.mount_id, |mount| mount.stat(node.ino)).ok_or(FsError::Io)?
    }

    fn poll(&self, _events: PollEvents) -> PollEvents {
        PollEvents::empty()
    }

    fn working_dir(&self) -> Option<WorkingDir> {
        Some(self.inner.lock().source)
    }

    fn vfs_node_id(&self) -> Option<VfsNodeId> {
        Some(self.source_node())
    }

    fn vfs_mount_id(&self) -> Option<super::mount::MountId> {
        Some(self.inner.lock().source.mount_id())
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }
}
