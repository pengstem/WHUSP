use super::super::mount::MountId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct VfsNodeId {
    pub(super) mount_id: MountId,
    pub(super) ino: u32,
}

impl VfsNodeId {
    pub(super) fn new(mount_id: MountId, ino: u32) -> Self {
        Self { mount_id, ino }
    }
}
