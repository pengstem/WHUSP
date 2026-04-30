use super::super::mount::{mounted_root_for, mounted_root_parent, primary_mount_id, with_mount};
use super::super::path::WorkingDir;
use super::{FsError, FsNodeKind, FsResult, VfsNodeId};
use lwext4_rust::ffi::EXT4_ROOT_INO;

const EXT4_NAME_MAX: usize = 255;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VfsPath {
    pub(crate) node: VfsNodeId,
    pub(crate) kind: FsNodeKind,
}

pub(crate) struct VfsCreateTarget<'a> {
    pub(crate) parent: VfsNodeId,
    pub(crate) leaf_name: &'a str,
}

pub(crate) enum VfsOpenTarget<'a> {
    Existing(VfsPath),
    Create(VfsCreateTarget<'a>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LookupMode {
    Normal,
    MountTarget,
}

#[derive(Clone, Copy, Debug)]
struct VfsCursor {
    node: VfsNodeId,
    kind: FsNodeKind,
}

impl VfsPath {
    pub(crate) fn new(node: VfsNodeId, kind: FsNodeKind) -> Self {
        Self { node, kind }
    }

    pub(crate) fn working_dir(self) -> Option<WorkingDir> {
        (self.kind == FsNodeKind::Directory)
            .then_some(WorkingDir::new(self.node.mount_id, self.node.ino))
    }
}

impl VfsCursor {
    fn root() -> Self {
        Self {
            node: VfsNodeId::new(primary_mount_id(), EXT4_ROOT_INO),
            kind: FsNodeKind::Directory,
        }
    }

    fn from_working_dir(cwd: WorkingDir) -> Self {
        Self {
            node: VfsNodeId::new(cwd.mount_id(), cwd.ino()),
            kind: FsNodeKind::Directory,
        }
    }

    fn as_path(self) -> VfsPath {
        VfsPath::new(self.node, self.kind)
    }

    fn is_mount_root(self) -> bool {
        self.node.ino == EXT4_ROOT_INO
    }
}

fn follow_mounted_root(cursor: VfsCursor) -> VfsCursor {
    if cursor.kind != FsNodeKind::Directory {
        return cursor;
    }
    if let Some(mount_id) = mounted_root_for(cursor.node) {
        return VfsCursor {
            node: VfsNodeId::new(mount_id, EXT4_ROOT_INO),
            kind: FsNodeKind::Directory,
        };
    }
    cursor
}

fn lookup_child_raw(cursor: VfsCursor, component: &str) -> FsResult<VfsCursor> {
    if cursor.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    if component.len() > EXT4_NAME_MAX {
        return Err(FsError::NameTooLong);
    }

    let (ino, kind) = with_mount(cursor.node.mount_id, |mount| {
        mount.lookup_component_from(cursor.node.ino, component)
    })
    .ok_or(FsError::Io)??;
    Ok(VfsCursor {
        node: VfsNodeId::new(cursor.node.mount_id, ino),
        kind,
    })
}

fn lookup_parent(cursor: VfsCursor) -> FsResult<VfsCursor> {
    if cursor.is_mount_root() {
        if cursor.node.mount_id == primary_mount_id() {
            return Ok(VfsCursor::root());
        }
        if let Some(parent) = mounted_root_parent(cursor.node.mount_id) {
            return Ok(VfsCursor {
                node: parent,
                kind: FsNodeKind::Directory,
            });
        }
        // UNFINISHED: This kernel still allows unmounting without mount-user
        // reference checks, so a cwd can point at a detached mounted root. Linux
        // keeps such paths alive through mount references; we currently fall
        // back to `/` for that orphaned case.
        return Ok(VfsCursor::root());
    }
    lookup_child_raw(cursor, "..")
}

fn start_cursor(cwd: Option<WorkingDir>, path: &str) -> VfsCursor {
    if path.starts_with('/') {
        VfsCursor::root()
    } else if let Some(cwd) = cwd {
        VfsCursor::from_working_dir(cwd)
    } else {
        VfsCursor::root()
    }
}

fn maybe_follow_symlink(
    cursor: VfsCursor,
    _mode: LookupMode,
    _is_final: bool,
) -> FsResult<VfsCursor> {
    if cursor.kind == FsNodeKind::Symlink {
        // UNFINISHED: Linux pathname lookup follows symlinks according to
        // LOOKUP_FOLLOW, O_NOFOLLOW, and final-vs-non-final component rules.
        // The current EXT4 wrapper has no readlink path yet, so symlink nodes
        // remain unresolved here.
    }
    Ok(cursor)
}

fn resolve_path_inner(
    cwd: Option<WorkingDir>,
    path: &str,
    mode: LookupMode,
) -> FsResult<VfsCursor> {
    if path.is_empty() {
        return Err(FsError::NotFound);
    }
    let follow_final_mount = mode == LookupMode::Normal;
    let mut cursor = start_cursor(cwd, path);
    let mut components = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .peekable();
    if follow_final_mount && components.peek().is_none() {
        cursor = follow_mounted_root(cursor);
    }
    while let Some(component) = components.next() {
        let is_final = components.peek().is_none();
        if component == ".." {
            cursor = lookup_parent(cursor)?;
        } else {
            cursor = lookup_child_raw(cursor, component)?;
            cursor = maybe_follow_symlink(cursor, mode, is_final)?;
        }
        if follow_final_mount || !is_final {
            cursor = follow_mounted_root(cursor);
        }
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

pub(crate) fn resolve_existing(
    cwd: Option<WorkingDir>,
    path: &str,
    mode: LookupMode,
) -> FsResult<VfsPath> {
    let resolved = resolve_path_inner(cwd, path, mode)?.as_path();
    if path.ends_with('/') && resolved.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    Ok(resolved)
}

pub(crate) fn resolve_mount_target(cwd: Option<WorkingDir>, path: &str) -> FsResult<VfsPath> {
    resolve_existing(cwd, path, LookupMode::MountTarget)
}

pub(crate) fn resolve_create_parent(
    cwd: Option<WorkingDir>,
    path: &str,
) -> FsResult<VfsCreateTarget<'_>> {
    let (parent_path, leaf_name) = split_parent_path(path)?;
    let parent_path = parent_path_for_lookup(path, parent_path);
    let parent = resolve_existing(cwd, parent_path, LookupMode::Normal)?;
    if parent.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    Ok(VfsCreateTarget {
        parent: parent.node,
        leaf_name,
    })
}

pub(crate) fn resolve_open(
    cwd: Option<WorkingDir>,
    path: &str,
    for_create: bool,
) -> FsResult<VfsOpenTarget<'_>> {
    match resolve_existing(cwd, path, LookupMode::Normal) {
        Ok(existing) => return Ok(VfsOpenTarget::Existing(existing)),
        Err(FsError::NotFound) if for_create => {}
        Err(err) => return Err(err),
    }

    if !for_create {
        return Err(FsError::NotFound);
    }
    Ok(VfsOpenTarget::Create(resolve_create_parent(cwd, path)?))
}
