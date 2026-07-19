use super::status_flags::StatusFlagsCell;
use super::{
    File, FileStat, FsError, FsResult, OpenFlags, PollEvents, PollWaitQueue, PollWaiter, S_IFIFO,
};
use crate::mm::UserBuffer;
use crate::perf;
use crate::sync::SpinNoIrqLock;
use crate::task::{
    TaskControlBlock, block_current_task_no_schedule_unless_unmasked_signal,
    current_has_unmasked_signal, current_task, schedule, wakeup_task,
};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::convert::TryInto;
use core::mem::size_of;

const EVENTFD_COUNTER_MAX: u64 = u64::MAX - 1;
// CONTEXT: eventfd reserves u64::MAX as an invalid write payload, so the
// counter is bounded by u64::MAX - 1 and POLLOUT means another valid write can fit.

struct EventFdInner {
    counter: u64,
    semaphore: bool,
    read_wait_queue: VecDeque<Arc<TaskControlBlock>>,
    write_wait_queue: VecDeque<Arc<TaskControlBlock>>,
    read_poll_waiters: PollWaitQueue,
    write_poll_waiters: PollWaitQueue,
}

pub struct EventFd {
    inner: SpinNoIrqLock<EventFdInner>,
    status_flags: StatusFlagsCell,
}

impl EventFd {
    fn new(initval: u64, semaphore: bool) -> Self {
        Self {
            inner: SpinNoIrqLock::new(EventFdInner {
                counter: initval,
                semaphore,
                read_wait_queue: VecDeque::new(),
                write_wait_queue: VecDeque::new(),
                read_poll_waiters: PollWaitQueue::new(),
                write_poll_waiters: PollWaitQueue::new(),
            }),
            status_flags: StatusFlagsCell::new(OpenFlags::empty()),
        }
    }
}

impl EventFdInner {
    fn sleep_reader(&mut self) -> Option<*mut crate::task::TaskContext> {
        let (task, task_cx_ptr) = block_current_task_no_schedule_unless_unmasked_signal()?;
        self.read_wait_queue.push_back(task);
        Some(task_cx_ptr)
    }

    fn sleep_writer(&mut self) -> Option<*mut crate::task::TaskContext> {
        let (task, task_cx_ptr) = block_current_task_no_schedule_unless_unmasked_signal()?;
        self.write_wait_queue.push_back(task);
        Some(task_cx_ptr)
    }

    fn wake_reader(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.read_wait_queue.pop_front()
    }

    fn wake_writer(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.write_wait_queue.pop_front()
    }

    fn remove_reader(&mut self, task: &Arc<TaskControlBlock>) {
        remove_waiter(&mut self.read_wait_queue, task);
    }

    fn remove_writer(&mut self, task: &Arc<TaskControlBlock>) {
        remove_waiter(&mut self.write_wait_queue, task);
    }
}

fn remove_waiter(queue: &mut VecDeque<Arc<TaskControlBlock>>, task: &Arc<TaskControlBlock>) {
    if let Some(index) = queue
        .iter()
        .position(|candidate| Arc::ptr_eq(candidate, task))
    {
        queue.remove(index);
    }
}

fn wake_eventfd_reader(task: Option<Arc<TaskControlBlock>>) {
    if let Some(task) = task
        && wakeup_task(task)
    {
        perf::record_eventfd_reader_wakeup();
    }
}

fn wake_eventfd_writer(task: Option<Arc<TaskControlBlock>>) {
    if let Some(task) = task
        && wakeup_task(task)
    {
        perf::record_eventfd_writer_wakeup();
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
        perf::record_eventfd_read_call();

        loop {
            let mut inner = self.inner.lock();
            let (value, writer, poll_writers) = if inner.counter == 0 {
                if self.status_flags().contains(OpenFlags::NONBLOCK) {
                    return 0;
                }
                if current_has_unmasked_signal() {
                    if let Some(task) = current_task() {
                        inner.remove_reader(&task);
                    }
                    return 0;
                }
                perf::record_eventfd_reader_sleep();
                let Some(task_cx_ptr) = inner.sleep_reader() else {
                    return 0;
                };
                drop(inner);
                schedule(task_cx_ptr);
                continue;
            } else if inner.semaphore {
                inner.counter -= 1;
                (1, inner.wake_writer(), inner.write_poll_waiters.drain())
            } else {
                let value = inner.counter;
                inner.counter = 0;
                (value, inner.wake_writer(), inner.write_poll_waiters.drain())
            };
            drop(inner);
            if value != 0 {
                wake_eventfd_writer(writer);
                PollWaiter::wake_all(poll_writers);
                return buf.copy_from_slice(&value.to_ne_bytes());
            }
        }
    }

    fn write(&self, buf: UserBuffer) -> usize {
        if buf.len() < size_of::<u64>() {
            return 0;
        }
        perf::record_eventfd_write_call();
        let data = buf.to_vec();
        let value = u64::from_ne_bytes(data[..size_of::<u64>()].try_into().unwrap());
        if value == u64::MAX {
            return 0;
        }

        loop {
            let mut inner = self.inner.lock();
            let (reader, poll_readers) =
                if value <= EVENTFD_COUNTER_MAX.saturating_sub(inner.counter) {
                    inner.counter += value;
                    if value == 0 {
                        (None, alloc::vec::Vec::new())
                    } else {
                        (inner.wake_reader(), inner.read_poll_waiters.drain())
                    }
                } else {
                    if self.status_flags().contains(OpenFlags::NONBLOCK) {
                        return 0;
                    }
                    if current_has_unmasked_signal() {
                        if let Some(task) = current_task() {
                            inner.remove_writer(&task);
                        }
                        return 0;
                    }
                    perf::record_eventfd_writer_sleep();
                    let Some(task_cx_ptr) = inner.sleep_writer() else {
                        return 0;
                    };
                    drop(inner);
                    schedule(task_cx_ptr);
                    continue;
                };
            drop(inner);
            wake_eventfd_reader(reader);
            PollWaiter::wake_all(poll_readers);
            return size_of::<u64>();
        }
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        self.poll_with_wait(events, None)
    }

    fn poll_with_wait(&self, events: PollEvents, waiter: Option<&Arc<PollWaiter>>) -> PollEvents {
        let mut inner = self.inner.lock();
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
