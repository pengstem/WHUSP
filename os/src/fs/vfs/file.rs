use super::super::devfs;
use super::super::inode::OpenFlags;
use super::super::mount::{mount_supports_page_cache, release_inode_from_drop, with_mount};
use super::super::path::{PathContext, WorkingDir};
use super::super::status_flags::StatusFlagsCell;
use super::super::{File, FileStat, FileTimestamp, SeekWhence};
use super::path::{self as vfs_path, LookupMode, VfsOpenTarget};
use super::{FsError, FsNodeKind, FsResult, VfsNodeId, VfsPath};
use crate::mm::{page_cache::PageCacheId, UserBuffer};
use crate::sync::SleepMutex;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

const VFS_WRITE_CHUNK_SIZE: usize = 64 * 1024;
const MODE_PERMISSIONS_MASK: u32 = 0o7777;
const MODE_SETGID: u32 = 0o2000;

#[derive(Clone, Copy, Debug)]
pub(crate) struct FileCreateAttrs {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) mode: u32,
}

pub(crate) struct VfsFile {
    node: VfsNodeId,
    kind: FsNodeKind,
    offset: SleepMutex<usize>,
    readable: bool,
    writable: bool,
    status_flags: StatusFlagsCell,
}

impl VfsFile {
    fn new(
        path: VfsPath,
        readable: bool,
        writable: bool,
        status_flags: OpenFlags,
    ) -> FsResult<Self> {
        with_mount(path.node.mount_id, |mount| {
            mount.retain_inode(path.node.ino)
        })
        .ok_or(FsError::Io)??;
        Ok(Self {
            node: path.node,
            kind: path.kind,
            offset: SleepMutex::new(0),
            readable,
            writable,
            status_flags: StatusFlagsCell::new(status_flags),
        })
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
            let write_size = self.write_at_chunks(*offset, slice);
            *offset = offset.checked_add(write_size).unwrap_or(usize::MAX);
            total_write_size = total_write_size.saturating_add(write_size);
            if write_size < slice.len() {
                break;
            }
        }
        total_write_size
    }

    fn write_at_chunks(&self, offset: usize, buf: &[u8]) -> usize {
        let mut total_write_size = 0usize;
        for chunk in buf.chunks(VFS_WRITE_CHUNK_SIZE) {
            let Some(chunk_offset) = offset.checked_add(total_write_size) else {
                break;
            };
            let write_size = with_mount(self.node.mount_id, |mount| {
                mount.write_at(self.node.ino, chunk, chunk_offset as u64)
            })
            .expect("filesystem mount is missing");
            total_write_size = total_write_size.saturating_add(write_size);
            if write_size < chunk.len() {
                break;
            }
        }
        total_write_size
    }

    fn noatime_snapshot(&self) -> Option<(FileTimestamp, FileTimestamp)> {
        if !self.status_flags.get().contains(OpenFlags::NOATIME) {
            return None;
        }
        let stat = with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino))?.ok()?;
        Some((
            FileTimestamp {
                sec: stat.atime_sec,
                nsec: stat.atime_nsec,
            },
            FileTimestamp {
                sec: stat.ctime_sec,
                nsec: stat.ctime_nsec,
            },
        ))
    }

    fn restore_noatime(&self, snapshot: Option<(FileTimestamp, FileTimestamp)>) {
        if let Some((atime, ctime)) = snapshot {
            let _ = with_mount(self.node.mount_id, |mount| {
                mount.set_times(self.node.ino, Some(atime), None, ctime)
            });
        }
    }
}

fn open_vfs_file_impl(
    context: PathContext,
    name: &str,
    flags: OpenFlags,
    create_attrs: Option<FileCreateAttrs>,
) -> FsResult<Arc<VfsFile>> {
    let follow_final_symlink = !flags.contains(OpenFlags::NOFOLLOW);
    let resolved = vfs_path::resolve_open_in(
        context,
        name,
        follow_final_symlink,
        flags.contains(OpenFlags::CREATE),
    )?;

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
                if path.kind == FsNodeKind::Fifo {
                    let (readable, writable) = flags.read_write();
                    if writable && !readable && flags.contains(OpenFlags::NONBLOCK) {
                        return Err(FsError::NoDeviceOrAddress);
                    }
                    // UNFINISHED: Named FIFO endpoints are not backed by a
                    // shared pipe object yet. This is enough for the Linux
                    // no-reader nonblocking writer check in open06, but real
                    // FIFO data transfer and blocking open pairing are still
                    // incomplete.
                }
                if path.kind == FsNodeKind::Symlink {
                    if flags.contains(OpenFlags::NOFOLLOW) && !flags.contains(OpenFlags::PATH) {
                        return Err(FsError::Loop);
                    }
                    // CONTEXT: readlinkat("", fd) needs an O_PATH|O_NOFOLLOW fd
                    // that refers to the symlink itself; full O_PATH semantics are
                    // intentionally deferred.
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
            let parent_stat = with_mount(target.parent.mount_id, |mount| {
                mount.stat(target.parent.ino)
            })
            .ok_or(FsError::Io)??;
            let ino = with_mount(target.parent.mount_id, |mount| {
                mount.create_file(target.parent.ino, target.leaf_name)
            })
            .ok_or(FsError::Io)??;
            if let Some(attrs) = create_attrs {
                let gid = if parent_stat.mode & MODE_SETGID != 0 {
                    parent_stat.gid
                } else {
                    attrs.gid
                };
                with_mount(target.parent.mount_id, |mount| {
                    mount.set_owner(ino, Some(attrs.uid), Some(gid))
                })
                .ok_or(FsError::Io)??;
                with_mount(target.parent.mount_id, |mount| {
                    mount.set_mode(ino, attrs.mode & MODE_PERMISSIONS_MASK)
                })
                .ok_or(FsError::Io)??;
            }
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
    )?))
}

pub(crate) fn open_file(name: &str, flags: OpenFlags) -> FsResult<Arc<VfsFile>> {
    open_vfs_file_impl(PathContext::global_root(), name, flags, None)
}

pub(crate) fn open_file_in(
    context: PathContext,
    name: &str,
    flags: OpenFlags,
) -> FsResult<Arc<dyn File + Send + Sync>> {
    open_file_in_with_attrs(context, name, flags, None)
}

pub(crate) fn open_file_in_with_attrs(
    context: PathContext,
    name: &str,
    flags: OpenFlags,
    create_attrs: Option<FileCreateAttrs>,
) -> FsResult<Arc<dyn File + Send + Sync>> {
    if context.is_global_root()
        && let Some(file) = devfs::open(name, flags)?
    {
        return Ok(file);
    }
    open_vfs_file_impl(context, name, flags, create_attrs)
        .map(|file| file as Arc<dyn File + Send + Sync>)
}

pub(crate) fn stat_in(
    context: PathContext,
    name: &str,
    follow_final_symlink: bool,
) -> FsResult<FileStat> {
    if context.is_global_root()
        && let Some(stat) = devfs::stat(name)
    {
        return Ok(stat);
    }
    let mode = if follow_final_symlink {
        LookupMode::FollowFinal
    } else {
        LookupMode::NoFollowFinal
    };
    let path = vfs_path::resolve_existing_in(context, name, mode)?;
    let mut stat =
        with_mount(path.node.mount_id, |mount| mount.stat(path.node.ino)).ok_or(FsError::Io)??;
    stat.dev = path.node.mount_id.0 as u64;
    Ok(stat)
}

pub(crate) fn lookup_dir_with_stat_in(
    context: PathContext,
    name: &str,
) -> FsResult<(WorkingDir, FileStat)> {
    let path = vfs_path::resolve_existing_in(context, name, LookupMode::FollowFinal)?;
    if path.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    let mut stat =
        with_mount(path.node.mount_id, |mount| mount.stat(path.node.ino)).ok_or(FsError::Io)??;
    stat.dev = path.node.mount_id.0 as u64;
    Ok((WorkingDir::new(path.node.mount_id, path.node.ino), stat))
}

pub(crate) fn chmod_in(
    context: PathContext,
    name: &str,
    follow_final_symlink: bool,
    mode: u32,
) -> FsResult {
    let lookup_mode = if follow_final_symlink {
        LookupMode::FollowFinal
    } else {
        LookupMode::NoFollowFinal
    };
    let path = vfs_path::resolve_existing_in(context, name, lookup_mode)?;
    with_mount(path.node.mount_id, |mount| {
        mount.set_mode(path.node.ino, mode)
    })
    .ok_or(FsError::Io)?
}

pub(crate) fn chown_in(
    context: PathContext,
    name: &str,
    follow_final_symlink: bool,
    uid: Option<u32>,
    gid: Option<u32>,
) -> FsResult {
    let lookup_mode = if follow_final_symlink {
        LookupMode::FollowFinal
    } else {
        LookupMode::NoFollowFinal
    };
    let path = vfs_path::resolve_existing_in(context, name, lookup_mode)?;
    with_mount(path.node.mount_id, |mount| {
        mount.set_owner(path.node.ino, uid, gid)
    })
    .ok_or(FsError::Io)?
}

pub(crate) fn truncate_in(context: PathContext, name: &str, len: usize) -> FsResult {
    let path = vfs_path::resolve_existing_in(context, name, LookupMode::FollowFinal)?;
    if path.kind == FsNodeKind::Directory {
        return Err(FsError::IsDir);
    }
    if path.kind != FsNodeKind::RegularFile {
        return Err(FsError::InvalidInput);
    }
    with_mount(path.node.mount_id, |mount| {
        mount.set_len(path.node.ino, len as u64)
    })
    .ok_or(FsError::Io)?
}

pub(crate) fn lookup_dir_in(context: PathContext, name: &str) -> FsResult<WorkingDir> {
    vfs_path::resolve_existing_in(context, name, LookupMode::FollowFinal)?
        .working_dir()
        .ok_or(FsError::NotDir)
}

impl File for VfsFile {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

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
        let noatime_snapshot = self.noatime_snapshot();
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
        drop(offset);
        if total_read_size > 0 {
            self.restore_noatime(noatime_snapshot);
        }
        total_read_size
    }

    fn write(&self, buf: UserBuffer) -> usize {
        self.write_inner(buf, false)
    }

    fn write_append(&self, buf: UserBuffer) -> usize {
        self.write_inner(buf, true)
    }

    fn stat(&self) -> FsResult<FileStat> {
        let mut stat = with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino))
            .ok_or(FsError::Io)??;
        stat.dev = self.node.mount_id.0 as u64;
        Ok(stat)
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        let noatime_snapshot = self.noatime_snapshot();
        let read_size = with_mount(self.node.mount_id, |mount| {
            mount.read_at(self.node.ino, buf, offset as u64)
        })
        .expect("filesystem mount is missing");
        if read_size > 0 {
            self.restore_noatime(noatime_snapshot);
        }
        read_size
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        self.write_at_chunks(offset, buf)
    }

    fn set_len(&self, len: usize) -> FsResult {
        if self.kind != FsNodeKind::RegularFile {
            return Err(FsError::InvalidInput);
        }
        if !self.writable {
            return Err(FsError::PermissionDenied);
        }
        with_mount(self.node.mount_id, |mount| {
            mount.set_len(self.node.ino, len as u64)
        })
        .ok_or(FsError::Io)?
    }

    fn sync(&self, data_only: bool) -> FsResult {
        with_mount(self.node.mount_id, |mount| {
            mount.sync(self.node.ino, data_only)
        })
        .ok_or(FsError::Io)?
    }

    fn seek(&self, offset: i64, whence: SeekWhence) -> FsResult<usize> {
        let mut current = self.offset.lock();
        let base = match whence {
            SeekWhence::Set => 0i128,
            SeekWhence::Current => *current as i128,
            SeekWhence::End => {
                let stat = with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino))
                    .ok_or(FsError::Io)??;
                stat.size as i128
            }
        };
        let new_offset = base
            .checked_add(offset as i128)
            .ok_or(FsError::InvalidInput)?;
        if new_offset < 0 || new_offset > usize::MAX as i128 || new_offset > isize::MAX as i128 {
            return Err(FsError::InvalidInput);
        }
        *current = new_offset as usize;
        Ok(*current)
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

    fn readlink(&self, buf: &mut [u8]) -> FsResult<usize> {
        if self.kind != FsNodeKind::Symlink {
            return Err(FsError::InvalidInput);
        }
        with_mount(self.node.mount_id, |mount| {
            mount.readlink(self.node.ino, buf)
        })
        .ok_or(FsError::Io)?
    }

    fn set_times(
        &self,
        atime: Option<FileTimestamp>,
        mtime: Option<FileTimestamp>,
        ctime: FileTimestamp,
    ) -> FsResult {
        with_mount(self.node.mount_id, |mount| {
            mount.set_times(self.node.ino, atime, mtime, ctime)
        })
        .ok_or(FsError::Io)?
    }

    fn set_mode(&self, mode: u32) -> FsResult {
        with_mount(self.node.mount_id, |mount| {
            mount.set_mode(self.node.ino, mode)
        })
        .ok_or(FsError::Io)?
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> FsResult {
        with_mount(self.node.mount_id, |mount| {
            mount.set_owner(self.node.ino, uid, gid)
        })
        .ok_or(FsError::Io)?
    }

    fn working_dir(&self) -> Option<WorkingDir> {
        if self.kind != FsNodeKind::Directory {
            return None;
        }
        Some(WorkingDir::new(self.node.mount_id, self.node.ino))
    }

    fn vfs_mount_id(&self) -> Option<super::super::mount::MountId> {
        Some(self.node.mount_id)
    }

    fn page_cache_id(&self) -> Option<PageCacheId> {
        if self.kind != FsNodeKind::RegularFile || !mount_supports_page_cache(self.node.mount_id) {
            return None;
        }
        Some(PageCacheId::new(self.node.mount_id, self.node.ino))
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }
}

impl Drop for VfsFile {
    fn drop(&mut self) {
        release_inode_from_drop(self.node.mount_id, self.node.ino);
    }
}
