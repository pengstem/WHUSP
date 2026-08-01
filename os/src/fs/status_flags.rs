use super::inode::OpenFlags;
use crate::sync::SpinNoIrqLock;

/// Interior storage for file status flags on a shared open file description.
///
/// Duplicated file descriptors should share this cell; per-descriptor flags
/// such as close-on-exec remain in `FdTableEntry`.
pub(super) struct StatusFlagsCell(SpinNoIrqLock<OpenFlags>);

impl StatusFlagsCell {
    pub(super) fn new(flags: OpenFlags) -> Self {
        Self(SpinNoIrqLock::new(flags))
    }

    pub(super) fn get(&self) -> OpenFlags {
        *self.0.lock()
    }

    pub(super) fn set(&self, flags: OpenFlags) {
        *self.0.lock() = flags;
    }
}
