use super::super::mount::{
    mounted_root_for, mounted_root_for_synthetic_child, mounted_root_parent, primary_mount_id,
    root_ino_for, with_mount,
};
use super::super::path::{PathContext, WorkingDir};
use super::{FsError, FsNodeKind, FsResult, VfsNodeId};
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
    FollowFinal,
    NoFollowFinal,
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

    pub(crate) fn working_dir(self) -> Option<WorkingDir> {
        (self.kind == FsNodeKind::Directory)
            .then_some(WorkingDir::new(self.node.mount_id, self.node.ino))
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

    fn as_path(self) -> VfsPath {
        VfsPath::with_visible_path(self.node, self.kind, self.path)
    }

    fn is_mount_root(&self) -> bool {
        root_ino_for(self.node.mount_id).is_some_and(|root_ino| self.node.ino == root_ino)
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
    if base == "/" {
        alloc::format!("/{component}")
    } else {
        alloc::format!("{base}/{component}")
    }
}

fn parent_visible_path(path: &str) -> String {
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
    cursor: VfsCursor,
    component: &str,
) -> FsResult<VfsCursor> {
    if cursor.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    if component.len() > EXT4_NAME_MAX {
        return Err(FsError::NameTooLong);
    }

    let child_path = join_visible_path(cursor.path.as_str(), component);
    if component != ".."
        && let Some(node) = mounted_root_for_synthetic_child(
            context.namespace_id(),
            cursor.node,
            child_path.as_str(),
        )
    {
        return Ok(VfsCursor {
            node,
            kind: FsNodeKind::Directory,
            path: child_path,
        });
    }

    let (ino, kind) = with_mount(cursor.node.mount_id, |mount| {
        mount.lookup_component_from(cursor.node.ino, component)
    })
    .ok_or(FsError::Io)??;
    Ok(VfsCursor {
        node: VfsNodeId::new(cursor.node.mount_id, ino),
        kind,
        path: child_path,
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
    lookup_child_raw(context, cursor, "..")
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

fn path_components(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .map(String::from)
        .collect()
}

fn read_symlink_target(cursor: VfsCursor) -> FsResult<String> {
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
    let mut components = path_components(path);
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
            let parent = cursor.clone();
            cursor = lookup_child_raw(&context, cursor, component)?;
            if cursor.kind == FsNodeKind::Symlink && (!is_final || mode.follow_final_symlink()) {
                if symlink_follows == MAX_SYMLINK_FOLLOWS {
                    return Err(FsError::Loop);
                }
                symlink_follows += 1;

                let target = read_symlink_target(cursor)?;
                let mut next_components = path_components(target.as_str());
                next_components.extend(components[index + 1..].iter().cloned());
                components = next_components;
                index = 0;
                cursor = if target.starts_with('/') {
                    VfsCursor::root(&context)
                } else {
                    parent
                };
                if mode.follow_final_mount() && components.is_empty() {
                    cursor = follow_mounted_root(&context, cursor);
                }
                continue;
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
    let resolved = resolve_path_inner(context, path, mode)?.as_path();
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
