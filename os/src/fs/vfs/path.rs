use super::super::mount::{
    mount_supports_dentry_cache, mounted_root_for, mounted_root_for_synthetic_child,
    mounted_root_parent, primary_mount_id, root_ino_for, with_mount,
};
use super::super::path::PathContext;
use super::super::{dentry_cache, dentry_cache::DentryLookupResult};
use super::{FsError, FsNodeKind, FsResult, VfsNodeId};
use crate::perf;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const EXT4_NAME_MAX: usize = 255;
const SYMLINK_TARGET_MAX: usize = 4096;
const MAX_SYMLINK_FOLLOWS: usize = 40; // Linux returns ELOOP after 40 symlink resolutions.

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VfsPath {
    pub(crate) node: VfsNodeId,
    pub(crate) kind: FsNodeKind,
    pub(crate) visible_path: Option<String>,
}

pub(crate) struct VfsCreateTarget<'a> {
    pub(crate) parent: VfsNodeId,
    pub(crate) leaf_name: &'a str,
    pub(crate) leaf_path: String,
}

pub(crate) enum VfsOpenTarget<'a> {
    Existing(VfsPath),
    Create(VfsCreateTarget<'a>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LookupMode {
    // Normal open/stat-style lookup: follow final symlinks and mounted roots.
    FollowFinal,
    // lstat/readlink/O_NOFOLLOW-style lookup: keep a final symlink as the node.
    NoFollowFinal,
    // mount/umount target lookup: follow the final symlink but stop before a
    // final mounted root so callers operate on the covered directory itself.
    MountTarget,
}

impl LookupMode {
    fn follow_final_symlink(self) -> bool {
        matches!(self, Self::FollowFinal | Self::MountTarget)
    }

    fn follow_final_mount(self) -> bool {
        !matches!(self, Self::MountTarget)
    }
}

#[derive(Clone, Debug)]
struct VfsCursor {
    node: VfsNodeId,
    kind: FsNodeKind,
    path: String,
}

#[derive(Debug)]
struct VfsChildLookup {
    cursor: VfsCursor,
    parent_node: VfsNodeId,
    parent_kind: FsNodeKind,
    parent_path_len: usize,
}

#[derive(Clone, Debug)]
enum PathComponent<'a> {
    Borrowed(&'a str),
    Owned(String),
}

impl PathComponent<'_> {
    fn as_str(&self) -> &str {
        match self {
            Self::Borrowed(component) => component,
            Self::Owned(component) => component.as_str(),
        }
    }
}

impl VfsPath {
    pub(crate) fn new(node: VfsNodeId, kind: FsNodeKind) -> Self {
        Self {
            node,
            kind,
            visible_path: None,
        }
    }

    pub(crate) fn with_visible_path(
        node: VfsNodeId,
        kind: FsNodeKind,
        visible_path: String,
    ) -> Self {
        Self {
            node,
            kind,
            visible_path: Some(visible_path),
        }
    }
}

impl VfsCreateTarget<'_> {
    pub(crate) fn synthetic_child(&self, context: &PathContext) -> Option<VfsPath> {
        mounted_root_for_synthetic_child(
            context.namespace_id(),
            self.parent,
            self.leaf_path.as_str(),
        )
        .map(|node| VfsPath::with_visible_path(node, FsNodeKind::Directory, self.leaf_path.clone()))
    }
}

impl VfsCursor {
    fn root(context: &PathContext) -> Self {
        let root = context.root();
        Self {
            node: VfsNodeId::new(root.mount_id(), root.ino()),
            kind: FsNodeKind::Directory,
            path: String::from(context.root_path()),
        }
    }

    fn from_working_dir(context: &PathContext) -> Self {
        let cwd = context.cwd();
        Self {
            node: VfsNodeId::new(cwd.mount_id(), cwd.ino()),
            kind: FsNodeKind::Directory,
            path: String::from(context.cwd_path()),
        }
    }

    fn into_path(self) -> VfsPath {
        VfsPath::with_visible_path(self.node, self.kind, self.path)
    }

    fn is_mount_root(&self) -> bool {
        root_ino_for(self.node.mount_id).is_some_and(|root_ino| self.node.ino == root_ino)
    }
}

impl VfsChildLookup {
    fn into_parent(mut self) -> VfsCursor {
        self.cursor.path.truncate(self.parent_path_len);
        VfsCursor {
            node: self.parent_node,
            kind: self.parent_kind,
            path: self.cursor.path,
        }
    }
}

fn follow_mounted_root(context: &PathContext, cursor: VfsCursor) -> VfsCursor {
    if cursor.kind != FsNodeKind::Directory {
        return cursor;
    }
    if let Some(node) = mounted_root_for(context.namespace_id(), cursor.node, cursor.path.as_str())
    {
        return VfsCursor {
            node,
            kind: FsNodeKind::Directory,
            path: cursor.path,
        };
    }
    cursor
}

fn join_visible_path(base: &str, component: &str) -> String {
    perf::record_vfs_visible_path_update(1);
    if base == "/" {
        alloc::format!("/{component}")
    } else {
        alloc::format!("{base}/{component}")
    }
}

fn reserve_visible_path(path: &mut String, additional: usize) {
    if path.capacity().saturating_sub(path.len()) < additional {
        perf::record_vfs_visible_path_allocation();
        path.reserve(additional);
    }
}

fn reserve_visible_path_for_lookup(cursor: &mut VfsCursor, path: &str) {
    reserve_visible_path(&mut cursor.path, path.len().saturating_add(1));
}

fn push_visible_path_component(path: &mut String, component: &str) {
    perf::record_vfs_visible_path_update(0);
    let slash_len = usize::from(path.as_str() != "/");
    reserve_visible_path(path, slash_len + component.len());
    if path.as_str() != "/" {
        path.push('/');
    }
    path.push_str(component);
}

fn truncate_visible_path_parent(path: &mut String) {
    perf::record_vfs_visible_path_update(0);
    if path.as_str() == "/" {
        return;
    }
    let parent_len = match path.rfind('/') {
        Some(0) | None => 1,
        Some(index) => index,
    };
    path.truncate(parent_len);
}

fn parent_visible_path(path: &str) -> String {
    perf::record_vfs_visible_path_update(1);
    if path == "/" {
        return String::from("/");
    }
    match path.rsplit_once('/') {
        Some(("", _)) | None => String::from("/"),
        Some((parent, _)) => String::from(parent),
    }
}

fn lookup_child_raw(
    context: &PathContext,
    mut cursor: VfsCursor,
    component: &str,
) -> FsResult<VfsChildLookup> {
    if cursor.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    if component.len() > EXT4_NAME_MAX {
        return Err(FsError::NameTooLong);
    }

    let parent_node = cursor.node;
    let parent_kind = cursor.kind;
    let parent_path_len = cursor.path.len();
    if component == ".." {
        truncate_visible_path_parent(&mut cursor.path);
    } else {
        push_visible_path_component(&mut cursor.path, component);
    };
    if component != ".."
        && let Some(node) = mounted_root_for_synthetic_child(
            context.namespace_id(),
            parent_node,
            cursor.path.as_str(),
        )
    {
        cursor.node = node;
        cursor.kind = FsNodeKind::Directory;
        return Ok(VfsChildLookup {
            cursor,
            parent_node,
            parent_kind,
            parent_path_len,
        });
    }

    let cacheable = component != ".." && mount_supports_dentry_cache(parent_node.mount_id);
    if cacheable {
        match dentry_cache::lookup(context.namespace_id(), parent_node, component) {
            Some(DentryLookupResult::Positive { node, kind }) => {
                cursor.node = node;
                cursor.kind = kind;
                return Ok(VfsChildLookup {
                    cursor,
                    parent_node,
                    parent_kind,
                    parent_path_len,
                });
            }
            Some(DentryLookupResult::Negative) => return Err(FsError::NotFound),
            None => {}
        }
    }

    let result = with_mount(parent_node.mount_id, |mount| {
        mount.lookup_component_from(parent_node.ino, component)
    })
    .ok_or(FsError::Io)?;
    let (ino, kind) = match result {
        Ok(found) => found,
        Err(FsError::NotFound) => {
            if cacheable {
                dentry_cache::insert_negative(context.namespace_id(), parent_node, component);
            }
            return Err(FsError::NotFound);
        }
        Err(err) => return Err(err),
    };
    let node = VfsNodeId::new(parent_node.mount_id, ino);
    if cacheable {
        dentry_cache::insert_positive(context.namespace_id(), parent_node, component, node, kind);
    }
    cursor.node = node;
    cursor.kind = kind;
    Ok(VfsChildLookup {
        cursor,
        parent_node,
        parent_kind,
        parent_path_len,
    })
}

fn lookup_parent(context: &PathContext, cursor: VfsCursor) -> FsResult<VfsCursor> {
    if cursor.is_mount_root() {
        if cursor.node.mount_id == primary_mount_id() {
            return Ok(VfsCursor::root(context));
        }
        if let Some(parent) =
            mounted_root_parent(context.namespace_id(), cursor.node, cursor.path.as_str())
        {
            return Ok(VfsCursor {
                node: parent,
                kind: FsNodeKind::Directory,
                path: parent_visible_path(cursor.path.as_str()),
            });
        }
        // UNFINISHED: This kernel still allows unmounting without mount-user
        // reference checks, so a cwd can point at a detached mounted root. Linux
        // keeps such paths alive through mount references; we currently fall
        // back to `/` for that orphaned case.
        return Ok(VfsCursor::root(context));
    }
    lookup_child_raw(context, cursor, "..").map(|child| child.cursor)
}

fn lookup_parent_in_context(cursor: VfsCursor, context: &PathContext) -> FsResult<VfsCursor> {
    let root = context.root();
    if cursor.node == VfsNodeId::new(root.mount_id(), root.ino()) {
        return Ok(cursor);
    }
    lookup_parent(context, cursor)
}

fn start_cursor(context: &PathContext, path: &str) -> VfsCursor {
    if path.starts_with('/') {
        VfsCursor::root(context)
    } else {
        VfsCursor::from_working_dir(context)
    }
}

fn borrowed_path_components(path: &str) -> Vec<PathComponent<'_>> {
    let components: Vec<PathComponent<'_>> = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .map(PathComponent::Borrowed)
        .collect();
    perf::record_vfs_path_components(components.len(), 0);
    components
}

fn owned_path_components<'a>(path: &str) -> Vec<PathComponent<'a>> {
    let components: Vec<PathComponent<'a>> = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .map(|component| PathComponent::Owned(String::from(component)))
        .collect();
    perf::record_vfs_path_components(components.len(), components.len());
    components
}

fn read_symlink_target(cursor: &VfsCursor) -> FsResult<String> {
    let mut buffer = vec![0u8; SYMLINK_TARGET_MAX + 1];
    let len = with_mount(cursor.node.mount_id, |mount| {
        mount.readlink(cursor.node.ino, &mut buffer)
    })
    .ok_or(FsError::Io)??;
    if len > SYMLINK_TARGET_MAX {
        return Err(FsError::NameTooLong);
    }
    let target = core::str::from_utf8(&buffer[..len]).map_err(|_| FsError::InvalidInput)?;
    Ok(String::from(target))
}

fn resolve_path_inner(context: PathContext, path: &str, mode: LookupMode) -> FsResult<VfsCursor> {
    if path.is_empty() {
        return Err(FsError::NotFound);
    }
    let mut cursor = start_cursor(&context, path);
    reserve_visible_path_for_lookup(&mut cursor, path);
    let mut components = borrowed_path_components(path);
    let mut index = 0usize;
    let mut symlink_follows = 0usize;

    if mode.follow_final_mount() && components.is_empty() {
        cursor = follow_mounted_root(&context, cursor);
    }
    while index < components.len() {
        let is_final = index + 1 == components.len();
        let component = components[index].as_str();
        if component == ".." {
            cursor = lookup_parent_in_context(cursor, &context)?;
        } else {
            let child = lookup_child_raw(&context, cursor, component)?;
            if child.cursor.kind == FsNodeKind::Symlink
                && (!is_final || mode.follow_final_symlink())
            {
                if symlink_follows == MAX_SYMLINK_FOLLOWS {
                    return Err(FsError::Loop);
                }
                symlink_follows += 1;

                let target = read_symlink_target(&child.cursor)?;
                let mut next_components = owned_path_components(target.as_str());
                next_components.extend(components[index + 1..].iter().cloned());
                components = next_components;
                index = 0;
                cursor = if target.starts_with('/') {
                    VfsCursor::root(&context)
                } else {
                    child.into_parent()
                };
                reserve_visible_path_for_lookup(&mut cursor, target.as_str());
                if mode.follow_final_mount() && components.is_empty() {
                    cursor = follow_mounted_root(&context, cursor);
                }
                continue;
            } else {
                cursor = child.cursor;
            }
        }
        if mode.follow_final_mount() || !is_final {
            cursor = follow_mounted_root(&context, cursor);
        }
        index += 1;
    }
    Ok(cursor)
}

fn split_parent_path(path: &str) -> FsResult<(&str, &str)> {
    if path.is_empty() {
        return Err(FsError::NotFound);
    }
    let (parent_path, leaf_name) = match path.rsplit_once('/') {
        Some((parent_path, leaf_name)) => (parent_path, leaf_name),
        None => ("", path),
    };
    if leaf_name.is_empty() || leaf_name == "." || leaf_name == ".." {
        return Err(FsError::InvalidInput);
    }
    if leaf_name.len() > EXT4_NAME_MAX {
        return Err(FsError::NameTooLong);
    }
    Ok((parent_path, leaf_name))
}

fn parent_path_for_lookup<'a>(path: &str, parent_path: &'a str) -> &'a str {
    if path.starts_with('/') && parent_path.is_empty() {
        "/"
    } else {
        parent_path
    }
}

pub(crate) fn resolve_existing_in(
    context: PathContext,
    path: &str,
    mode: LookupMode,
) -> FsResult<VfsPath> {
    let resolved = resolve_path_inner(context, path, mode)?.into_path();
    if path.ends_with('/') && resolved.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    Ok(resolved)
}

pub(crate) fn resolve_mount_target_in(context: PathContext, path: &str) -> FsResult<VfsPath> {
    resolve_existing_in(context, path, LookupMode::MountTarget)
}

pub(crate) fn resolve_create_parent_in(
    context: PathContext,
    path: &str,
) -> FsResult<VfsCreateTarget<'_>> {
    let (parent_path, leaf_name) = split_parent_path(path)?;
    let parent_path = parent_path_for_lookup(path, parent_path);
    let parent = if parent_path.is_empty() {
        let cursor = start_cursor(&context, path);
        follow_mounted_root(&context, cursor)
    } else {
        resolve_path_inner(context.clone(), parent_path, LookupMode::FollowFinal)?
    };
    if parent.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    Ok(VfsCreateTarget {
        parent: parent.node,
        leaf_name,
        leaf_path: join_visible_path(parent.path.as_str(), leaf_name),
    })
}

pub(crate) fn resolve_open_in(
    context: PathContext,
    path: &str,
    follow_final_symlink: bool,
    for_create: bool,
) -> FsResult<VfsOpenTarget<'_>> {
    let mode = if follow_final_symlink {
        LookupMode::FollowFinal
    } else {
        LookupMode::NoFollowFinal
    };
    match resolve_existing_in(context.clone(), path, mode) {
        Ok(existing) => return Ok(VfsOpenTarget::Existing(existing)),
        Err(FsError::NotFound) if for_create => {}
        Err(err) => return Err(err),
    }

    if !for_create {
        return Err(FsError::NotFound);
    }
    Ok(VfsOpenTarget::Create(resolve_create_parent_in(
        context, path,
    )?))
}
