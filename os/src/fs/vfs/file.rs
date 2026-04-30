use super::super::devfs;
use super::super::inode::OpenFlags;
use super::super::mount::with_mount;
use super::super::path::WorkingDir;
use super::super::status_flags::StatusFlagsCell;
use super::super::{File, FileStat};
use super::path::{self as vfs_path, LookupMode, VfsOpenTarget};
use super::{FsError, FsNodeKind, FsResult, VfsNodeId, VfsPath};
use crate::mm::UserBuffer;
use crate::sync::SleepMutex;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

pub(crate) struct VfsFile {
    node: VfsNodeId,
    kind: FsNodeKind,
    offset: SleepMutex<usize>,
    readable: bool,
    writable: bool,
    status_flags: StatusFlagsCell,
}

impl VfsFile {
    fn new(path: VfsPath, readable: bool, writable: bool, status_flags: OpenFlags) -> Self {
        Self {
            node: path.node,
            kind: path.kind,
            offset: SleepMutex::new(0),
            readable,
            writable,
            status_flags: StatusFlagsCell::new(status_flags),
        }
    }

    pub(crate) fn read_all(&self) -> Vec<u8> {
        let mut offset = self.offset.lock();
        let mut buffer = [0u8; 4096];
        let mut data = Vec::new();
        loop {
            let len = with_mount(self.node.mount_id, |mount| {
                mount.read_at(self.node.ino, &mut buffer, *offset as u64)
            })
            .expect("filesystem mount is missing");
            if len == 0 {
                break;
            }
            *offset += len;
            data.extend_from_slice(&buffer[..len]);
        }
        data
    }

    fn write_inner(&self, buf: UserBuffer, append: bool) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        let mut offset = self.offset.lock();
        if append {
            let stat = match with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino)) {
                Some(Ok(stat)) => stat,
                _ => {
                    return 0;
                }
            };
            *offset = stat.size as usize;
        }
        let mut total_write_size = 0usize;
        for slice in buf.buffers.iter() {
            let write_size = with_mount(self.node.mount_id, |mount| {
                mount.write_at(self.node.ino, slice, *offset as u64)
            })
            .expect("filesystem mount is missing");
            *offset += write_size;
            total_write_size += write_size;
        }
        total_write_size
    }
}

fn open_vfs_file_impl(
    cwd: Option<WorkingDir>,
    name: &str,
    flags: OpenFlags,
) -> FsResult<Arc<VfsFile>> {
    let resolved = vfs_path::resolve_open(cwd, name, flags.contains(OpenFlags::CREATE))?;

    let (path, readable, writable) = match resolved {
        VfsOpenTarget::Existing(path) => {
            if flags.contains(OpenFlags::CREATE | OpenFlags::EXCL) {
                return Err(FsError::AlreadyExists);
            }
            if path.kind == FsNodeKind::Directory {
                if !flags.can_open_directory() {
                    return Err(FsError::IsDir);
                }
                (path, false, false)
            } else {
                if flags.contains(OpenFlags::DIRECTORY) {
                    return Err(FsError::NotDir);
                }
                if path.kind == FsNodeKind::Symlink && flags.contains(OpenFlags::NOFOLLOW) {
                    return Err(FsError::Loop);
                }
                if path.kind == FsNodeKind::Symlink {
                    // UNFINISHED: Linux openat follows final symlinks unless O_NOFOLLOW
                    // or O_PATH says otherwise. This VFS does not read symlink targets yet.
                }
                let (readable, writable) = flags.read_write();
                if flags.contains(OpenFlags::TRUNC) && flags.writable_target() {
                    with_mount(path.node.mount_id, |mount| mount.set_len(path.node.ino, 0))
                        .ok_or(FsError::Io)??;
                }
                (path, readable, writable)
            }
        }
        VfsOpenTarget::Create(target) => {
            if flags.contains(OpenFlags::DIRECTORY) {
                return Err(FsError::InvalidInput);
            }
            let ino = with_mount(target.parent.mount_id, |mount| {
                mount.create_file(target.parent.ino, target.leaf_name)
            })
            .ok_or(FsError::Io)??;
            let (readable, writable) = flags.read_write();
            (
                VfsPath::new(
                    VfsNodeId::new(target.parent.mount_id, ino),
                    FsNodeKind::RegularFile,
                ),
                readable,
                writable,
            )
        }
    };

    Ok(Arc::new(VfsFile::new(
        path,
        readable,
        writable,
        OpenFlags::file_status_flags(flags),
    )))
}

pub(crate) fn open_file(name: &str, flags: OpenFlags) -> FsResult<Arc<VfsFile>> {
    open_vfs_file_impl(None, name, flags)
}

pub(crate) fn open_file_at(
    cwd: WorkingDir,
    name: &str,
    flags: OpenFlags,
) -> FsResult<Arc<dyn File + Send + Sync>> {
    if let Some(file) = devfs::open(name, flags) {
        return Ok(file);
    }
    open_vfs_file_impl(Some(cwd), name, flags).map(|file| file as Arc<dyn File + Send + Sync>)
}

pub(crate) fn stat_at(cwd: WorkingDir, name: &str) -> FsResult<FileStat> {
    if let Some(stat) = devfs::stat(name) {
        return Ok(stat);
    }
    let path = vfs_path::resolve_existing(Some(cwd), name, LookupMode::Normal)?;
    let mut stat =
        with_mount(path.node.mount_id, |mount| mount.stat(path.node.ino)).ok_or(FsError::Io)??;
    stat.dev = path.node.mount_id.0 as u64;
    Ok(stat)
}

pub(crate) fn lookup_dir_at(cwd: WorkingDir, name: &str) -> FsResult<WorkingDir> {
    vfs_path::resolve_existing(Some(cwd), name, LookupMode::Normal)?
        .working_dir()
        .ok_or(FsError::NotDir)
}

impl File for VfsFile {
    fn readable(&self) -> bool {
        self.readable
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn read(&self, mut buf: UserBuffer) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        let mut offset = self.offset.lock();
        let mut total_read_size = 0usize;
        for slice in buf.buffers.iter_mut() {
            let read_size = with_mount(self.node.mount_id, |mount| {
                mount.read_at(self.node.ino, slice, *offset as u64)
            })
            .expect("filesystem mount is missing");
            if read_size == 0 {
                break;
            }
            *offset += read_size;
            total_read_size += read_size;
        }
        total_read_size
    }

    fn write(&self, buf: UserBuffer) -> usize {
        self.write_inner(buf, false)
    }

    fn write_append(&self, buf: UserBuffer) -> usize {
        self.write_inner(buf, true)
    }

    fn stat(&self) -> FileStat {
        let mut stat = with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino))
            .expect("filesystem mount is missing")
            .expect("inode stat failed");
        stat.dev = self.node.mount_id.0 as u64;
        stat
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        with_mount(self.node.mount_id, |mount| {
            mount.read_at(self.node.ino, buf, offset as u64)
        })
        .expect("filesystem mount is missing")
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        with_mount(self.node.mount_id, |mount| {
            mount.write_at(self.node.ino, buf, offset as u64)
        })
        .expect("filesystem mount is missing")
    }

    fn read_dirent64(&self, user_buf: UserBuffer) -> FsResult<isize> {
        if self.kind != FsNodeKind::Directory {
            return Err(FsError::NotDir);
        }
        let mut offset = self.offset.lock();
        let mut kernel_buf = vec![0u8; user_buf.len()];
        let (read_size, next_offset) = with_mount(self.node.mount_id, |mount| {
            mount.read_dirent64(self.node.ino, *offset as u64, &mut kernel_buf)
        })
        .ok_or(FsError::Io)??;
        if read_size == 0 {
            return Ok(0);
        }
        for (idx, byte_ref) in user_buf.into_iter().take(read_size).enumerate() {
            unsafe {
                *byte_ref = kernel_buf[idx];
            }
        }
        *offset = next_offset as usize;
        Ok(read_size as isize)
    }

    fn working_dir(&self) -> Option<WorkingDir> {
        if self.kind != FsNodeKind::Directory {
            return None;
        }
        Some(WorkingDir::new(self.node.mount_id, self.node.ino))
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }
}
