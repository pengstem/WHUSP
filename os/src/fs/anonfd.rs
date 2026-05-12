use super::status_flags::StatusFlagsCell;
use super::{File, FileStat, FsResult, OpenFlags};
use crate::mm::UserBuffer;
use alloc::sync::Arc;
use core::any::Any;

pub(crate) struct AnonymousFd {
    readable: bool,
    writable: bool,
    mode: u32,
    status_flags: StatusFlagsCell,
}

impl AnonymousFd {
    fn new(readable: bool, writable: bool, mode: u32) -> Self {
        Self {
            readable,
            writable,
            mode,
            status_flags: StatusFlagsCell::new(OpenFlags::empty()),
        }
    }
}

impl File for AnonymousFd {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        self.readable
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn read(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn write(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn stat(&self) -> FsResult<FileStat> {
        Ok(FileStat::with_mode(self.mode))
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }
}

pub(crate) fn make_anonymous_fd(
    readable: bool,
    writable: bool,
    mode: u32,
) -> Arc<dyn File + Send + Sync> {
    Arc::new(AnonymousFd::new(readable, writable, mode))
}
