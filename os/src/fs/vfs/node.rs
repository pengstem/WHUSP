use super::super::mount::MountId;

/// Stable identity of a node within one mounted backend.
///
/// This is not a pathname. Callers that bypass path lookup, such as file-handle
/// or executable-open tracking paths, must preserve the mount id together with
/// the inode so dynamic mounts and same-number inodes on different filesystems
/// stay distinguishable.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct VfsNodeId {
    pub(crate) mount_id: MountId,
    pub(crate) ino: u32,
}

impl VfsNodeId {
    pub(crate) fn new(mount_id: MountId, ino: u32) -> Self {
        Self { mount_id, ino }
    }
}
