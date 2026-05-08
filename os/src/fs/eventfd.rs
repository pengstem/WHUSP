use super::status_flags::StatusFlagsCell;
use super::{File, FileStat, FsError, FsResult, OpenFlags, PollEvents, S_IFIFO};
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use crate::task::{current_has_unmasked_signal, suspend_current_and_run_next};
use alloc::sync::Arc;
use core::convert::TryInto;
use core::mem::size_of;

const EVENTFD_COUNTER_MAX: u64 = u64::MAX - 1;

struct EventFdInner {
    counter: u64,
    semaphore: bool,
}

pub struct EventFd {
    inner: UPIntrFreeCell<EventFdInner>,
    status_flags: StatusFlagsCell,
}

impl EventFd {
    fn new(initval: u64, semaphore: bool) -> Self {
        Self {
            inner: unsafe {
                UPIntrFreeCell::new(EventFdInner {
                    counter: initval,
                    semaphore,
                })
            },
            status_flags: StatusFlagsCell::new(OpenFlags::empty()),
        }
    }
}

impl File for EventFd {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read(&self, mut buf: UserBuffer) -> usize {
        if buf.len() < size_of::<u64>() {
            return 0;
        }

        loop {
            let value = {
                let mut inner = self.inner.exclusive_access();
                if inner.counter == 0 {
                    0
                } else if inner.semaphore {
                    inner.counter -= 1;
                    1
                } else {
                    let value = inner.counter;
                    inner.counter = 0;
                    value
                }
            };
            if value != 0 {
                return buf.copy_from_slice(&value.to_ne_bytes());
            }
            if self.status_flags().contains(OpenFlags::NONBLOCK) || current_has_unmasked_signal() {
                return 0;
            }
            suspend_current_and_run_next();
        }
    }

    fn write(&self, buf: UserBuffer) -> usize {
        if buf.len() < size_of::<u64>() {
            return 0;
        }
        let data = buf.to_vec();
        let value = u64::from_ne_bytes(data[..size_of::<u64>()].try_into().unwrap());
        if value == u64::MAX {
            return 0;
        }

        loop {
            let wrote = {
                let mut inner = self.inner.exclusive_access();
                if value <= EVENTFD_COUNTER_MAX.saturating_sub(inner.counter) {
                    inner.counter += value;
                    true
                } else {
                    false
                }
            };
            if wrote {
                return size_of::<u64>();
            }
            if self.status_flags().contains(OpenFlags::NONBLOCK) || current_has_unmasked_signal() {
                return 0;
            }
            suspend_current_and_run_next();
        }
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        let counter = self.inner.exclusive_access().counter;
        let mut ready = PollEvents::empty();
        if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI) && counter > 0 {
            ready |= PollEvents::POLLIN;
        }
        if events.contains(PollEvents::POLLOUT) && counter < EVENTFD_COUNTER_MAX {
            ready |= PollEvents::POLLOUT;
        }
        ready
    }

    fn stat(&self) -> FsResult<FileStat> {
        Ok(FileStat::with_mode(S_IFIFO | 0o600))
    }

    fn check_read(&self, len: usize) -> FsResult {
        if len < size_of::<u64>() {
            Err(FsError::InvalidInput)
        } else {
            Ok(())
        }
    }

    fn check_write(&self, len: usize, _append: bool) -> FsResult {
        if len < size_of::<u64>() {
            Err(FsError::InvalidInput)
        } else {
            Ok(())
        }
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }
}

pub(crate) fn make_eventfd(initval: u64, semaphore: bool) -> Arc<dyn File + Send + Sync> {
    Arc::new(EventFd::new(initval, semaphore))
}
