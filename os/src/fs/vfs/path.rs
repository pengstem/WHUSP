use super::super::ext4::FsNodeKind;
use super::super::path::{self as fs_path, WorkingDir};
use super::VfsNodeId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct VfsPath {
    pub(super) node: VfsNodeId,
    pub(super) kind: FsNodeKind,
}

pub(super) struct VfsCreateTarget<'a> {
    pub(super) parent: VfsNodeId,
    pub(super) leaf_name: &'a str,
}

pub(super) enum VfsOpenTarget<'a> {
    Existing(VfsPath),
    Create(VfsCreateTarget<'a>),
}

impl VfsPath {
    pub(super) fn new(node: VfsNodeId, kind: FsNodeKind) -> Self {
        Self { node, kind }
    }

    fn from_resolved(file: fs_path::ResolvedFile) -> Self {
        Self {
            node: VfsNodeId::new(file.mount_id, file.ino),
            kind: file.kind,
        }
    }

    pub(super) fn working_dir(self) -> Option<WorkingDir> {
        (self.kind == FsNodeKind::Directory)
            .then_some(WorkingDir::new(self.node.mount_id, self.node.ino))
    }
}

pub(super) fn resolve_existing(cwd: Option<WorkingDir>, path: &str) -> Option<VfsPath> {
    let fs_path::ResolvedOpen::Existing(file) =
        fs_path::resolve_open_target(cwd, path, false, false)?
    else {
        return None;
    };
    Some(VfsPath::from_resolved(file))
}

pub(super) fn resolve_open(
    cwd: Option<WorkingDir>,
    path: &str,
    require_writable: bool,
    for_create: bool,
) -> Option<VfsOpenTarget<'_>> {
    match fs_path::resolve_open_target(cwd, path, require_writable, for_create)? {
        fs_path::ResolvedOpen::Existing(file) => {
            Some(VfsOpenTarget::Existing(VfsPath::from_resolved(file)))
        }
        fs_path::ResolvedOpen::Create(target) => Some(VfsOpenTarget::Create(VfsCreateTarget {
            parent: VfsNodeId::new(target.mount_id, target.parent_ino),
            leaf_name: target.leaf_name,
        })),
    }
}
