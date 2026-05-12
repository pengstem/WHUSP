use super::inode::OpenFlags;
use crate::sync::UPIntrFreeCell;

/// Interior storage for file status flags on a shared open file description.
///
/// Duplicated file descriptors should share this cell; per-descriptor flags
/// such as close-on-exec remain in `FdTableEntry`.
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
