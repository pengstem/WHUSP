use super::mount::{mounted_root_for, with_mount};
use super::path::WorkingDir;
use super::vfs::{
    FsError, FsNodeKind, FsResult, VfsNodeId, resolve_create_parent, resolve_mount_target,
};
use bitflags::*;
use lwext4_rust::ffi::EXT4_ROOT_INO;

// UNFINISHED: Linux open/openat define additional status and creation flags
// such as O_SYNC, O_DSYNC, O_TMPFILE, O_NOATIME, and O_ASYNC. This kernel
// accepts only the flags represented below.
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
        const PATH = 0o10000000;
    }
}

impl OpenFlags {
    const ACCESS_MODE_MASK: u32 = 0b11;
    const FCNTL_MUTABLE_STATUS_MASK: u32 =
        OpenFlags::APPEND.bits() | OpenFlags::NONBLOCK.bits() | OpenFlags::DIRECT.bits();

    pub fn read_write(&self) -> (bool, bool) {
        if self.contains(Self::PATH) {
            return (false, false);
        }
        match self.bits() & Self::ACCESS_MODE_MASK {
            0 => (true, false),
            1 => (false, true),
            2 => (true, true),
            _ => (false, false),
        }
    }

    pub fn writable_target(&self) -> bool {
        if self.contains(Self::PATH) {
            return false;
        }
        matches!(self.bits() & Self::ACCESS_MODE_MASK, 1 | 2)
    }

    pub fn can_open_directory(&self) -> bool {
        !self.writable_target() && !self.contains(Self::CREATE) && !self.contains(Self::TRUNC)
    }

    pub fn file_status_flags(flags: Self) -> Self {
        Self::from_bits_truncate(
            flags.bits()
                & (Self::ACCESS_MODE_MASK | Self::PATH.bits() | Self::FCNTL_MUTABLE_STATUS_MASK),
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

fn has_trailing_slash(name: &str) -> bool {
    name.len() > 1 && name.ends_with('/')
}

fn validate_rename_path(name: &str) -> FsResult {
    match final_component(name) {
        None => Err(FsError::NotFound),
        Some("/") => Err(FsError::Busy),
        Some("." | "..") => Err(FsError::InvalidInput),
        Some(_) => Ok(()),
    }
}

fn lookup_node(parent: VfsNodeId, leaf_name: &str) -> FsResult<(VfsNodeId, FsNodeKind)> {
    let (ino, kind) = with_mount(parent.mount_id, |mount| {
        mount.lookup_component_from(parent.ino, leaf_name)
    })
    .ok_or(FsError::Io)??;
    Ok((VfsNodeId::new(parent.mount_id, ino), kind))
}

// UNFINISHED: MAX_DEPTH is a defensive bound, not strict Linux semantics;
// extremely deep but legitimate directory trees would be misreported as ELOOP.
fn is_descendant_or_self(mut node: VfsNodeId, ancestor: VfsNodeId) -> FsResult<bool> {
    const MAX_DEPTH: usize = 256;
    if node.mount_id != ancestor.mount_id {
        return Ok(false);
    }
    with_mount(node.mount_id, |mount| {
        for _ in 0..MAX_DEPTH {
            if node == ancestor {
                return Ok(true);
            }
            if node.ino == EXT4_ROOT_INO {
                return Ok(false);
            }
            let (parent_ino, kind) = mount.lookup_component_from(node.ino, "..")?;
            if kind != FsNodeKind::Directory || parent_ino == node.ino {
                return Err(FsError::Loop);
            }
            node = VfsNodeId::new(node.mount_id, parent_ino);
        }
        Err(FsError::Loop)
    })
    .ok_or(FsError::Io)?
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

pub(crate) fn link_file_at(
    old_cwd: WorkingDir,
    old_name: &str,
    new_cwd: WorkingDir,
    new_name: &str,
) -> FsResult {
    match final_component(old_name) {
        None => return Err(FsError::NotFound),
        Some("." | ".." | "/") => return Err(FsError::PermissionDenied),
        _ => {}
    }
    match final_component(new_name) {
        None => return Err(FsError::NotFound),
        Some("." | ".." | "/") => return Err(FsError::AlreadyExists),
        _ => {}
    }

    let old_has_trailing_slash = has_trailing_slash(old_name);
    let new_has_trailing_slash = has_trailing_slash(new_name);
    let old_target = resolve_create_parent(Some(old_cwd), trimmed_nonroot_path(old_name))?;
    let new_target = resolve_create_parent(Some(new_cwd), trimmed_nonroot_path(new_name))?;
    if old_target.parent.mount_id != new_target.parent.mount_id {
        return Err(FsError::CrossDevice);
    }

    let (old_node, old_kind) = lookup_node(old_target.parent, old_target.leaf_name)?;
    if old_has_trailing_slash && old_kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    if old_kind == FsNodeKind::Directory || old_node.ino == EXT4_ROOT_INO {
        return Err(FsError::PermissionDenied);
    }
    with_mount(new_target.parent.mount_id, |mount| {
        match mount.lookup_component_from(new_target.parent.ino, new_target.leaf_name) {
            Ok((_, kind)) => {
                if new_has_trailing_slash && kind != FsNodeKind::Directory {
                    return Err(FsError::NotDir);
                }
                return Err(FsError::AlreadyExists);
            }
            Err(FsError::NotFound) => {
                if new_has_trailing_slash {
                    return Err(FsError::NotFound);
                }
            }
            Err(err) => return Err(err),
        }
        mount.link(new_target.parent.ino, new_target.leaf_name, old_node.ino)
    })
    .ok_or(FsError::Io)??;
    Ok(())
}

pub(crate) fn symlink_at(cwd: WorkingDir, target: &str, link_name: &str) -> FsResult {
    match final_component(link_name) {
        None => return Err(FsError::NotFound),
        Some("." | ".." | "/") => return Err(FsError::AlreadyExists),
        _ => {}
    }

    let link_has_trailing_slash = has_trailing_slash(link_name);
    let create_target = resolve_create_parent(Some(cwd), trimmed_nonroot_path(link_name))?;
    with_mount(create_target.parent.mount_id, |mount| {
        match mount.lookup_component_from(create_target.parent.ino, create_target.leaf_name) {
            Ok((_, kind)) => {
                if link_has_trailing_slash && kind != FsNodeKind::Directory {
                    return Err(FsError::NotDir);
                }
                return Err(FsError::AlreadyExists);
            }
            Err(FsError::NotFound) => {
                if link_has_trailing_slash {
                    return Err(FsError::NotFound);
                }
            }
            Err(err) => return Err(err),
        }
        mount.symlink(
            create_target.parent.ino,
            create_target.leaf_name,
            target.as_bytes(),
        )
    })
    .ok_or(FsError::Io)??;
    Ok(())
}

pub(crate) fn rename_at(
    old_cwd: WorkingDir,
    old_name: &str,
    new_cwd: WorkingDir,
    new_name: &str,
    no_replace: bool,
) -> FsResult {
    validate_rename_path(old_name)?;
    validate_rename_path(new_name)?;

    let old_has_trailing_slash = has_trailing_slash(old_name);
    let new_has_trailing_slash = has_trailing_slash(new_name);
    let old_target = resolve_create_parent(Some(old_cwd), trimmed_nonroot_path(old_name))?;
    let new_target = resolve_create_parent(Some(new_cwd), trimmed_nonroot_path(new_name))?;
    if old_target.parent.mount_id != new_target.parent.mount_id {
        return Err(FsError::CrossDevice);
    }

    let (old_node, old_kind) = lookup_node(old_target.parent, old_target.leaf_name)?;
    if old_has_trailing_slash && old_kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    if old_node.ino == EXT4_ROOT_INO || mounted_root_for(old_node).is_some() {
        return Err(FsError::Busy);
    }

    match lookup_node(new_target.parent, new_target.leaf_name) {
        Ok((new_node, new_kind)) => {
            if new_has_trailing_slash && new_kind != FsNodeKind::Directory {
                return Err(FsError::NotDir);
            }
            if no_replace {
                return Err(FsError::AlreadyExists);
            }
            if old_node == new_node {
                return Ok(());
            }
            if mounted_root_for(new_node).is_some() {
                return Err(FsError::Busy);
            }
            if old_kind == FsNodeKind::Directory && new_kind != FsNodeKind::Directory {
                return Err(FsError::NotDir);
            }
            if old_kind != FsNodeKind::Directory && new_kind == FsNodeKind::Directory {
                return Err(FsError::IsDir);
            }
        }
        Err(FsError::NotFound) => {
            if new_has_trailing_slash {
                return Err(FsError::NotFound);
            }
        }
        Err(err) => return Err(err),
    }

    if old_kind == FsNodeKind::Directory && is_descendant_or_self(new_target.parent, old_node)? {
        return Err(FsError::InvalidInput);
    }

    with_mount(old_target.parent.mount_id, |mount| {
        mount.rename(
            old_target.parent.ino,
            old_target.leaf_name,
            new_target.parent.ino,
            new_target.leaf_name,
        )
    })
    .ok_or(FsError::Io)??;
    Ok(())
}

pub(crate) fn unlink_file_at(cwd: WorkingDir, name: &str) -> FsResult {
    let trailing_slash = has_trailing_slash(name);
    match final_component(name) {
        Some("." | ".." | "/") => return Err(FsError::IsDir),
        _ => {}
    }
    let target = resolve_create_parent(Some(cwd), trimmed_nonroot_path(name))?;
    with_mount(target.parent.mount_id, |mount| {
        let (_, kind) = mount.lookup_component_from(target.parent.ino, target.leaf_name)?;
        if trailing_slash && kind != FsNodeKind::Directory {
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
