use super::File;
use super::ext4::FsNodeKind;
use super::mount::{MountId, with_mount};
use super::path::{ResolvedOpen, resolve_open_target};
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::*;

pub struct OSInode {
    readable: bool,
    writable: bool,
    inner: UPIntrFreeCell<OSInodeInner>,
}

pub struct OSInodeInner {
    offset: usize,
    mount_id: MountId,
    ino: u32,
}

impl OSInode {
    fn new(readable: bool, writable: bool, mount_id: MountId, ino: u32) -> Self {
        Self {
            readable,
            writable,
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

bitflags! {
    pub struct OpenFlags: u32 {
        const RDONLY = 0;
        const WRONLY = 1 << 0;
        const RDWR = 1 << 1;
        const CREATE = 1 << 9;
        const TRUNC = 1 << 10;
    }
}

impl OpenFlags {
    pub fn read_write(&self) -> (bool, bool) {
        if self.is_empty() {
            (true, false)
        } else if self.contains(Self::WRONLY) {
            (false, true)
        } else {
            (true, true)
        }
    }
}

pub fn open_file(name: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    let (readable, writable) = flags.read_write();
    let resolved = resolve_open_target(
        name,
        writable || flags.contains(OpenFlags::TRUNC),
        flags.contains(OpenFlags::CREATE),
    )?;

    let (mount_id, ino) = match resolved {
        ResolvedOpen::Existing(file) => {
            if file.kind == FsNodeKind::Directory {
                return None;
            }
            if flags.contains(OpenFlags::TRUNC) || flags.contains(OpenFlags::CREATE) {
                with_mount(file.mount_id, |mount| mount.set_len(file.ino, 0))
                    .expect("filesystem mount is missing")?;
            }
            (file.mount_id, file.ino)
        }
        ResolvedOpen::Create(target) => {
            let ino = with_mount(target.mount_id, |mount| {
                mount.create_file(target.parent_ino, target.leaf_name)
            })
            .expect("filesystem mount is missing")?;
            (target.mount_id, ino)
        }
    };

    Some(Arc::new(OSInode::new(readable, writable, mount_id, ino)))
}

impl File for OSInode {
    fn readable(&self) -> bool {
        self.readable
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn read(&self, mut buf: UserBuffer) -> usize {
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
}
