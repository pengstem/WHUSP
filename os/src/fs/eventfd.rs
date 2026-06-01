use super::status_flags::StatusFlagsCell;
use super::{
    File, FileStat, FsError, FsResult, OpenFlags, PollEvents, PollWaitQueue, PollWaiter, S_IFIFO,
};
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use crate::task::{current_has_unmasked_signal, suspend_current_and_run_next};
use alloc::sync::Arc;
use core::convert::TryInto;
use core::mem::size_of;

const EVENTFD_COUNTER_MAX: u64 = u64::MAX - 1;
// CONTEXT: eventfd reserves u64::MAX as an invalid write payload, so the
// counter is bounded by u64::MAX - 1 and POLLOUT means another valid write can fit.

struct EventFdInner {
    counter: u64,
    semaphore: bool,
    read_poll_waiters: PollWaitQueue,
    write_poll_waiters: PollWaitQueue,
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
                    read_poll_waiters: PollWaitQueue::new(),
                    write_poll_waiters: PollWaitQueue::new(),
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
            let (value, poll_writers) = {
                let mut inner = self.inner.exclusive_access();
                if inner.counter == 0 {
                    (0, alloc::vec::Vec::new())
                } else if inner.semaphore {
                    inner.counter -= 1;
                    (1, inner.write_poll_waiters.drain())
                } else {
                    let value = inner.counter;
                    inner.counter = 0;
                    (value, inner.write_poll_waiters.drain())
                }
            };
            if value != 0 {
                PollWaiter::wake_all(poll_writers);
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
            let (wrote, poll_readers) = {
                let mut inner = self.inner.exclusive_access();
                if value <= EVENTFD_COUNTER_MAX.saturating_sub(inner.counter) {
                    inner.counter += value;
                    let poll_readers = if value == 0 {
                        alloc::vec::Vec::new()
                    } else {
                        inner.read_poll_waiters.drain()
                    };
                    (true, poll_readers)
                } else {
                    (false, alloc::vec::Vec::new())
                }
            };
            if wrote {
                PollWaiter::wake_all(poll_readers);
                return size_of::<u64>();
            }
            if self.status_flags().contains(OpenFlags::NONBLOCK) || current_has_unmasked_signal() {
                return 0;
            }
            suspend_current_and_run_next();
        }
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        self.poll_with_wait(events, None)
    }

    fn poll_with_wait(&self, events: PollEvents, waiter: Option<&Arc<PollWaiter>>) -> PollEvents {
        let mut inner = self.inner.exclusive_access();
        if let Some(waiter) = waiter {
            if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI) {
                inner.read_poll_waiters.register(waiter);
            }
            if events.contains(PollEvents::POLLOUT) {
                inner.write_poll_waiters.register(waiter);
            }
        }
        let counter = inner.counter;
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

    fn is_eventfd(&self) -> bool {
        true
    }
}

pub(crate) fn make_eventfd(initval: u64, semaphore: bool) -> Arc<dyn File + Send + Sync> {
    Arc::new(EventFd::new(initval, semaphore))
}
