use super::mount::{mounted_root_for, with_mount};
use super::path::WorkingDir;
use super::vfs::{
    FsError, FsNodeKind, FsResult, VfsNodeId, resolve_create_parent, resolve_mount_target,
};
use bitflags::*;

// UNFINISHED: Linux open/openat define additional status and creation flags
// such as O_SYNC, O_DSYNC, O_PATH, O_TMPFILE, O_NOATIME, and O_ASYNC. This
// kernel accepts only the flags represented below.
bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct OpenFlags: u32 {
        const RDONLY = 0;
        const WRONLY = 1 << 0;
        const RDWR = 1 << 1;
        const CREATE = 0o100;
        const EXCL = 0o200;
        const NOCTTY = 0o400;
        const TRUNC = 0o1000;
        const APPEND = 0o2000;
        const NONBLOCK = 0o4000;
        const DIRECT = 0o40000;
        const LARGEFILE = 0o100000;
        const DIRECTORY = 0o200000;
        const NOFOLLOW = 0o400000;
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

fn trimmed_nonroot_path(name: &str) -> &str {
    let trimmed = name.trim_end_matches('/');
    if trimmed.is_empty() { name } else { trimmed }
}

fn final_component(name: &str) -> Option<&str> {
    if name.is_empty() {
        return None;
    }
    let trimmed = name.trim_end_matches('/');
    if trimmed.is_empty() {
        return Some("/");
    }
    trimmed.rsplit('/').find(|component| !component.is_empty())
}

pub(crate) fn lookup_mount_target_dir_at(cwd: WorkingDir, name: &str) -> FsResult<WorkingDir> {
    let file = resolve_mount_target(Some(cwd), name)?;
    if file.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    Ok(WorkingDir::new(file.node.mount_id, file.node.ino))
}

pub(crate) fn mkdir_at(cwd: WorkingDir, name: &str, mode: u32) -> FsResult {
    match final_component(name) {
        Some("." | ".." | "/") => return Err(FsError::AlreadyExists),
        _ => {}
    }
    let target = resolve_create_parent(Some(cwd), trimmed_nonroot_path(name))?;
    with_mount(target.parent.mount_id, |mount| {
        match mount.lookup_component_from(target.parent.ino, target.leaf_name) {
            Ok(_) => return Err(FsError::AlreadyExists),
            Err(FsError::NotFound) => {}
            Err(err) => return Err(err),
        }
        mount.create_dir(target.parent.ino, target.leaf_name, mode)
    })
    .ok_or(FsError::Io)??;
    Ok(())
}

pub(crate) fn unlink_file_at(cwd: WorkingDir, name: &str) -> FsResult {
    let has_trailing_slash = name.len() > 1 && name.ends_with('/');
    match final_component(name) {
        Some("." | ".." | "/") => return Err(FsError::IsDir),
        _ => {}
    }
    let target = resolve_create_parent(Some(cwd), trimmed_nonroot_path(name))?;
    with_mount(target.parent.mount_id, |mount| {
        let (_, kind) = mount.lookup_component_from(target.parent.ino, target.leaf_name)?;
        if has_trailing_slash && kind != FsNodeKind::Directory {
            return Err(FsError::NotDir);
        }
        if kind == FsNodeKind::Directory {
            return Err(FsError::IsDir);
        }
        mount.unlink(target.parent.ino, target.leaf_name)
    })
    .ok_or(FsError::Io)??;
    Ok(())
}

pub(crate) fn rmdir_at(cwd: WorkingDir, name: &str) -> FsResult {
    match final_component(name) {
        Some(".") => return Err(FsError::InvalidInput),
        Some("..") => return Err(FsError::NotEmpty),
        Some("/") => return Err(FsError::Busy),
        _ => {}
    }

    let target = resolve_create_parent(Some(cwd), trimmed_nonroot_path(name))?;
    with_mount(target.parent.mount_id, |mount| {
        let (ino, kind) = mount.lookup_component_from(target.parent.ino, target.leaf_name)?;
        if kind != FsNodeKind::Directory {
            return Err(FsError::NotDir);
        }
        let node = VfsNodeId::new(target.parent.mount_id, ino);
        if mounted_root_for(node).is_some() || ino == lwext4_rust::ffi::EXT4_ROOT_INO {
            return Err(FsError::Busy);
        }
        mount.unlink(target.parent.ino, target.leaf_name)
    })
    .ok_or(FsError::Io)??;
    Ok(())
}
