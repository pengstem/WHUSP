use crate::arch::interrupt;
use crate::fs::{File, FileStat, OpenFlags, PollEvents, PollWaiter, S_IFDIR, S_IFMT};
use crate::mm::UserBuffer;
use crate::perf;
use crate::sync::UPIntrFreeCell;
use crate::task::{
    SignalFlags, block_current_task_no_schedule, current_has_interrupting_signal, current_task,
    current_user_token, linux_sigset_to_flags, schedule,
};
use crate::timer::{add_timer, get_time_us};
use alloc::collections::{BTreeMap, btree_map::Entry};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;

use super::super::errno::{SysError, SysResult};
use super::super::time::timespec_to_nanos;
use super::super::uapi::LinuxTimeSpec;
use super::super::user_ptr::{read_user_value, write_user_value};
use super::fd::{get_fd_entry_by_fd, install_file_fd};

const EPOLL_CTL_ADD: i32 = 1;
const EPOLL_CTL_DEL: i32 = 2;
const EPOLL_CTL_MOD: i32 = 3;

const EPOLLIN: u32 = 0x0001;
const EPOLLPRI: u32 = 0x0002;
const EPOLLOUT: u32 = 0x0004;
const EPOLLERR: u32 = 0x0008;
const EPOLLHUP: u32 = 0x0010;
const EPOLLRDHUP: u32 = 0x2000;
const EPOLLONESHOT: u32 = 1 << 30;
const EPOLLET: u32 = 1 << 31;
const EPOLL_CLOEXEC: u32 = OpenFlags::CLOEXEC.bits();
// CONTEXT: The contest glibc/musl sysroot exposes 64-bit struct epoll_event as
// 16 bytes with u64 data aligned at offset 8. Copy this user ABI instead of the
// packed 12-byte kernel-internal layout used by some Linux headers.
const EPOLL_EVENT_SIZE: usize = 16;
const EPOLL_EVENT_DATA_OFFSET: usize = 8;
const EPOLL_MAX_NEST_DEPTH: usize = 5;
const EPOLL_POLL_BACKOFF_US: usize = 1_000;
const LINUX_RT_SIGSET_SIZE: usize = 8;

#[derive(Clone, Copy, Debug, Default)]
struct LinuxEpollEvent {
    events: u32,
    data: u64,
}

#[derive(Clone)]
struct EpollInterest {
    file: Arc<dyn File + Send + Sync>,
    event: LinuxEpollEvent,
    last_ready: u32,
    disabled: bool,
}

struct EpollScanResult {
    ready_events: Vec<LinuxEpollEvent>,
    waiter_registrations: usize,
    fallback_needed: bool,
}

struct ProcSleepGuard {
    task: Arc<crate::task::TaskControlBlock>,
}

impl ProcSleepGuard {
    fn new() -> SysResult<Self> {
        let task = current_task().ok_or(SysError::ESRCH)?;
        task.inner_exclusive_access().proc_sleeping = true;
        Ok(Self { task })
    }
}

impl Drop for ProcSleepGuard {
    fn drop(&mut self) {
        self.task.inner_exclusive_access().proc_sleeping = false;
    }
}

struct TemporarySignalMask {
    task: Arc<crate::task::TaskControlBlock>,
    old_mask: SignalFlags,
}

impl TemporarySignalMask {
    fn install(mask: SignalFlags) -> SysResult<Self> {
        let task = current_task().ok_or(SysError::ESRCH)?;
        let old_mask = {
            let mut task_inner = task.inner_exclusive_access();
            let old_mask = task_inner.signal_mask;
            task_inner.signal_mask = mask;
            old_mask
        };
        Ok(Self { task, old_mask })
    }
}

impl Drop for TemporarySignalMask {
    fn drop(&mut self) {
        self.task.inner_exclusive_access().signal_mask = self.old_mask;
    }
}

pub struct EpollFile {
    interests: UPIntrFreeCell<BTreeMap<usize, EpollInterest>>,
}

impl EpollFile {
    fn new() -> Self {
        Self {
            interests: unsafe { UPIntrFreeCell::new(BTreeMap::new()) },
        }
    }

    fn collect_ready_sources(&self, maxevents: usize, sources: &[usize]) -> Vec<LinuxEpollEvent> {
        self.interests.exclusive_session(|interests| {
            let mut ready_events = Vec::new();
            let mut visits = 0usize;
            for fd in sources.iter().copied() {
                if ready_events.len() >= maxevents {
                    break;
                }
                let Some(interest) = interests.get_mut(&fd) else {
                    continue;
                };
                if interest.disabled {
                    continue;
                }
                visits += 1;
                let requested = epoll_to_poll_events(interest.event.events);
                let poll_ready = interest.file.poll(requested);
                if let Some(event) = epoll_ready_event_from_poll(interest, poll_ready, true) {
                    ready_events.push(event);
                }
            }
            perf::record_epoll_ready_list(visits, ready_events.len());
            ready_events
        })
    }

    fn scan_ready_with_waiter(
        &self,
        maxevents: usize,
        waiter: Option<&Arc<PollWaiter>>,
    ) -> EpollScanResult {
        self.interests.exclusive_session(|interests| {
            let mut ready_events = Vec::new();
            let mut visits = 0usize;
            let mut fallback_needed = false;
            for (&fd, interest) in interests.iter_mut() {
                visits += 1;
                if ready_events.len() >= maxevents || interest.disabled {
                    continue;
                }
                let requested = epoll_to_poll_events(interest.event.events);
                let registrations_before = waiter.map_or(0, |waiter| waiter.registration_count());
                let poll_ready = match waiter {
                    Some(waiter) => {
                        let _source = waiter.registration_source_guard(fd);
                        interest.file.poll_with_wait(requested, Some(waiter))
                    }
                    None => interest.file.poll(requested),
                };
                let registered =
                    waiter.is_some_and(|waiter| waiter.registration_count() > registrations_before);
                if let Some(event) = epoll_ready_event_from_poll(interest, poll_ready, false) {
                    ready_events.push(event);
                } else if waiter.is_some() && !registered {
                    fallback_needed = true;
                }
            }
            perf::record_epoll_scan(visits, ready_events.len());
            EpollScanResult {
                ready_events,
                waiter_registrations: waiter.map_or(0, |waiter| waiter.registration_count()),
                fallback_needed,
            }
        })
    }
}

fn epoll_ready_event_from_poll(
    interest: &mut EpollInterest,
    poll_ready: PollEvents,
    source_wake: bool,
) -> Option<LinuxEpollEvent> {
    let ready = poll_events_to_epoll(poll_ready) & epoll_readiness_mask(interest.event.events);
    if ready == 0 {
        if interest.event.events & EPOLLET != 0 {
            interest.last_ready = 0;
        }
        return None;
    }
    if interest.event.events & EPOLLET != 0 && ready == interest.last_ready && !source_wake {
        return None;
    }
    interest.last_ready = ready;
    if interest.event.events & EPOLLONESHOT != 0 {
        interest.disabled = true;
    }
    Some(LinuxEpollEvent {
        events: ready,
        data: interest.event.data,
    })
}

impl File for EpollFile {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        false
    }

    fn writable(&self) -> bool {
        false
    }

    fn read(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn write(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn poll(&self, _events: PollEvents) -> PollEvents {
        PollEvents::empty()
    }

    fn stat(&self) -> crate::fs::FsResult<FileStat> {
        Ok(FileStat::with_mode(0))
    }
}

fn is_negative_fd(raw: usize) -> bool {
    (raw as isize) < 0
}

fn epoll_to_poll_events(events: u32) -> PollEvents {
    let mut poll_events = PollEvents::empty();
    if events & EPOLLIN != 0 {
        poll_events |= PollEvents::POLLIN;
    }
    if events & EPOLLPRI != 0 {
        poll_events |= PollEvents::POLLPRI;
    }
    if events & EPOLLOUT != 0 {
        poll_events |= PollEvents::POLLOUT;
    }
    if events & EPOLLRDHUP != 0 {
        poll_events |= PollEvents::POLLRDHUP;
    }
    poll_events
}

fn poll_events_to_epoll(events: PollEvents) -> u32 {
    let mut epoll_events = 0;
    if events.contains(PollEvents::POLLIN) {
        epoll_events |= EPOLLIN;
    }
    if events.contains(PollEvents::POLLPRI) {
        epoll_events |= EPOLLPRI;
    }
    if events.contains(PollEvents::POLLOUT) {
        epoll_events |= EPOLLOUT;
    }
    if events.contains(PollEvents::POLLERR) {
        epoll_events |= EPOLLERR;
    }
    if events.contains(PollEvents::POLLHUP) {
        epoll_events |= EPOLLHUP;
    }
    if events.contains(PollEvents::POLLRDHUP) {
        epoll_events |= EPOLLRDHUP;
    }
    epoll_events
}

fn epoll_readiness_mask(events: u32) -> u32 {
    events & (EPOLLIN | EPOLLPRI | EPOLLOUT | EPOLLRDHUP) | EPOLLERR | EPOLLHUP
}

fn read_epoll_event(token: usize, event: *const u8) -> SysResult<LinuxEpollEvent> {
    if event.is_null() {
        return Err(SysError::EFAULT);
    }
    let bytes = read_user_value(token, event.cast::<[u8; EPOLL_EVENT_SIZE]>())?;
    let mut event_bytes = [0u8; 4];
    event_bytes.copy_from_slice(&bytes[..4]);
    let mut data_bytes = [0u8; 8];
    data_bytes.copy_from_slice(&bytes[EPOLL_EVENT_DATA_OFFSET..EPOLL_EVENT_DATA_OFFSET + 8]);
    Ok(LinuxEpollEvent {
        events: u32::from_ne_bytes(event_bytes),
        data: u64::from_ne_bytes(data_bytes),
    })
}

fn write_epoll_event(
    token: usize,
    events: *mut u8,
    index: usize,
    event: LinuxEpollEvent,
) -> SysResult<()> {
    if events.is_null() {
        return Err(SysError::EFAULT);
    }
    let addr = (events as usize)
        .checked_add(
            index
                .checked_mul(EPOLL_EVENT_SIZE)
                .ok_or(SysError::EFAULT)?,
        )
        .ok_or(SysError::EFAULT)?;
    let mut bytes = [0u8; EPOLL_EVENT_SIZE];
    bytes[..4].copy_from_slice(&event.events.to_ne_bytes());
    bytes[EPOLL_EVENT_DATA_OFFSET..EPOLL_EVENT_DATA_OFFSET + 8]
        .copy_from_slice(&event.data.to_ne_bytes());
    write_user_value(token, addr as *mut [u8; EPOLL_EVENT_SIZE], &bytes)
}

fn read_epoll_sigmask(
    token: usize,
    sigmask: *const u8,
    sigsetsize: usize,
) -> SysResult<Option<SignalFlags>> {
    if sigmask.is_null() {
        return Ok(None);
    }
    if sigsetsize != LINUX_RT_SIGSET_SIZE {
        return Err(SysError::EINVAL);
    }
    if (sigmask as usize) < size_of::<u64>() {
        return Err(SysError::EFAULT);
    }
    let raw = read_user_value(token, sigmask.cast::<u64>())?;
    let mut mask = linux_sigset_to_flags(raw);
    mask.remove(SignalFlags::SIGKILL);
    mask.remove(SignalFlags::SIGSTOP);
    Ok(Some(mask))
}

fn epoll_file_from(file: &Arc<dyn File + Send + Sync>) -> Option<&EpollFile> {
    file.as_any().downcast_ref::<EpollFile>()
}

fn epoll_children(file: &Arc<dyn File + Send + Sync>) -> Vec<Arc<dyn File + Send + Sync>> {
    epoll_file_from(file).map_or_else(Vec::new, |epoll| {
        epoll.interests.exclusive_session(|interests| {
            interests
                .values()
                .map(|entry| Arc::clone(&entry.file))
                .collect()
        })
    })
}

fn epoll_reaches(
    start: &Arc<dyn File + Send + Sync>,
    target: &Arc<dyn File + Send + Sync>,
    depth: usize,
) -> bool {
    if Arc::ptr_eq(start, target) {
        return true;
    }
    if depth > EPOLL_MAX_NEST_DEPTH {
        return false;
    }
    epoll_children(start)
        .iter()
        .any(|child| epoll_reaches(child, target, depth + 1))
}

fn epoll_depth(file: &Arc<dyn File + Send + Sync>, depth: usize) -> usize {
    if depth > EPOLL_MAX_NEST_DEPTH || epoll_file_from(file).is_none() {
        return 0;
    }
    1 + epoll_children(file)
        .iter()
        .map(|child| epoll_depth(child, depth + 1))
        .max()
        .unwrap_or(0)
}

fn validate_epoll_target(
    epoll_file: &Arc<dyn File + Send + Sync>,
    target_file: &Arc<dyn File + Send + Sync>,
) -> SysResult<()> {
    if let Some(stat) = target_file.stat().ok()
        && stat.mode & S_IFMT == S_IFDIR
    {
        return Err(SysError::EPERM);
    }
    if epoll_file_from(target_file).is_some() {
        if epoll_reaches(target_file, epoll_file, 0) {
            return Err(SysError::ELOOP);
        }
        if epoll_depth(target_file, 0) >= EPOLL_MAX_NEST_DEPTH {
            return Err(SysError::EINVAL);
        }
    }
    Ok(())
}

fn get_epoll_file(fd: usize) -> SysResult<Arc<dyn File + Send + Sync>> {
    let file = get_fd_entry_by_fd(fd)?.file();
    if epoll_file_from(&file).is_none() {
        return Err(SysError::EINVAL);
    }
    Ok(file)
}

pub fn sys_epoll_create1(flags: u32) -> SysResult {
    if flags & !EPOLL_CLOEXEC != 0 {
        return Err(SysError::EINVAL);
    }
    let open_flags = OpenFlags::from_bits_truncate(flags & EPOLL_CLOEXEC);
    let file = Arc::new(EpollFile::new());
    install_file_fd(file, open_flags, None)
}

pub fn sys_epoll_ctl(epfd_raw: usize, op: i32, fd_raw: usize, event: *const u8) -> SysResult {
    if is_negative_fd(epfd_raw) {
        return Err(SysError::EBADF);
    }
    let epoll_file = get_epoll_file(epfd_raw)?;
    if !matches!(op, EPOLL_CTL_ADD | EPOLL_CTL_DEL | EPOLL_CTL_MOD) {
        return Err(SysError::EINVAL);
    }
    if is_negative_fd(fd_raw) {
        return Err(SysError::EBADF);
    }
    if fd_raw == epfd_raw {
        return Err(SysError::EINVAL);
    }

    let target_file = get_fd_entry_by_fd(fd_raw)?.file();
    let epoll = epoll_file_from(&epoll_file).ok_or(SysError::EINVAL)?;
    let token = current_user_token();

    match op {
        EPOLL_CTL_ADD => {
            let event = read_epoll_event(token, event)?;
            validate_epoll_target(&epoll_file, &target_file)?;
            epoll.interests.exclusive_session(|interests| {
                let result = match interests.entry(fd_raw) {
                    Entry::Occupied(_) => Err(SysError::EEXIST),
                    Entry::Vacant(slot) => {
                        slot.insert(EpollInterest {
                            file: target_file,
                            event,
                            last_ready: 0,
                            disabled: false,
                        });
                        Ok(0)
                    }
                };
                perf::record_epoll_ctl(0, 1, interests.len());
                result
            })
        }
        EPOLL_CTL_DEL => epoll.interests.exclusive_session(|interests| {
            let result = interests.remove(&fd_raw).map(|_| 0).ok_or(SysError::ENOENT);
            perf::record_epoll_ctl(0, 1, interests.len());
            result
        }),
        EPOLL_CTL_MOD => {
            let event = read_epoll_event(token, event)?;
            epoll.interests.exclusive_session(|interests| {
                let result = match interests.get_mut(&fd_raw) {
                    Some(interest) => {
                        interest.event = event;
                        interest.last_ready = 0;
                        interest.disabled = false;
                        Ok(0)
                    }
                    None => Err(SysError::ENOENT),
                };
                perf::record_epoll_ctl(0, 1, interests.len());
                result
            })
        }
        _ => Err(SysError::EINVAL),
    }
}

fn nanos_to_us_ceil(nanos: u64) -> SysResult<usize> {
    let us = nanos / 1_000 + if nanos % 1_000 == 0 { 0 } else { 1 };
    if us > usize::MAX as u64 {
        return Err(SysError::EINVAL);
    }
    Ok(us as usize)
}

fn deadline_from_timeout_us(timeout_us: usize) -> SysResult<Option<usize>> {
    Ok(Some(
        get_time_us()
            .checked_add(timeout_us)
            .ok_or(SysError::EINVAL)?,
    ))
}

fn deadline_from_timeout_ms(timeout_ms: i32) -> SysResult<Option<usize>> {
    if timeout_ms == -1 {
        return Ok(None);
    }
    if timeout_ms < -1 {
        return Err(SysError::EINVAL);
    }
    let timeout_us = (timeout_ms as usize)
        .checked_mul(1_000)
        .ok_or(SysError::EINVAL)?;
    deadline_from_timeout_us(timeout_us)
}

fn deadline_from_timeout_timespec(
    token: usize,
    timeout: *const LinuxTimeSpec,
) -> SysResult<Option<usize>> {
    if timeout.is_null() {
        return Ok(None);
    }
    let timeout = read_user_value(token, timeout)?;
    deadline_from_timeout_us(nanos_to_us_ceil(timespec_to_nanos(timeout)?)?)
}

fn sleep_until_next_epoll_probe(deadline_us: Option<usize>) -> SysResult {
    let now_us = get_time_us();
    if deadline_us.is_some_and(|deadline_us| now_us >= deadline_us) {
        return Ok(0);
    }
    if current_has_interrupting_signal() {
        return Err(SysError::EINTR);
    }

    let target_us = now_us.saturating_add(EPOLL_POLL_BACKOFF_US);
    let target_us = deadline_us.map_or(target_us, |deadline_us| deadline_us.min(target_us));
    let sleep_us = target_us.saturating_sub(now_us);
    let expire_ms = target_us.div_ceil(1_000);
    current_task().ok_or(SysError::ESRCH)?;
    let (task, task_cx_ptr) = block_current_task_no_schedule();
    add_timer(expire_ms, task);
    perf::record_epoll_backoff_sleep(sleep_us);
    schedule(task_cx_ptr);
    Ok(0)
}

fn sleep_until_epoll_event(waiter: &Arc<PollWaiter>, deadline_us: Option<usize>) -> SysResult {
    if waiter.was_triggered() {
        return Ok(0);
    }
    if deadline_us.is_some_and(|deadline_us| get_time_us() >= deadline_us) {
        return Ok(0);
    }
    if current_has_interrupting_signal() {
        return Err(SysError::EINTR);
    }

    let interrupts_enabled = interrupt::supervisor_interrupt_enabled();
    interrupt::disable_supervisor_interrupt();
    if waiter.was_triggered() {
        if interrupts_enabled {
            interrupt::enable_supervisor_interrupt();
        }
        return Ok(0);
    }
    let (task, task_cx_ptr) = block_current_task_no_schedule();
    debug_assert!(waiter.task_matches(&task));
    if let Some(deadline_us) = deadline_us {
        add_timer(deadline_us.div_ceil(1_000), task);
    }
    if interrupts_enabled {
        interrupt::enable_supervisor_interrupt();
    }
    perf::record_epoll_waiter_sleep();
    schedule(task_cx_ptr);
    Ok(0)
}

fn sys_epoll_wait_until(
    epfd_raw: usize,
    events: *mut u8,
    maxevents: i32,
    deadline_us: Option<usize>,
) -> SysResult {
    if is_negative_fd(epfd_raw) {
        return Err(SysError::EBADF);
    }
    if maxevents <= 0 {
        return Err(SysError::EINVAL);
    }
    if events.is_null() {
        return Err(SysError::EFAULT);
    }
    let epoll_file = get_epoll_file(epfd_raw)?;
    let epoll = epoll_file_from(&epoll_file).ok_or(SysError::EINVAL)?;
    let token = current_user_token();
    let maxevents = maxevents as usize;
    // CONTEXT: This kernel polls readiness cooperatively instead of parking
    // epoll waiters on every monitored file. Keep the task runnable while
    // exposing Linux's sleeping state through /proc for LTP synchronizers.
    let _proc_sleep = ProcSleepGuard::new()?;
    let mut ready_sources = Vec::new();

    loop {
        if !ready_sources.is_empty() {
            let ready_events = epoll.collect_ready_sources(maxevents, &ready_sources);
            ready_sources.clear();
            if !ready_events.is_empty() {
                for (index, event) in ready_events.iter().enumerate() {
                    write_epoll_event(token, events, index, *event)?;
                }
                return Ok(ready_events.len() as isize);
            }
        }

        let waiter = PollWaiter::new(current_task().ok_or(SysError::ESRCH)?);
        let scan = epoll.scan_ready_with_waiter(maxevents, Some(&waiter));
        if scan.waiter_registrations > 0 {
            perf::record_epoll_waiter_registrations(scan.waiter_registrations);
        }
        if !scan.ready_events.is_empty() {
            for (index, event) in scan.ready_events.iter().enumerate() {
                write_epoll_event(token, events, index, *event)?;
            }
            return Ok(scan.ready_events.len() as isize);
        }
        if deadline_us.is_some_and(|deadline_us| get_time_us() >= deadline_us) {
            return Ok(0);
        }
        if current_has_interrupting_signal() {
            return Err(SysError::EINTR);
        }
        if scan.waiter_registrations > 0 && !scan.fallback_needed {
            sleep_until_epoll_event(&waiter, deadline_us)?;
            ready_sources = waiter.drain_ready_sources();
        } else {
            sleep_until_next_epoll_probe(deadline_us)?;
            if scan.waiter_registrations > 0 {
                ready_sources = waiter.drain_ready_sources();
            }
        }
    }
}

pub fn sys_epoll_pwait(
    epfd: usize,
    events: *mut u8,
    maxevents: i32,
    timeout_ms: i32,
    sigmask: *const u8,
    sigsetsize: usize,
) -> SysResult {
    // UNFINISHED: Linux installs the supplied signal mask atomically with
    // entering the wait. This kernel applies it around the cooperative polling
    // loop, which is sufficient for the current single-hart LTP cases.
    let token = current_user_token();
    let sigmask = read_epoll_sigmask(token, sigmask, sigsetsize)?;
    let _mask_guard = match sigmask {
        Some(mask) => Some(TemporarySignalMask::install(mask)?),
        None => None,
    };
    sys_epoll_wait_until(
        epfd,
        events,
        maxevents,
        deadline_from_timeout_ms(timeout_ms)?,
    )
}

pub fn sys_epoll_pwait2(
    epfd: usize,
    events: *mut u8,
    maxevents: i32,
    timeout: *const LinuxTimeSpec,
    sigmask: *const u8,
    sigsetsize: usize,
) -> SysResult {
    // UNFINISHED: See sys_epoll_pwait(); the mask is applied around the
    // cooperative wait rather than as a fully atomic sleep transition.
    let token = current_user_token();
    let sigmask = read_epoll_sigmask(token, sigmask, sigsetsize)?;
    let _mask_guard = match sigmask {
        Some(mask) => Some(TemporarySignalMask::install(mask)?),
        None => None,
    };
    sys_epoll_wait_until(
        epfd,
        events,
        maxevents,
        deadline_from_timeout_timespec(token, timeout)?,
    )
}
