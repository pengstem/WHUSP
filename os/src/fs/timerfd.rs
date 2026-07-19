use super::status_flags::StatusFlagsCell;
use super::{
    File, FileStat, FsError, FsResult, OpenFlags, PollEvents, PollWaitQueue, PollWaiter, S_IFIFO,
};
use crate::mm::UserBuffer;
use crate::sync::SpinNoIrqLock;
use crate::task::{
    TaskControlBlock, block_current_task_no_schedule_unless_unmasked_signal,
    current_has_unmasked_signal, current_task, schedule, wakeup_task,
};
use crate::timer::get_time_us;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimerFdClock {
    Realtime,
    Monotonic,
}

struct TimerFdInner {
    generation: u64,
    interval_us: usize,
    next_expire_us: Option<usize>,
    expirations: u64,
    read_waiters: VecDeque<Arc<TaskControlBlock>>,
    poll_waiters: PollWaitQueue,
}

pub(crate) struct TimerFd {
    clock: TimerFdClock,
    state: Arc<TimerFdState>,
    status_flags: StatusFlagsCell,
}

pub(crate) struct TimerFdState {
    inner: SpinNoIrqLock<TimerFdInner>,
}

impl TimerFd {
    fn new(clock: TimerFdClock) -> Self {
        Self {
            clock,
            state: Arc::new(TimerFdState {
                inner: SpinNoIrqLock::new(TimerFdInner {
                    generation: 0,
                    interval_us: 0,
                    next_expire_us: None,
                    expirations: 0,
                    read_waiters: VecDeque::new(),
                    poll_waiters: PollWaitQueue::new(),
                }),
            }),
            status_flags: StatusFlagsCell::new(OpenFlags::empty()),
        }
    }

    pub(crate) fn clock(&self) -> TimerFdClock {
        self.clock
    }

    pub(crate) fn set_time(
        &self,
        interval_us: usize,
        next_expire_us: Option<usize>,
    ) -> (usize, usize) {
        let now_us = get_time_us();
        let (old_interval_us, old_remaining_us, readers, poll_waiters, event) = {
            let mut inner = self.state.inner.lock();
            inner.refresh_expirations(now_us);
            let old_interval_us = inner.interval_us;
            let old_remaining_us = inner.remaining_us(now_us);
            inner.generation = inner.generation.wrapping_add(1);
            inner.expirations = 0;
            inner.interval_us = interval_us;
            inner.next_expire_us = next_expire_us;
            let (readers, poll_waiters) = inner.drain_waiters();
            let event = next_expire_us.map(|deadline| (deadline, inner.generation));
            (
                old_interval_us,
                old_remaining_us,
                readers,
                poll_waiters,
                event,
            )
        };
        wake_readers(readers);
        PollWaiter::wake_all(poll_waiters);
        if let Some((deadline_us, generation)) = event {
            crate::timer::add_timerfd_timer(deadline_us, generation, Arc::clone(&self.state));
        }
        (old_interval_us, old_remaining_us)
    }

    pub(crate) fn get_time(&self) -> (usize, usize) {
        let now_us = get_time_us();
        let mut inner = self.state.inner.lock();
        inner.refresh_expirations(now_us);
        (inner.interval_us, inner.remaining_us(now_us))
    }
}

impl TimerFdState {
    pub(crate) fn expire(&self, generation: u64, now_us: usize) -> Option<(usize, u64)> {
        let (readers, poll_waiters, next_event) = {
            let mut inner = self.inner.lock();
            if inner.generation != generation {
                return None;
            }
            inner.refresh_expirations(now_us);
            if inner.expirations == 0 {
                return inner
                    .next_expire_us
                    .map(|deadline| (deadline, inner.generation));
            }
            let (readers, poll_waiters) = inner.drain_waiters();
            let next_event = inner
                .next_expire_us
                .map(|deadline| (deadline, inner.generation));
            (readers, poll_waiters, next_event)
        };
        wake_readers(readers);
        PollWaiter::wake_all(poll_waiters);
        next_event
    }
}

impl TimerFdInner {
    fn refresh_expirations(&mut self, now_us: usize) {
        let Some(next_expire_us) = self.next_expire_us else {
            return;
        };
        if now_us < next_expire_us {
            return;
        }
        if self.interval_us == 0 {
            self.expirations = self.expirations.saturating_add(1);
            self.next_expire_us = None;
            return;
        }

        let elapsed_us = now_us.saturating_sub(next_expire_us);
        let count = 1usize.saturating_add(elapsed_us / self.interval_us);
        self.expirations = self.expirations.saturating_add(count as u64);
        let advance_us = self.interval_us.saturating_mul(count);
        self.next_expire_us = Some(next_expire_us.saturating_add(advance_us));
    }

    fn remaining_us(&self, now_us: usize) -> usize {
        self.next_expire_us
            .map(|deadline| deadline.saturating_sub(now_us))
            .unwrap_or(0)
    }

    fn remove_reader(&mut self, task: &Arc<TaskControlBlock>) {
        if let Some(index) = self
            .read_waiters
            .iter()
            .position(|candidate| Arc::ptr_eq(candidate, task))
        {
            self.read_waiters.remove(index);
        }
    }

    fn drain_waiters(&mut self) -> (Vec<Arc<TaskControlBlock>>, Vec<Arc<PollWaiter>>) {
        (
            self.read_waiters.drain(..).collect(),
            self.poll_waiters.drain(),
        )
    }
}

fn wake_readers(readers: Vec<Arc<TaskControlBlock>>) {
    for task in readers {
        wakeup_task(task);
    }
}

impl File for TimerFd {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        false
    }

    fn read(&self, mut buf: UserBuffer) -> usize {
        if buf.len() < size_of::<u64>() {
            return 0;
        }

        loop {
            let (task, task_cx_ptr) = {
                let now_us = get_time_us();
                let mut inner = self.state.inner.lock();
                inner.refresh_expirations(now_us);
                if inner.expirations > 0 {
                    let expirations = inner.expirations;
                    inner.expirations = 0;
                    return buf.copy_from_slice(&expirations.to_ne_bytes());
                }
                if self.status_flags().contains(OpenFlags::NONBLOCK) {
                    return 0;
                }
                if current_has_unmasked_signal() {
                    if let Some(task) = current_task() {
                        inner.remove_reader(&task);
                    }
                    return 0;
                }

                let Some((task, task_cx_ptr)) =
                    block_current_task_no_schedule_unless_unmasked_signal()
                else {
                    return 0;
                };
                inner.read_waiters.push_back(Arc::clone(&task));
                (task, task_cx_ptr)
            };
            schedule(task_cx_ptr);
            self.state.inner.lock().remove_reader(&task);
        }
    }

    fn write(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        self.poll_with_wait(events, None)
    }

    fn poll_with_wait(&self, events: PollEvents, waiter: Option<&Arc<PollWaiter>>) -> PollEvents {
        let now_us = get_time_us();
        let mut ready = PollEvents::empty();
        {
            let mut inner = self.state.inner.lock();
            inner.refresh_expirations(now_us);
            if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI) && inner.expirations > 0
            {
                ready |= PollEvents::POLLIN;
            } else if let Some(waiter) = waiter
                && events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI)
            {
                inner.poll_waiters.register(waiter);
            }
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

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }
}

pub(crate) fn make_timerfd(clock: TimerFdClock) -> Arc<dyn File + Send + Sync> {
    Arc::new(TimerFd::new(clock))
}
