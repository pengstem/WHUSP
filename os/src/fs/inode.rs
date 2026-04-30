use super::devfs;
use super::ext4::FsNodeKind;
use super::mount::{MountId, with_mount};
use super::path::{
    ResolvedOpen, WorkingDir, resolve_mount_target, resolve_open_target, resolve_parent_target,
};
use super::{File, FileStat};
use crate::mm::UserBuffer;
use crate::sync::SleepMutex;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bitflags::*;

pub struct OSInode {
    readable: bool,
    writable: bool,
    kind: FsNodeKind,
    mount_id: MountId,
    ino: u32,
    state: SleepMutex<OSInodeState>,
}

pub struct OSInodeState {
    offset: usize,
}

impl OSInode {
    fn new(readable: bool, writable: bool, kind: FsNodeKind, mount_id: MountId, ino: u32) -> Self {
        Self {
            readable,
            writable,
            kind,
            mount_id,
            ino,
            state: SleepMutex::new(OSInodeState { offset: 0 }),
        }
    }

    pub fn read_all(&self) -> Vec<u8> {
        let mut state = self.state.lock();
        let mut buffer = [0u8; 4096];
        let mut data = Vec::new();
        loop {
            let len = with_mount(self.mount_id, |mount| {
                mount.read_at(self.ino, &mut buffer, state.offset as u64)
            })
            .expect("filesystem mount is missing");
            if len == 0 {
                break;
            }
            state.offset += len;
            data.extend_from_slice(&buffer[..len]);
        }
        data
    }
}

// TODO: add remaining Linux open flags as syscall coverage needs them.
bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct OpenFlags: u32 {
        const RDONLY = 0;
        const WRONLY = 1 << 0;
        const RDWR = 1 << 1;
        const CREATE = 0o100;
        const NOCTTY = 0o400;
        const TRUNC = 0o1000;
        const APPEND = 0o2000;
        const NONBLOCK = 0o4000;
        const DIRECT = 0o40000;
        const LARGEFILE = 0o100000;
        const DIRECTORY = 0o200000;
        const CLOEXEC = 0o2000000;
    }
}

impl OpenFlags {
    const ACCESS_MODE_MASK: u32 = 0b11;
    const FCNTL_MUTABLE_STATUS_MASK: u32 =
        OpenFlags::APPEND.bits() | OpenFlags::NONBLOCK.bits() | OpenFlags::DIRECT.bits();

    pub fn read_write(&self) -> (bool, bool) {
        match self.bits() & Self::ACCESS_MODE_MASK {
            0 => (true, false),
            1 => (false, true),
            2 => (true, true),
            _ => (false, false),
        }
    }

    pub fn writable_target(&self) -> bool {
        matches!(self.bits() & Self::ACCESS_MODE_MASK, 1 | 2)
    }

    pub fn can_open_directory(&self) -> bool {
        !self.writable_target() && !self.contains(Self::CREATE) && !self.contains(Self::TRUNC)
    }

    pub fn file_status_flags(flags: Self) -> Self {
        Self::from_bits_truncate(
            flags.bits() & (Self::ACCESS_MODE_MASK | Self::FCNTL_MUTABLE_STATUS_MASK),
        )
    }

    pub fn with_fcntl_status_flags(self, flags: u32) -> Self {
        let preserved = self.bits() & !Self::FCNTL_MUTABLE_STATUS_MASK;
        let updated = flags & Self::FCNTL_MUTABLE_STATUS_MASK;
        Self::from_bits_truncate(preserved | updated)
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

pub(crate) fn open_file_at(
    cwd: WorkingDir,
    name: &str,
    flags: OpenFlags,
) -> Option<Arc<dyn File + Send + Sync>> {
    if let Some(file) = devfs::open(name, flags) {
        return Some(file);
    }
    open_file_impl(Some(cwd), name, flags).map(|inode| inode as Arc<dyn File + Send + Sync>)
}

pub(crate) fn stat_at(cwd: WorkingDir, name: &str) -> Option<FileStat> {
    if let Some(stat) = devfs::stat(name) {
        return Some(stat);
    }
    let ResolvedOpen::Existing(file) = resolve_open_target(Some(cwd), name, false, false)? else {
        return None;
    };
    let mut stat = with_mount(file.mount_id, |mount| mount.stat(file.ino))
        .expect("filesystem mount is missing")?;
    stat.dev = file.mount_id.0 as u64;
    Some(stat)
}

pub(crate) fn lookup_dir_at(cwd: WorkingDir, name: &str) -> Option<WorkingDir> {
    match resolve_open_target(Some(cwd), name, false, false)? {
        ResolvedOpen::Existing(file) if file.kind == FsNodeKind::Directory => {
            Some(WorkingDir::new(file.mount_id, file.ino))
        }
        _ => None,
    }
}

pub(crate) fn lookup_mount_target_dir_at(cwd: WorkingDir, name: &str) -> Option<WorkingDir> {
    let file = resolve_mount_target(Some(cwd), name)?;
    (file.kind == FsNodeKind::Directory).then_some(WorkingDir::new(file.mount_id, file.ino))
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
        let mut state = self.state.lock();
        let mut total_read_size = 0usize;
        for slice in buf.buffers.iter_mut() {
            let read_size = with_mount(self.mount_id, |mount| {
                mount.read_at(self.ino, slice, state.offset as u64)
            })
            .expect("filesystem mount is missing");
            if read_size == 0 {
                break;
            }
            state.offset += read_size;
            total_read_size += read_size;
        }
        total_read_size
    }

    fn write(&self, buf: UserBuffer) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        let mut state = self.state.lock();
        let mut total_write_size = 0usize;
        for slice in buf.buffers.iter() {
            let write_size = with_mount(self.mount_id, |mount| {
                mount.write_at(self.ino, slice, state.offset as u64)
            })
            .expect("filesystem mount is missing");
            state.offset += write_size;
            total_write_size += write_size;
        }
        total_write_size
    }

    fn write_append(&self, buf: UserBuffer) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        let mut state = self.state.lock();
        let Some(stat) = with_mount(self.mount_id, |mount| mount.stat(self.ino))
            .expect("filesystem mount is missing")
        else {
            return 0;
        };
        state.offset = stat.size as usize;
        let mut total_write_size = 0usize;
        for slice in buf.buffers.iter() {
            let write_size = with_mount(self.mount_id, |mount| {
                mount.write_at(self.ino, slice, state.offset as u64)
            })
            .expect("filesystem mount is missing");
            state.offset += write_size;
            total_write_size += write_size;
        }
        total_write_size
    }

    fn stat(&self) -> FileStat {
        let mut stat = with_mount(self.mount_id, |mount| mount.stat(self.ino))
            .expect("filesystem mount is missing")
            .expect("inode stat failed");
        stat.dev = self.mount_id.0 as u64;
        stat
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        with_mount(self.mount_id, |mount| {
            mount.read_at(self.ino, buf, offset as u64)
        })
        .expect("filesystem mount is missing")
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        if self.kind == FsNodeKind::Directory {
            return 0;
        }
        with_mount(self.mount_id, |mount| {
            mount.write_at(self.ino, buf, offset as u64)
        })
        .expect("filesystem mount is missing")
    }

    fn read_dirent64(&self, user_buf: UserBuffer) -> isize {
        if self.kind != FsNodeKind::Directory {
            return -1;
        }
        let mut state = self.state.lock();
        let mut kernel_buf = vec![0u8; user_buf.len()];
        let Some((read_size, next_offset)) = with_mount(self.mount_id, |mount| {
            mount.read_dirent64(self.ino, state.offset as u64, &mut kernel_buf)
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
        state.offset = next_offset as usize;
        read_size as isize
    }

    fn working_dir(&self) -> Option<WorkingDir> {
        if self.kind != FsNodeKind::Directory {
            return None;
        }
        Some(WorkingDir::new(self.mount_id, self.ino))
    }
}
