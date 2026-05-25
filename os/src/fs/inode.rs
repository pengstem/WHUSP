use super::dentry_cache;
use super::mount::{mounted_root_for_any_path, with_mount};
use super::path::{PathContext, WorkingDir};
use super::vfs::{
    FsError, FsNodeKind, FsResult, LookupMode, VfsCreateTarget, VfsNodeId,
    invalidate_regular_file_read_cache, resolve_create_parent_in, resolve_existing_in,
    resolve_mount_target_in,
};
use bitflags::*;
use lwext4_rust::ffi::EXT4_ROOT_INO;

// UNFINISHED: Linux open/openat define additional status and creation flags
// such as O_ASYNC. This kernel accepts only the flags represented below.
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
        const DSYNC = 0o10000;
        const DIRECT = 0o40000;
        const LARGEFILE = 0o100000;
        const DIRECTORY = 0o200000;
        const NOFOLLOW = 0o400000;
        const NOATIME = 0o1000000;
        const CLOEXEC = 0o2000000;
        const SYNC = 0o4010000;
        const PATH = 0o10000000;
        const TMPFILE = 0o20200000;
    }
}

impl OpenFlags {
    const ACCESS_MODE_MASK: u32 = 0b11;
    const FCNTL_MUTABLE_STATUS_MASK: u32 =
        OpenFlags::APPEND.bits() | OpenFlags::NONBLOCK.bits() | OpenFlags::DIRECT.bits();
    const TMPFILE_BASE_MASK: u32 = OpenFlags::TMPFILE.bits() & !OpenFlags::DIRECTORY.bits();
    // CONTEXT: O_DSYNC/O_SYNC are accepted for libc/LTP compatibility even
    // though this filesystem layer does not yet provide synchronous writeback
    // semantics beyond its existing fsync path.
    const ACCEPTED_SYNC_STATUS_MASK: u32 = OpenFlags::DSYNC.bits() | OpenFlags::SYNC.bits();

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

    pub fn has_tmpfile_base_bit(&self) -> bool {
        self.bits() & Self::TMPFILE_BASE_MASK != 0
    }

    pub fn file_status_flags(flags: Self) -> Self {
        Self::from_bits_truncate(
            flags.bits()
                & (Self::ACCESS_MODE_MASK
                    | Self::PATH.bits()
                    | Self::NOATIME.bits()
                    | Self::FCNTL_MUTABLE_STATUS_MASK
                    | Self::ACCEPTED_SYNC_STATUS_MASK),
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

const MODE_PERMISSIONS_MASK: u32 = 0o7777;
const MODE_SETGID: u32 = 0o2000;

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

fn lookup_create_target(
    context: &PathContext,
    target: &VfsCreateTarget<'_>,
) -> FsResult<Option<(VfsNodeId, FsNodeKind, bool)>> {
    if let Some(path) = target.synthetic_child(context) {
        return Ok(Some((path.node, path.kind, true)));
    }
    match lookup_node(target.parent, target.leaf_name) {
        Ok((node, kind)) => Ok(Some((node, kind, false))),
        Err(FsError::NotFound) => Ok(None),
        Err(err) => Err(err),
    }
}

fn ensure_create_target_absent(
    context: &PathContext,
    target: &VfsCreateTarget<'_>,
    trailing_slash: bool,
) -> FsResult {
    if let Some((_, kind, _)) = lookup_create_target(context, target)? {
        if trailing_slash && kind != FsNodeKind::Directory {
            return Err(FsError::NotDir);
        }
        return Err(FsError::AlreadyExists);
    }
    if trailing_slash {
        return Err(FsError::NotFound);
    }
    Ok(())
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

pub(crate) fn lookup_mount_target_dir_in(context: PathContext, name: &str) -> FsResult<WorkingDir> {
    let file = resolve_mount_target_in(context, name)?;
    if file.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    Ok(WorkingDir::new(file.node.mount_id, file.node.ino))
}

pub(crate) fn lookup_existing_dir_in(context: PathContext, name: &str) -> FsResult<WorkingDir> {
    let file = resolve_existing_in(context, name, LookupMode::FollowFinal)?;
    if file.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    Ok(WorkingDir::new(file.node.mount_id, file.node.ino))
}

pub(crate) fn mkdir_in(context: PathContext, name: &str, mode: u32) -> FsResult {
    if let Some("." | ".." | "/") = final_component(name) {
        return Err(FsError::AlreadyExists);
    }
    let target = resolve_create_parent_in(context.clone(), trimmed_nonroot_path(name))?;
    ensure_create_target_absent(&context, &target, false)?;
    with_mount(target.parent.mount_id, |mount| {
        mount.create_dir(target.parent.ino, target.leaf_name, mode)
    })
    .ok_or(FsError::Io)??;
    dentry_cache::invalidate_parent(target.parent);
    Ok(())
}

pub(crate) fn create_node_in(
    context: PathContext,
    name: &str,
    kind: FsNodeKind,
    mode: u32,
    uid: u32,
    gid: u32,
    rdev: u64,
) -> FsResult {
    match final_component(name) {
        None => return Err(FsError::NotFound),
        Some("." | ".." | "/") => return Err(FsError::AlreadyExists),
        _ => {}
    }
    let trailing_slash = has_trailing_slash(name);
    let target = resolve_create_parent_in(context.clone(), trimmed_nonroot_path(name))?;
    ensure_create_target_absent(&context, &target, trailing_slash)?;
    with_mount(target.parent.mount_id, |mount| {
        let parent_stat = mount.stat(target.parent.ino)?;
        let ino = mount.create_node(
            target.parent.ino,
            target.leaf_name,
            kind,
            mode & MODE_PERMISSIONS_MASK,
            rdev,
        )?;
        let gid = if parent_stat.mode & MODE_SETGID != 0 {
            parent_stat.gid
        } else {
            gid
        };
        mount.set_owner(ino, Some(uid), Some(gid))?;
        mount.set_mode(ino, mode & MODE_PERMISSIONS_MASK)
    })
    .ok_or(FsError::Io)??;
    dentry_cache::invalidate_parent(target.parent);
    Ok(())
}

pub(crate) fn link_file_in(
    old_context: PathContext,
    old_name: &str,
    new_context: PathContext,
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
    let old_target = resolve_create_parent_in(old_context.clone(), trimmed_nonroot_path(old_name))?;
    let new_target = resolve_create_parent_in(new_context.clone(), trimmed_nonroot_path(new_name))?;
    if old_target.parent.mount_id != new_target.parent.mount_id {
        return Err(FsError::CrossDevice);
    }

    let Some((old_node, old_kind, _)) = lookup_create_target(&old_context, &old_target)? else {
        return Err(FsError::NotFound);
    };
    if old_has_trailing_slash && old_kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    if old_kind == FsNodeKind::Directory || old_node.ino == EXT4_ROOT_INO {
        return Err(FsError::PermissionDenied);
    }
    ensure_create_target_absent(&new_context, &new_target, new_has_trailing_slash)?;
    with_mount(new_target.parent.mount_id, |mount| {
        mount.link(new_target.parent.ino, new_target.leaf_name, old_node.ino)
    })
    .ok_or(FsError::Io)??;
    dentry_cache::invalidate_parent(new_target.parent);
    Ok(())
}

pub(crate) fn link_node_in(
    old_node: VfsNodeId,
    old_kind: FsNodeKind,
    new_context: PathContext,
    new_name: &str,
) -> FsResult {
    if old_kind == FsNodeKind::Directory || old_node.ino == EXT4_ROOT_INO {
        return Err(FsError::PermissionDenied);
    }
    match final_component(new_name) {
        None => return Err(FsError::NotFound),
        Some("." | ".." | "/") => return Err(FsError::AlreadyExists),
        _ => {}
    }

    let new_has_trailing_slash = has_trailing_slash(new_name);
    let new_target = resolve_create_parent_in(new_context.clone(), trimmed_nonroot_path(new_name))?;
    if old_node.mount_id != new_target.parent.mount_id {
        return Err(FsError::CrossDevice);
    }
    ensure_create_target_absent(&new_context, &new_target, new_has_trailing_slash)?;
    with_mount(new_target.parent.mount_id, |mount| {
        mount.link(new_target.parent.ino, new_target.leaf_name, old_node.ino)
    })
    .ok_or(FsError::Io)??;
    dentry_cache::invalidate_parent(new_target.parent);
    Ok(())
}

pub(crate) fn symlink_in(context: PathContext, target: &str, link_name: &str) -> FsResult {
    match final_component(link_name) {
        None => return Err(FsError::NotFound),
        Some("." | ".." | "/") => return Err(FsError::AlreadyExists),
        _ => {}
    }

    let link_has_trailing_slash = has_trailing_slash(link_name);
    let create_target = resolve_create_parent_in(context.clone(), trimmed_nonroot_path(link_name))?;
    ensure_create_target_absent(&context, &create_target, link_has_trailing_slash)?;
    with_mount(create_target.parent.mount_id, |mount| {
        mount.symlink(
            create_target.parent.ino,
            create_target.leaf_name,
            target.as_bytes(),
        )
    })
    .ok_or(FsError::Io)??;
    dentry_cache::invalidate_parent(create_target.parent);
    Ok(())
}

pub(crate) fn rename_in(
    old_context: PathContext,
    old_name: &str,
    new_context: PathContext,
    new_name: &str,
    no_replace: bool,
) -> FsResult {
    validate_rename_path(old_name)?;
    validate_rename_path(new_name)?;

    let old_has_trailing_slash = has_trailing_slash(old_name);
    let new_has_trailing_slash = has_trailing_slash(new_name);
    let old_target = resolve_create_parent_in(old_context.clone(), trimmed_nonroot_path(old_name))?;
    let new_target = resolve_create_parent_in(new_context.clone(), trimmed_nonroot_path(new_name))?;
    if old_target.parent.mount_id != new_target.parent.mount_id {
        return Err(FsError::CrossDevice);
    }

    let Some((old_node, old_kind, old_is_synthetic)) =
        lookup_create_target(&old_context, &old_target)?
    else {
        return Err(FsError::NotFound);
    };
    if old_has_trailing_slash && old_kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    if old_node.ino == EXT4_ROOT_INO
        || mounted_root_for_any_path(old_context.namespace_id(), old_node).is_some()
    {
        return Err(FsError::Busy);
    }

    let replaced_target = match lookup_create_target(&new_context, &new_target)? {
        Some((new_node, new_kind, new_is_synthetic)) => {
            if new_has_trailing_slash && new_kind != FsNodeKind::Directory {
                return Err(FsError::NotDir);
            }
            if no_replace {
                return Err(FsError::AlreadyExists);
            }
            if old_node == new_node {
                return Ok(());
            }
            if new_is_synthetic
                || mounted_root_for_any_path(new_context.namespace_id(), new_node).is_some()
            {
                return Err(FsError::Busy);
            }
            if old_kind == FsNodeKind::Directory && new_kind != FsNodeKind::Directory {
                return Err(FsError::NotDir);
            }
            if old_kind != FsNodeKind::Directory && new_kind == FsNodeKind::Directory {
                return Err(FsError::IsDir);
            }
            Some((new_node, new_kind))
        }
        None => {
            if new_has_trailing_slash {
                return Err(FsError::NotFound);
            }
            None
        }
    };

    if old_is_synthetic {
        return Err(FsError::Busy);
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
    dentry_cache::invalidate_parent(old_target.parent);
    if new_target.parent != old_target.parent {
        dentry_cache::invalidate_parent(new_target.parent);
    }
    if let Some((node, kind)) = replaced_target {
        invalidate_regular_file_read_cache(node, kind);
    }
    Ok(())
}

pub(crate) fn rename_exchange_in(
    old_context: PathContext,
    old_name: &str,
    new_context: PathContext,
    new_name: &str,
) -> FsResult {
    validate_rename_path(old_name)?;
    validate_rename_path(new_name)?;

    let old_has_trailing_slash = has_trailing_slash(old_name);
    let new_has_trailing_slash = has_trailing_slash(new_name);
    let old_target = resolve_create_parent_in(old_context.clone(), trimmed_nonroot_path(old_name))?;
    let new_target = resolve_create_parent_in(new_context.clone(), trimmed_nonroot_path(new_name))?;
    if old_target.parent.mount_id != new_target.parent.mount_id {
        return Err(FsError::CrossDevice);
    }

    let Some((old_node, old_kind, old_is_synthetic)) =
        lookup_create_target(&old_context, &old_target)?
    else {
        return Err(FsError::NotFound);
    };
    let Some((new_node, new_kind, new_is_synthetic)) =
        lookup_create_target(&new_context, &new_target)?
    else {
        return Err(FsError::NotFound);
    };
    if old_has_trailing_slash && old_kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    if new_has_trailing_slash && new_kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    if old_node == new_node {
        return Ok(());
    }
    if old_node.ino == EXT4_ROOT_INO
        || new_node.ino == EXT4_ROOT_INO
        || old_is_synthetic
        || new_is_synthetic
        || mounted_root_for_any_path(old_context.namespace_id(), old_node).is_some()
        || mounted_root_for_any_path(new_context.namespace_id(), new_node).is_some()
    {
        return Err(FsError::Busy);
    }
    if old_kind == FsNodeKind::Directory && is_descendant_or_self(new_target.parent, old_node)? {
        return Err(FsError::InvalidInput);
    }
    if new_kind == FsNodeKind::Directory && is_descendant_or_self(old_target.parent, new_node)? {
        return Err(FsError::InvalidInput);
    }

    with_mount(old_target.parent.mount_id, |mount| {
        mount.exchange(
            old_target.parent.ino,
            old_target.leaf_name,
            new_target.parent.ino,
            new_target.leaf_name,
        )
    })
    .ok_or(FsError::Io)??;
    dentry_cache::invalidate_parent(old_target.parent);
    if new_target.parent != old_target.parent {
        dentry_cache::invalidate_parent(new_target.parent);
    }
    invalidate_regular_file_read_cache(old_node, old_kind);
    invalidate_regular_file_read_cache(new_node, new_kind);
    Ok(())
}

pub(crate) fn unlink_file_in(context: PathContext, name: &str) -> FsResult {
    let trailing_slash = has_trailing_slash(name);
    if let Some("." | ".." | "/") = final_component(name) {
        return Err(FsError::IsDir);
    }
    let target = resolve_create_parent_in(context.clone(), trimmed_nonroot_path(name))?;
    let Some((node, kind, is_synthetic)) = lookup_create_target(&context, &target)? else {
        return Err(FsError::NotFound);
    };
    if trailing_slash && kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    if kind == FsNodeKind::Directory {
        return Err(FsError::IsDir);
    }
    if is_synthetic {
        return Err(FsError::Busy);
    }
    with_mount(target.parent.mount_id, |mount| {
        mount.unlink(target.parent.ino, target.leaf_name)
    })
    .ok_or(FsError::Io)??;
    dentry_cache::invalidate_parent(target.parent);
    invalidate_regular_file_read_cache(node, kind);
    Ok(())
}

pub(crate) fn rmdir_in(context: PathContext, name: &str) -> FsResult {
    match final_component(name) {
        Some(".") => return Err(FsError::InvalidInput),
        Some("..") => return Err(FsError::NotEmpty),
        Some("/") => return Err(FsError::Busy),
        _ => {}
    }

    let target = resolve_create_parent_in(context.clone(), trimmed_nonroot_path(name))?;
    let Some((node, kind, is_synthetic)) = lookup_create_target(&context, &target)? else {
        return Err(FsError::NotFound);
    };
    if kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    if is_synthetic
        || mounted_root_for_any_path(context.namespace_id(), node).is_some()
        || node.ino == lwext4_rust::ffi::EXT4_ROOT_INO
    {
        return Err(FsError::Busy);
    }
    with_mount(target.parent.mount_id, |mount| {
        mount.unlink(target.parent.ino, target.leaf_name)
    })
    .ok_or(FsError::Io)??;
    dentry_cache::invalidate_parent(target.parent);
    Ok(())
}
