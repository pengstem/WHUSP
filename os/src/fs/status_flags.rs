use super::inode::OpenFlags;
use crate::sync::UPIntrFreeCell;

pub(super) struct StatusFlagsCell(UPIntrFreeCell<OpenFlags>);

impl StatusFlagsCell {
    pub(super) fn new(flags: OpenFlags) -> Self {
        Self(unsafe { UPIntrFreeCell::new(flags) })
    }

    pub(super) fn get(&self) -> OpenFlags {
        *self.0.exclusive_access()
    }

    pub(super) fn set(&self, flags: OpenFlags) {
        *self.0.exclusive_access() = flags;
    }
}
