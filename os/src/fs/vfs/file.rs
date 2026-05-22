use super::super::dentry_cache;
use super::super::devfs;
use super::super::dirent::{DT_DIR, RawDirEntry, write_dir_entries_with_offset_base};
use super::super::inode::{OpenFlags, link_node_in};
use super::super::mount::{
    MountId, MountNamespaceId, mount_is_devfs, mount_is_read_only, mount_supports_page_cache,
    release_inode_from_drop, synthetic_children_for_dir, with_mount,
};
use super::super::named_fifo::open_named_fifo;
use super::super::path::{PathContext, WorkingDir};
use super::super::status_flags::StatusFlagsCell;
use super::super::{
    FS_APPEND_FL, FS_IMMUTABLE_FL, File, FileStat, FileTimestamp, S_IFBLK, S_IFCHR, S_IFDIR,
    S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK, SeekWhence,
};
use super::path::{self as vfs_path, LookupMode, VfsOpenTarget};
use super::{FsError, FsNodeKind, FsResult, VfsNodeId, VfsPath};
use crate::config::PAGE_SIZE;
use crate::mm::{
    UserBuffer, frame_alloc,
    page_cache::{PAGE_CACHE, PageCacheId, PageCacheKey},
};
use crate::perf;
use crate::sync::SleepMutex;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;

// Bound each backend write while a shared file offset lock is held; large user
// buffers still progress in order without monopolizing one mount backend.
const VFS_WRITE_CHUNK_SIZE: usize = 64 * 1024;
const VFS_READ_CACHE_MAX_FILE_SIZE: usize = 1024 * 1024;
const VFS_READ_CACHE_MAX_PAGES: usize = 4096;
const MODE_PERMISSIONS_MASK: u32 = 0o7777;
const MODE_SETGID: u32 = 0o2000;
const TMPFILE_CREATE_ATTEMPTS: usize = 64;
const SEEK_SCAN_MIN_BLOCK_SIZE: usize = 1;
// Synthetic mountpoint entries live in a high offset range so they cannot
// collide with real backend dirent offsets returned by the filesystem.
const SYNTHETIC_DIRENT_OFFSET_BASE: u64 = 1 << 60;

static TMPFILE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

lazy_static! {
    static ref WRITABLE_REGULAR_OPEN_COUNTS: SleepMutex<BTreeMap<VfsNodeId, usize>> =
        SleepMutex::new(BTreeMap::new());
    static ref EXECUTABLE_REGULAR_COUNTS: SleepMutex<BTreeMap<VfsNodeId, usize>> =
        SleepMutex::new(BTreeMap::new());
}

fn track_writable_regular_open(node: VfsNodeId, kind: FsNodeKind, writable: bool) {
    if kind != FsNodeKind::RegularFile || !writable {
        return;
    }
    let mut counts = WRITABLE_REGULAR_OPEN_COUNTS.lock();
    *counts.entry(node).or_insert(0) += 1;
}

fn untrack_writable_regular_open(node: VfsNodeId, kind: FsNodeKind, writable: bool) {
    if kind != FsNodeKind::RegularFile || !writable {
        return;
    }
    let mut counts = WRITABLE_REGULAR_OPEN_COUNTS.lock();
    let Some(count) = counts.get_mut(&node) else {
        return;
    };
    if *count > 1 {
        *count -= 1;
    } else {
        counts.remove(&node);
    }
}

fn ensure_mount_writable(mount_id: MountId) -> FsResult {
    if mount_is_read_only(mount_id) {
        Err(FsError::ReadOnly)
    } else {
        Ok(())
    }
}

fn page_cache_id_for_node(node: VfsNodeId, kind: FsNodeKind) -> Option<PageCacheId> {
    if kind != FsNodeKind::RegularFile || !mount_supports_page_cache(node.mount_id) {
        return None;
    }
    Some(PageCacheId::new(node.mount_id, node.ino))
}

pub(crate) fn invalidate_regular_file_read_cache(node: VfsNodeId, kind: FsNodeKind) {
    let Some(id) = page_cache_id_for_node(node, kind) else {
        return;
    };
    let removed = PAGE_CACHE
        .exclusive_access()
        .invalidate_clean_unreferenced(id);
    if removed > 0 {
        perf::record_vfs_read_cache_invalidation(removed);
    }
}

pub(crate) fn regular_file_is_open_writable_in(context: PathContext, name: &str) -> FsResult<bool> {
    let path = vfs_path::resolve_existing_in(context, name, LookupMode::FollowFinal)?;
    if path.kind != FsNodeKind::RegularFile {
        return Ok(false);
    }
    Ok(regular_file_node_is_open_writable(path.node))
}

pub(crate) fn regular_file_node_is_open_writable(node: VfsNodeId) -> bool {
    WRITABLE_REGULAR_OPEN_COUNTS
        .lock()
        .get(&node)
        .copied()
        .unwrap_or(0)
        > 0
}

pub(crate) fn track_regular_file_executable(node: VfsNodeId) {
    let mut counts = EXECUTABLE_REGULAR_COUNTS.lock();
    *counts.entry(node).or_insert(0) += 1;
}

pub(crate) fn untrack_regular_file_executable(node: VfsNodeId) {
    let mut counts = EXECUTABLE_REGULAR_COUNTS.lock();
    let Some(count) = counts.get_mut(&node) else {
        return;
    };
    if *count > 1 {
        *count -= 1;
    } else {
        counts.remove(&node);
    }
}

pub(crate) fn regular_file_node_is_executable(node: VfsNodeId) -> bool {
    EXECUTABLE_REGULAR_COUNTS
        .lock()
        .get(&node)
        .copied()
        .unwrap_or(0)
        > 0
}

#[derive(Clone, Debug)]
pub(crate) struct FileCreateAttrs {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) euid: u32,
    pub(crate) egid: u32,
    pub(crate) fsgid: u32,
    pub(crate) mode: u32,
    pub(crate) umask: u32,
    pub(crate) groups: Vec<u32>,
}

impl FileCreateAttrs {
    fn can_keep_setgid(&self, gid: u32) -> bool {
        self.euid == 0 || self.egid == gid || self.fsgid == gid || self.groups.contains(&gid)
    }
}

fn prepare_created_file_mode(parent_stat: FileStat, attrs: &FileCreateAttrs) -> u32 {
    let mut mode = attrs.mode;
    if parent_stat.mode & MODE_SETGID != 0
        && mode & MODE_SETGID != 0
        && !attrs.can_keep_setgid(parent_stat.gid)
    {
        mode &= !MODE_SETGID;
    }
    (mode & !attrs.umask) & MODE_PERMISSIONS_MASK
}

pub(crate) struct VfsFile {
    node: VfsNodeId,
    parent: Option<VfsNodeId>,
    kind: FsNodeKind,
    namespace_id: MountNamespaceId,
    visible_path: Option<String>,
    offset: SleepMutex<usize>,
    readable: bool,
    writable: bool,
    status_flags: StatusFlagsCell,
    suppress_fanotify: bool,
}

impl VfsFile {
    fn new(
        path: VfsPath,
        parent: Option<VfsNodeId>,
        readable: bool,
        writable: bool,
        status_flags: OpenFlags,
        namespace_id: MountNamespaceId,
        suppress_fanotify: bool,
    ) -> FsResult<Self> {
        let node = path.node;
        let kind = path.kind;
        let visible_path = path.visible_path;
        with_mount(node.mount_id, |mount| mount.retain_inode(node.ino)).ok_or(FsError::Io)??;
        track_writable_regular_open(node, kind, writable);
        let file = Self {
            node,
            parent,
            kind,
            namespace_id,
            visible_path,
            offset: SleepMutex::new(0),
            readable,
            writable,
            status_flags: StatusFlagsCell::new(status_flags),
            suppress_fanotify,
        };
        if writable {
            invalidate_regular_file_read_cache(node, kind);
        }
        Ok(file)
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
        invalidate_regular_file_read_cache(self.node, self.kind);
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

    fn seek_data_or_hole(&self, offset: usize, seek_hole: bool) -> FsResult<usize> {
        if self.kind != FsNodeKind::RegularFile {
            return Err(FsError::IllegalSeek);
        }
        // UNFINISHED: This generic fallback infers sparse data/hole ranges
        // from nonzero bytes in filesystem-sized blocks instead of querying
        // backend extent allocation, so allocated zero-filled blocks may be
        // reported as holes.
        let stat = with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino))
            .ok_or(FsError::Io)??;
        let size = stat.size as usize;
        if offset > size {
            return Err(FsError::NoDeviceOrAddress);
        }
        if offset == size {
            return if seek_hole {
                Ok(size)
            } else {
                Err(FsError::NoDeviceOrAddress)
            };
        }

        let block_size = (stat.blksize as usize).max(SEEK_SCAN_MIN_BLOCK_SIZE);
        let mut buf = vec![0u8; block_size];
        let mut block_start = offset / block_size * block_size;
        let mut result = offset;

        while block_start < size {
            let block_end = block_start.saturating_add(block_size).min(size);
            let valid_len = block_end - block_start;
            buf[..valid_len].fill(0);
            let read_len = with_mount(self.node.mount_id, |mount| {
                mount.read_at(self.node.ino, &mut buf[..valid_len], block_start as u64)
            })
            .ok_or(FsError::Io)?;
            if read_len < valid_len {
                buf[read_len..valid_len].fill(0);
            }
            let is_data = buf[..valid_len].iter().any(|byte| *byte != 0);
            if seek_hole != is_data {
                return Ok(result.min(size));
            }

            block_start = block_start.saturating_add(block_size);
            result = block_start;
        }

        if seek_hole {
            Ok(size)
        } else {
            Err(FsError::NoDeviceOrAddress)
        }
    }

    fn inode_flags_or_empty(&self) -> FsResult<u32> {
        match self.inode_flags() {
            Ok(flags) => Ok(flags),
            // CONTEXT: procfs and other synthetic filesystems do not expose
            // ext-style inode flags. Treat them as having no immutable/append
            // bits so writable sysctl-style files can be updated normally.
            Err(FsError::Unsupported) => Ok(0),
            Err(err) => Err(err),
        }
    }

    fn read_synthetic_dirent64(&self, entry_offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        let Some(parent_path) = self.visible_path.as_deref() else {
            return Ok((0, entry_offset));
        };
        let entries: Vec<RawDirEntry> =
            synthetic_children_for_dir(self.namespace_id, self.node, parent_path)
                .into_iter()
                .filter(|entry| {
                    !with_mount(self.node.mount_id, |mount| {
                        mount
                            .lookup_component_from(self.node.ino, entry.name.as_str())
                            .is_ok()
                    })
                    .unwrap_or(false)
                })
                .map(|entry| RawDirEntry {
                    ino: entry.ino,
                    name: entry.name,
                    dtype: DT_DIR,
                })
                .collect();
        let (read_size, next_entry_offset) = write_dir_entries_with_offset_base(
            entries.as_slice(),
            entry_offset,
            SYNTHETIC_DIRENT_OFFSET_BASE,
            buf,
        )?;
        Ok((read_size, SYNTHETIC_DIRENT_OFFSET_BASE + next_entry_offset))
    }

    fn read_cache_id_for_size(&self, file_size: usize) -> Option<PageCacheId> {
        if self.writable
            || file_size > VFS_READ_CACHE_MAX_FILE_SIZE
            || regular_file_node_is_open_writable(self.node)
        {
            return None;
        }
        page_cache_id_for_node(self.node, self.kind)
    }

    fn read_regular_cached_at(&self, offset: usize, buf: &mut [u8]) -> Option<usize> {
        if buf.is_empty() {
            return Some(0);
        }
        let stat = with_mount(self.node.mount_id, |mount| mount.stat(self.node.ino))?.ok()?;
        let file_size = stat.size as usize;
        let id = self.read_cache_id_for_size(file_size)?;
        let mut total_read_size = 0usize;

        while total_read_size < buf.len() {
            let file_offset = offset.checked_add(total_read_size)?;
            if file_offset >= file_size {
                break;
            }
            let page_start = file_offset / PAGE_SIZE * PAGE_SIZE;
            let page_offset = file_offset - page_start;
            let valid_len = PAGE_SIZE.min(file_size - page_start);
            if page_offset >= valid_len {
                break;
            }
            let copy_len = (buf.len() - total_read_size).min(valid_len - page_offset);
            let key = PageCacheKey {
                id,
                page_index: page_start / PAGE_SIZE,
            };

            if let Some(read_size) = PAGE_CACHE.exclusive_access().copy_read_cache_page_data(
                key,
                page_offset,
                copy_len,
                &mut buf[total_read_size..total_read_size + copy_len],
            ) {
                total_read_size += read_size;
                perf::record_vfs_read_cache_hit(read_size);
                continue;
            }

            perf::record_vfs_read_cache_miss();
            if PAGE_CACHE.exclusive_access().len() >= VFS_READ_CACHE_MAX_PAGES {
                let read_size = with_mount(self.node.mount_id, |mount| {
                    mount.read_at(
                        self.node.ino,
                        &mut buf[total_read_size..],
                        file_offset as u64,
                    )
                })
                .expect("filesystem mount is missing");
                perf::record_vfs_read_cache_backend_read();
                total_read_size += read_size;
                break;
            }
            let Some(frame) = frame_alloc() else {
                let read_size = with_mount(self.node.mount_id, |mount| {
                    mount.read_at(
                        self.node.ino,
                        &mut buf[total_read_size..],
                        file_offset as u64,
                    )
                })
                .expect("filesystem mount is missing");
                perf::record_vfs_read_cache_backend_read();
                total_read_size += read_size;
                break;
            };

            let read_len = {
                let dst = &mut frame.ppn.get_bytes_array()[..valid_len];
                with_mount(self.node.mount_id, |mount| {
                    mount.read_at(self.node.ino, dst, page_start as u64)
                })
                .expect("filesystem mount is missing")
            };
            perf::record_vfs_read_cache_backend_read();
            if read_len == 0 || page_offset >= read_len {
                break;
            }
            let copy_len = copy_len.min(read_len - page_offset);
            buf[total_read_size..total_read_size + copy_len]
                .copy_from_slice(&frame.ppn.get_bytes_array()[page_offset..page_offset + copy_len]);
            if read_len == valid_len {
                PAGE_CACHE
                    .exclusive_access()
                    .insert_read_cache_page(key, frame, file_size);
            }
            total_read_size += copy_len;
            if read_len < valid_len {
                break;
            }
        }

        Some(total_read_size)
    }
}

fn parent_hint_for_open(context: &PathContext, name: &str) -> Option<VfsNodeId> {
    vfs_path::resolve_create_parent_in(context.clone(), name)
        .ok()
        .map(|target| target.parent)
}

fn open_vfs_file_impl(
    context: PathContext,
    name: &str,
    flags: OpenFlags,
    create_attrs: Option<FileCreateAttrs>,
) -> FsResult<Arc<VfsFile>> {
    let namespace_id = context.namespace_id();
    let parent_hint = parent_hint_for_open(&context, name);
    let follow_final_symlink = !flags.contains(OpenFlags::NOFOLLOW);
    let resolved = vfs_path::resolve_open_in(
        context,
        name,
        follow_final_symlink,
        flags.contains(OpenFlags::CREATE),
    )?;

    let (path, parent, readable, writable) = match resolved {
        VfsOpenTarget::Existing(path) => {
            if flags.contains(OpenFlags::CREATE | OpenFlags::EXCL) {
                return Err(FsError::AlreadyExists);
            }
            if path.kind == FsNodeKind::Directory {
                if !flags.can_open_directory() {
                    return Err(FsError::IsDir);
                }
                (path, parent_hint, false, false)
            } else {
                if flags.contains(OpenFlags::DIRECTORY) {
                    return Err(FsError::NotDir);
                }
                // CONTEXT: readlinkat("", fd) needs an O_PATH|O_NOFOLLOW fd
                // that refers to the symlink itself; full O_PATH semantics are
                // intentionally deferred.
                if path.kind == FsNodeKind::Symlink
                    && flags.contains(OpenFlags::NOFOLLOW)
                    && !flags.contains(OpenFlags::PATH)
                {
                    return Err(FsError::Loop);
                }
                let (readable, writable) = flags.read_write();
                if path.kind == FsNodeKind::RegularFile
                    && writable
                    && regular_file_node_is_executable(path.node)
                {
                    return Err(FsError::TextBusy);
                }
                if flags.contains(OpenFlags::TRUNC) && flags.writable_target() {
                    ensure_mount_writable(path.node.mount_id)?;
                    invalidate_regular_file_read_cache(path.node, path.kind);
                    with_mount(path.node.mount_id, |mount| mount.set_len(path.node.ino, 0))
                        .ok_or(FsError::Io)??;
                }
                (path, parent_hint, readable, writable)
            }
        }
        VfsOpenTarget::Create(target) => {
            if flags.contains(OpenFlags::DIRECTORY) {
                return Err(FsError::InvalidInput);
            }
            ensure_mount_writable(target.parent.mount_id)?;
            let parent_stat = with_mount(target.parent.mount_id, |mount| {
                mount.stat(target.parent.ino)
            })
            .ok_or(FsError::Io)??;
            let ino = with_mount(target.parent.mount_id, |mount| {
                mount.create_file(target.parent.ino, target.leaf_name)
            })
            .ok_or(FsError::Io)??;
            dentry_cache::invalidate_parent(target.parent);
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
                let mode = prepare_created_file_mode(parent_stat, &attrs);
                with_mount(target.parent.mount_id, |mount| mount.set_mode(ino, mode))
                    .ok_or(FsError::Io)??;
            }
            let (readable, writable) = flags.read_write();
            (
                VfsPath::with_visible_path(
                    VfsNodeId::new(target.parent.mount_id, ino),
                    FsNodeKind::RegularFile,
                    target.leaf_path,
                ),
                Some(target.parent),
                readable,
                writable,
            )
        }
    };

    Ok(Arc::new(VfsFile::new(
        path,
        parent,
        readable,
        writable,
        OpenFlags::file_status_flags(flags),
        namespace_id,
        false,
    )?))
}

fn create_tmpfile_inode(
    namespace_id: MountNamespaceId,
    directory: VfsPath,
    flags: OpenFlags,
    create_attrs: Option<FileCreateAttrs>,
) -> FsResult<Arc<VfsFile>> {
    if directory.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    let (_, writable) = flags.read_write();
    if !writable {
        return Err(FsError::InvalidInput);
    }
    ensure_mount_writable(directory.node.mount_id)?;

    let parent_stat = with_mount(directory.node.mount_id, |mount| {
        mount.stat(directory.node.ino)
    })
    .ok_or(FsError::Io)??;
    let (ino, leaf_name) = {
        let mut created = None;
        for _ in 0..TMPFILE_CREATE_ATTEMPTS {
            let seq = TMPFILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let leaf_name = format!(".whusp-tmpfile-{seq:x}");
            let result = with_mount(directory.node.mount_id, |mount| {
                mount.create_file(directory.node.ino, leaf_name.as_str())
            })
            .ok_or(FsError::Io)?;
            match result {
                Ok(ino) => {
                    dentry_cache::invalidate_parent(directory.node);
                    created = Some((ino, leaf_name));
                    break;
                }
                Err(FsError::AlreadyExists) => continue,
                Err(err) => return Err(err),
            }
        }
        created.ok_or(FsError::AlreadyExists)?
    };

    if let Some(attrs) = create_attrs {
        let gid = if parent_stat.mode & MODE_SETGID != 0 {
            parent_stat.gid
        } else {
            attrs.gid
        };
        with_mount(directory.node.mount_id, |mount| {
            mount.set_owner(ino, Some(attrs.uid), Some(gid))
        })
        .ok_or(FsError::Io)??;
        let mode = prepare_created_file_mode(parent_stat, &attrs);
        with_mount(directory.node.mount_id, |mount| mount.set_mode(ino, mode))
            .ok_or(FsError::Io)??;
    }

    let (readable, writable) = flags.read_write();
    let file = Arc::new(VfsFile::new(
        VfsPath::new(
            VfsNodeId::new(directory.node.mount_id, ino),
            FsNodeKind::RegularFile,
        ),
        Some(directory.node),
        readable,
        writable,
        OpenFlags::file_status_flags(flags),
        namespace_id,
        false,
    )?);

    match with_mount(directory.node.mount_id, |mount| {
        mount.unlink(directory.node.ino, leaf_name.as_str())
    })
    .ok_or(FsError::Io)?
    {
        Ok(()) => {
            dentry_cache::invalidate_parent(directory.node);
            Ok(file)
        }
        Err(err) => {
            drop(file);
            Err(err)
        }
    }
}

pub(crate) fn open_tmpfile_in_with_attrs(
    context: PathContext,
    name: &str,
    flags: OpenFlags,
    create_attrs: Option<FileCreateAttrs>,
) -> FsResult<Arc<dyn File + Send + Sync>> {
    let namespace_id = context.namespace_id();
    let directory = vfs_path::resolve_existing_in(context, name, LookupMode::FollowFinal)?;
    create_tmpfile_inode(namespace_id, directory, flags, create_attrs)
        .map(|file| file as Arc<dyn File + Send + Sync>)
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
    let follow_final_symlink = !flags.contains(OpenFlags::NOFOLLOW);
    let lookup_mode = if follow_final_symlink {
        LookupMode::FollowFinal
    } else {
        LookupMode::NoFollowFinal
    };
    if let Ok(path) = vfs_path::resolve_existing_in(context.clone(), name, lookup_mode) {
        if mount_is_devfs(path.node.mount_id) {
            if path.kind == FsNodeKind::Directory {
                return open_vfs_file_impl(context, name, flags, create_attrs)
                    .map(|file| file as Arc<dyn File + Send + Sync>);
            }
            return devfs::open_inode(path.node.mount_id, path.node.ino, flags);
        }
        if path.kind == FsNodeKind::Fifo {
            if flags.contains(OpenFlags::CREATE | OpenFlags::EXCL) {
                return Err(FsError::AlreadyExists);
            }
            if flags.contains(OpenFlags::DIRECTORY) {
                return Err(FsError::NotDir);
            }
            return open_named_fifo(path.node, OpenFlags::file_status_flags(flags));
        }
    }
    open_vfs_file_impl(context, name, flags, create_attrs)
        .map(|file| file as Arc<dyn File + Send + Sync>)
}

fn node_kind_from_mode(mode: u32) -> FsNodeKind {
    match mode & S_IFMT {
        S_IFDIR => FsNodeKind::Directory,
        S_IFREG => FsNodeKind::RegularFile,
        S_IFLNK => FsNodeKind::Symlink,
        S_IFIFO => FsNodeKind::Fifo,
        S_IFCHR => FsNodeKind::CharacterDevice,
        S_IFBLK => FsNodeKind::BlockDevice,
        S_IFSOCK => FsNodeKind::Socket,
        _ => FsNodeKind::Other,
    }
}

pub(crate) fn open_file_handle_node(
    node: VfsNodeId,
    flags: OpenFlags,
    namespace_id: MountNamespaceId,
) -> FsResult<Arc<dyn File + Send + Sync>> {
    if mount_is_devfs(node.mount_id) {
        return devfs::open_inode(node.mount_id, node.ino, flags);
    }

    let stat =
        with_mount(node.mount_id, |mount| mount.stat(node.ino)).ok_or(FsError::NotFound)??;
    let kind = node_kind_from_mode(stat.mode);
    if flags.contains(OpenFlags::DIRECTORY) && kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    if kind == FsNodeKind::Directory && !flags.can_open_directory() {
        return Err(FsError::IsDir);
    }
    if kind == FsNodeKind::Symlink && !flags.contains(OpenFlags::PATH) {
        return Err(FsError::Loop);
    }
    if kind == FsNodeKind::Fifo {
        return open_named_fifo(node, OpenFlags::file_status_flags(flags));
    }

    let (readable, writable) = flags.read_write();
    if kind == FsNodeKind::RegularFile && writable && regular_file_node_is_executable(node) {
        return Err(FsError::TextBusy);
    }
    if kind == FsNodeKind::RegularFile && flags.contains(OpenFlags::TRUNC) && writable {
        ensure_mount_writable(node.mount_id)?;
        invalidate_regular_file_read_cache(node, kind);
        with_mount(node.mount_id, |mount| mount.set_len(node.ino, 0)).ok_or(FsError::Io)??;
    }

    Ok(Arc::new(VfsFile::new(
        VfsPath::new(node, kind),
        None,
        readable,
        writable,
        OpenFlags::file_status_flags(flags),
        namespace_id,
        false,
    )?))
}

pub(crate) fn link_open_file_in(
    file: Arc<dyn File + Send + Sync>,
    new_context: PathContext,
    new_name: &str,
) -> FsResult {
    let Some(file) = file.as_any().downcast_ref::<VfsFile>() else {
        return Err(FsError::CrossDevice);
    };
    link_node_in(file.node, file.kind, new_context, new_name)
}

pub(crate) fn stat_in(
    context: PathContext,
    name: &str,
    follow_final_symlink: bool,
) -> FsResult<FileStat> {
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

pub(crate) fn lookup_path_in(
    context: PathContext,
    name: &str,
    follow_final_symlink: bool,
) -> FsResult<VfsPath> {
    let mode = if follow_final_symlink {
        LookupMode::FollowFinal
    } else {
        LookupMode::NoFollowFinal
    };
    vfs_path::resolve_existing_in(context, name, mode)
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

pub(crate) fn lookup_dir_with_stat_path_in(
    context: PathContext,
    name: &str,
) -> FsResult<(WorkingDir, FileStat, String)> {
    let path = vfs_path::resolve_existing_in(context, name, LookupMode::FollowFinal)?;
    if path.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    let mut stat =
        with_mount(path.node.mount_id, |mount| mount.stat(path.node.ino)).ok_or(FsError::Io)??;
    stat.dev = path.node.mount_id.0 as u64;
    let visible_path = path.visible_path.ok_or(FsError::NotFound)?;
    Ok((
        WorkingDir::new(path.node.mount_id, path.node.ino),
        stat,
        visible_path,
    ))
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
    ensure_mount_writable(path.node.mount_id)?;
    invalidate_regular_file_read_cache(path.node, path.kind);
    with_mount(path.node.mount_id, |mount| {
        mount.set_len(path.node.ino, len as u64)
    })
    .ok_or(FsError::Io)?
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
        let requested_len = buf.len();
        let noatime_snapshot = self.noatime_snapshot();
        let mut offset = self.offset.lock();
        let mut total_read_size = 0usize;
        for slice in buf.buffers.iter_mut() {
            let read_size = self
                .read_regular_cached_at(*offset, slice)
                .unwrap_or_else(|| {
                    with_mount(self.node.mount_id, |mount| {
                        mount.read_at(self.node.ino, slice, *offset as u64)
                    })
                    .expect("filesystem mount is missing")
                });
            if read_size == 0 {
                break;
            }
            *offset += read_size;
            total_read_size += read_size;
        }
        drop(offset);
        if requested_len > 0 {
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
        let read_size = self.read_regular_cached_at(offset, buf).unwrap_or_else(|| {
            with_mount(self.node.mount_id, |mount| {
                mount.read_at(self.node.ino, buf, offset as u64)
            })
            .expect("filesystem mount is missing")
        });
        if !buf.is_empty() {
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
        self.check_set_len(len)?;
        invalidate_regular_file_read_cache(self.node, self.kind);
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
            SeekWhence::Data | SeekWhence::Hole => {
                if offset < 0 {
                    return Err(FsError::InvalidInput);
                }
                let next = self.seek_data_or_hole(offset as usize, whence == SeekWhence::Hole)?;
                *current = next;
                return Ok(next);
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
        let current_offset = *offset as u64;
        let (read_size, next_offset) = if current_offset >= SYNTHETIC_DIRENT_OFFSET_BASE {
            self.read_synthetic_dirent64(
                current_offset - SYNTHETIC_DIRENT_OFFSET_BASE,
                &mut kernel_buf,
            )?
        } else {
            let (read_size, next_offset) = with_mount(self.node.mount_id, |mount| {
                mount.read_dirent64(self.node.ino, current_offset, &mut kernel_buf)
            })
            .ok_or(FsError::Io)??;
            if read_size == 0 {
                self.read_synthetic_dirent64(0, &mut kernel_buf)?
            } else {
                (read_size, next_offset)
            }
        };
        if read_size == 0 {
            return Ok(0);
        }
        let mut user_buf = user_buf;
        let copied = user_buf.copy_from_slice(&kernel_buf[..read_size]);
        debug_assert_eq!(copied, read_size);
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

    fn proc_fd_target(&self) -> Option<String> {
        self.visible_path.clone()
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

    fn inode_flags(&self) -> FsResult<u32> {
        with_mount(self.node.mount_id, |mount| mount.inode_flags(self.node.ino))
            .ok_or(FsError::Io)?
    }

    fn set_inode_flags(&self, flags: u32) -> FsResult {
        with_mount(self.node.mount_id, |mount| {
            mount.set_inode_flags(self.node.ino, flags)
        })
        .ok_or(FsError::Io)?
    }

    fn check_write(&self, _len: usize, append: bool) -> FsResult {
        ensure_mount_writable(self.node.mount_id)?;
        let flags = self.inode_flags_or_empty()?;
        if flags & FS_IMMUTABLE_FL != 0 {
            return Err(FsError::PermissionDenied);
        }
        if flags & FS_APPEND_FL != 0 && !append {
            return Err(FsError::PermissionDenied);
        }
        Ok(())
    }

    fn check_write_at(&self, _offset: usize, _len: usize) -> FsResult {
        ensure_mount_writable(self.node.mount_id)?;
        let flags = self.inode_flags_or_empty()?;
        if flags & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0 {
            return Err(FsError::PermissionDenied);
        }
        Ok(())
    }

    fn check_set_len(&self, _len: usize) -> FsResult {
        ensure_mount_writable(self.node.mount_id)?;
        let flags = self.inode_flags_or_empty()?;
        if flags & (FS_IMMUTABLE_FL | FS_APPEND_FL) != 0 {
            return Err(FsError::PermissionDenied);
        }
        Ok(())
    }

    fn working_dir(&self) -> Option<WorkingDir> {
        if self.kind != FsNodeKind::Directory {
            return None;
        }
        Some(WorkingDir::new(self.node.mount_id, self.node.ino))
    }

    fn vfs_node_id(&self) -> Option<VfsNodeId> {
        Some(self.node)
    }

    fn vfs_parent_node_id(&self) -> Option<VfsNodeId> {
        self.parent
    }

    fn vfs_mount_id(&self) -> Option<super::super::mount::MountId> {
        Some(self.node.mount_id)
    }

    fn is_devfs_dir(&self) -> bool {
        self.kind == FsNodeKind::Directory && mount_is_devfs(self.node.mount_id)
    }

    fn is_devfs_misc_dir(&self) -> bool {
        mount_is_devfs(self.node.mount_id) && devfs::inode_is_misc_dir(self.node.ino)
    }

    fn is_devfs_pts_dir(&self) -> bool {
        mount_is_devfs(self.node.mount_id) && devfs::inode_is_pts_dir(self.node.ino)
    }

    fn page_cache_id(&self) -> Option<PageCacheId> {
        page_cache_id_for_node(self.node, self.kind)
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }

    fn clone_for_fanotify_event(&self, flags: OpenFlags) -> FsResult<Arc<dyn File + Send + Sync>> {
        let (readable, writable) = flags.read_write();
        Ok(Arc::new(VfsFile::new(
            VfsPath {
                node: self.node,
                kind: self.kind,
                visible_path: self.visible_path.clone(),
            },
            self.parent,
            readable,
            writable,
            OpenFlags::file_status_flags(flags),
            self.namespace_id,
            true,
        )?))
    }

    fn suppresses_fanotify(&self) -> bool {
        self.suppress_fanotify
    }
}

impl Drop for VfsFile {
    fn drop(&mut self) {
        untrack_writable_regular_open(self.node, self.kind, self.writable);
        release_inode_from_drop(self.node.mount_id, self.node.ino);
    }
}
