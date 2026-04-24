use super::ext4::FsNodeKind;
use super::mount::{MountId, with_mount};
use super::path::{ResolvedOpen, WorkingDir, resolve_open_target, resolve_parent_target};
use super::{File, FileStat};
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bitflags::*;

pub struct OSInode {
    readable: bool,
    writable: bool,
    kind: FsNodeKind,
    inner: UPIntrFreeCell<OSInodeInner>,
}

pub struct OSInodeInner {
    offset: usize,
    mount_id: MountId,
    ino: u32,
}

impl OSInode {
    fn new(readable: bool, writable: bool, kind: FsNodeKind, mount_id: MountId, ino: u32) -> Self {
        Self {
            readable,
            writable,
            kind,
            inner: unsafe {
                UPIntrFreeCell::new(OSInodeInner {
                    offset: 0,
                    mount_id,
                    ino,
                })
            },
        }
    }

    pub fn read_all(&self) -> Vec<u8> {
        let mut inner = self.inner.exclusive_access();
        let mut buffer = [0u8; 512];
        let mut data = Vec::new();
        loop {
            let len = with_mount(inner.mount_id, |mount| {
                mount.read_at(inner.ino, &mut buffer, inner.offset as u64)
            })
            .expect("filesystem mount is missing");
            if len == 0 {
                break;
            }
            inner.offset += len;
            data.extend_from_slice(&buffer[..len]);
        }
        data
    }
}

// TODO: more flags to implemnent
bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct OpenFlags: u32 {
        const RDONLY = 0;
        const WRONLY = 1 << 0;
        const RDWR = 1 << 1;
        const CREATE = 0o100;
        const TRUNC = 0o1000;
        const DIRECTORY = 0o200000;
    }
}

impl OpenFlags {
    pub fn read_write(&self) -> (bool, bool) {
        match self.bits() & 0b11 {
            0 => (true, false),
            1 => (false, true),
            2 => (true, true),
            _ => (false, false),
        }
    }

    pub fn writable_target(&self) -> bool {
        matches!(self.bits() & 0b11, 1 | 2)
    }

    pub fn can_open_directory(&self) -> bool {
        !self.writable_target() && !self.contains(Self::CREATE) && !self.contains(Self::TRUNC)
    }
}

fn open_file_impl(cwd: Option<WorkingDir>, name: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    let resolved = resolve_open_target(
        cwd,
        name,
        flags.writable_target() || flags.contains(OpenFlags::TRUNC),
        flags.contains(OpenFlags::CREATE),
    )?;

    let (mount_id, ino, kind, readable, writable) = match resolved {
        ResolvedOpen::Existing(file) => {
            if file.kind == FsNodeKind::Directory {
                if !flags.can_open_directory() {
                    return None;
                }
                (file.mount_id, file.ino, file.kind, false, false)
            } else {
                let (readable, writable) = flags.read_write();
                if flags.contains(OpenFlags::TRUNC) {
                    with_mount(file.mount_id, |mount| mount.set_len(file.ino, 0))
                        .expect("filesystem mount is missing")?;
                }
                (file.mount_id, file.ino, file.kind, readable, writable)
            }
        }
        ResolvedOpen::Create(target) => {
            let ino = with_mount(target.mount_id, |mount| {
                mount.create_file(target.parent_ino, target.leaf_name)
            })
            .expect("filesystem mount is missing")?;
            let (readable, writable) = flags.read_write();
            (
                target.mount_id,
                ino,
                FsNodeKind::RegularFile,
                readable,
                writable,
            )
        }
    };

    Some(Arc::new(OSInode::new(
        readable, writable, kind, mount_id, ino,
    )))
}

pub fn open_file(name: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    open_file_impl(None, name, flags)
}

pub(crate) fn open_file_at(cwd: WorkingDir, name: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    open_file_impl(Some(cwd), name, flags)
}

pub(crate) fn lookup_dir_at(cwd: WorkingDir, name: &str) -> Option<WorkingDir> {
    match resolve_open_target(Some(cwd), name, false, false)? {
        ResolvedOpen::Existing(file) if file.kind == FsNodeKind::Directory => {
            Some(WorkingDir::new(file.mount_id, file.ino))
        }
        _ => None,
    }
}

pub(crate) fn mkdir_at(cwd: WorkingDir, name: &str, mode: u32) -> Option<()> {
    if matches!(
        resolve_open_target(Some(cwd), name, false, false),
        Some(ResolvedOpen::Existing(_))
    ) {
        return None;
    }
    let target = resolve_parent_target(Some(cwd), name)?;
    with_mount(target.mount_id, |mount| {
        mount.create_dir(target.parent_ino, target.leaf_name, mode)
    })
    .expect("filesystem mount is missing")?;
    Some(())
}

pub(crate) fn unlink_file_at(cwd: WorkingDir, name: &str) -> Option<()> {
    let resolved = resolve_open_target(Some(cwd), name, false, false)?;
    let ResolvedOpen::Existing(file) = resolved else {
        return None;
    };
    if file.kind == FsNodeKind::Directory {
        return None;
    }
    let target = resolve_parent_target(Some(cwd), name)?;
    with_mount(target.mount_id, |mount| {
        mount.unlink(target.parent_ino, target.leaf_name)
    })
    .expect("filesystem mount is missing")?;
    Some(())
}

impl File for OSInode {
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
        let mut inner = self.inner.exclusive_access();
        let mut total_read_size = 0usize;
        for slice in buf.buffers.iter_mut() {
            let read_size = with_mount(inner.mount_id, |mount| {
                mount.read_at(inner.ino, slice, inner.offset as u64)
            })
            .expect("filesystem mount is missing");
            if read_size == 0 {
                break;
            }
            inner.offset += read_size;
            total_read_size += read_size;
        }
        total_read_size
    }

    fn write(&self, buf: UserBuffer) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        let mut inner = self.inner.exclusive_access();
        let mut total_write_size = 0usize;
        for slice in buf.buffers.iter() {
            let write_size = with_mount(inner.mount_id, |mount| {
                mount.write_at(inner.ino, slice, inner.offset as u64)
            })
            .expect("filesystem mount is missing");
            inner.offset += write_size;
            total_write_size += write_size;
        }
        total_write_size
    }

    fn stat(&self) -> FileStat {
        let inner = self.inner.exclusive_access();
        let mut stat = with_mount(inner.mount_id, |mount| mount.stat(inner.ino))
            .expect("filesystem mount is missing")
            .expect("inode stat failed");
        stat.dev = inner.mount_id.0 as u64;
        stat
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        let inner = self.inner.exclusive_access();
        with_mount(inner.mount_id, |mount| {
            mount.read_at(inner.ino, buf, offset as u64)
        })
        .expect("filesystem mount is missing")
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        let inner = self.inner.exclusive_access();
        with_mount(inner.mount_id, |mount| {
            mount.write_at(inner.ino, buf, offset as u64)
        })
        .expect("filesystem mount is missing")
    }

    fn read_dirent64(&self, user_buf: UserBuffer) -> isize {
        if self.kind != FsNodeKind::Directory {
            return -1;
        }
        let mut inner = self.inner.exclusive_access();
        let mut kernel_buf = vec![0u8; user_buf.len()];
        let Some((read_size, next_offset)) = with_mount(inner.mount_id, |mount| {
            mount.read_dirent64(inner.ino, inner.offset as u64, &mut kernel_buf)
        })
        .expect("filesystem mount is missing") else {
            return -1;
        };
        if read_size == 0 {
            return 0;
        }
        // TODO: feel that there will be a performance loss since it is not necessary
        for (idx, byte_ref) in user_buf.into_iter().take(read_size).enumerate() {
            unsafe {
                *byte_ref = kernel_buf[idx];
            }
        }
        inner.offset = next_offset as usize;
        read_size as isize
    }

    fn working_dir(&self) -> Option<WorkingDir> {
        if self.kind != FsNodeKind::Directory {
            return None;
        }
        let inner = self.inner.exclusive_access();
        Some(WorkingDir::new(inner.mount_id, inner.ino))
    }
}
