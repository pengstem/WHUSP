use super::ext4::FsNodeKind;
use super::mount::with_mount;
use super::path::WorkingDir;
use super::vfs::{resolve_create_parent, resolve_mount_target};
use bitflags::*;

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

pub(crate) fn lookup_mount_target_dir_at(cwd: WorkingDir, name: &str) -> Option<WorkingDir> {
    let file = resolve_mount_target(Some(cwd), name)?;
    (file.kind == FsNodeKind::Directory)
        .then_some(WorkingDir::new(file.node.mount_id, file.node.ino))
}

pub(crate) fn mkdir_at(cwd: WorkingDir, name: &str, mode: u32) -> Option<()> {
    let target = resolve_create_parent(Some(cwd), name)?;
    with_mount(target.parent.mount_id, |mount| {
        if mount
            .lookup_component_from(target.parent.ino, target.leaf_name)
            .is_some()
        {
            return None;
        }
        mount.create_dir(target.parent.ino, target.leaf_name, mode)
    })
    .expect("filesystem mount is missing")?;
    Some(())
}

pub(crate) fn unlink_file_at(cwd: WorkingDir, name: &str) -> Option<()> {
    let target = resolve_create_parent(Some(cwd), name)?;
    with_mount(target.parent.mount_id, |mount| {
        let (_, kind) = mount.lookup_component_from(target.parent.ino, target.leaf_name)?;
        if kind == FsNodeKind::Directory {
            return None;
        }
        mount.unlink(target.parent.ino, target.leaf_name)
    })
    .expect("filesystem mount is missing")?;
    Some(())
}
