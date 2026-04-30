use super::super::mount::MountId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VfsNodeId {
    pub(crate) mount_id: MountId,
    pub(crate) ino: u32,
}

impl VfsNodeId {
    pub(crate) fn new(mount_id: MountId, ino: u32) -> Self {
        Self { mount_id, ino }
    }
}
