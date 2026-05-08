use crate::fs::{File, FileStat, OpenFlags, PollEvents, S_IFDIR, S_IFMT};
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use crate::task::{
    FdTableEntry, current_has_interrupting_signal, current_process, current_user_token,
    suspend_current_and_run_next,
};
use crate::timer::get_time_us;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::super::errno::{SysError, SysResult};
use super::super::time::timespec_to_nanos;
use super::super::uapi::LinuxTimeSpec;
use super::super::user_ptr::{read_user_value, write_user_value};
use super::fd::get_fd_entry_by_fd;

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

#[derive(Clone, Copy, Debug, Default)]
struct LinuxEpollEvent {
    events: u32,
    data: u64,
}

#[derive(Clone)]
struct EpollInterest {
    fd: usize,
    file: Arc<dyn File + Send + Sync>,
    event: LinuxEpollEvent,
    last_ready: u32,
    disabled: bool,
}

pub struct EpollFile {
    interests: UPIntrFreeCell<Vec<EpollInterest>>,
}

impl EpollFile {
    fn new() -> Self {
        Self {
            interests: unsafe { UPIntrFreeCell::new(Vec::new()) },
        }
    }

    fn scan_ready(&self, maxevents: usize) -> Vec<LinuxEpollEvent> {
        self.interests.exclusive_session(|interests| {
            let mut ready_events = Vec::new();
            for interest in interests.iter_mut() {
                if ready_events.len() >= maxevents || interest.disabled {
                    continue;
                }
                let requested = epoll_to_poll_events(interest.event.events);
                let ready = poll_events_to_epoll(interest.file.poll(requested))
                    & epoll_readiness_mask(interest.event.events);
                if ready == 0 {
                    if interest.event.events & EPOLLET != 0 {
                        interest.last_ready = 0;
                    }
                    continue;
                }
                if interest.event.events & EPOLLET != 0 && ready == interest.last_ready {
                    continue;
                }
                interest.last_ready = ready;
                if interest.event.events & EPOLLONESHOT != 0 {
                    interest.disabled = true;
                }
                ready_events.push(LinuxEpollEvent {
                    events: ready,
                    data: interest.event.data,
                });
            }
            ready_events
        })
    }
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

fn epoll_file_from(file: &Arc<dyn File + Send + Sync>) -> Option<&EpollFile> {
    file.as_any().downcast_ref::<EpollFile>()
}

fn epoll_children(file: &Arc<dyn File + Send + Sync>) -> Vec<Arc<dyn File + Send + Sync>> {
    epoll_file_from(file).map_or_else(Vec::new, |epoll| {
        epoll.interests.exclusive_session(|interests| {
            interests
                .iter()
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
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let fd = inner.alloc_fd_from(0).ok_or(SysError::EMFILE)?;
    inner.fd_table[fd] = Some(FdTableEntry::from_file(file, open_flags));
    Ok(fd as isize)
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
                if interests.iter().any(|entry| entry.fd == fd_raw) {
                    return Err(SysError::EEXIST);
                }
                interests.push(EpollInterest {
                    fd: fd_raw,
                    file: target_file,
                    event,
                    last_ready: 0,
                    disabled: false,
                });
                Ok(0)
            })
        }
        EPOLL_CTL_DEL => epoll.interests.exclusive_session(|interests| {
            let Some(index) = interests.iter().position(|entry| entry.fd == fd_raw) else {
                return Err(SysError::ENOENT);
            };
            interests.remove(index);
            Ok(0)
        }),
        EPOLL_CTL_MOD => {
            let event = read_epoll_event(token, event)?;
            epoll.interests.exclusive_session(|interests| {
                let Some(interest) = interests.iter_mut().find(|entry| entry.fd == fd_raw) else {
                    return Err(SysError::ENOENT);
                };
                interest.event = event;
                interest.last_ready = 0;
                interest.disabled = false;
                Ok(0)
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

    loop {
        let ready = epoll.scan_ready(maxevents);
        if !ready.is_empty() {
            for (index, event) in ready.iter().enumerate() {
                write_epoll_event(token, events, index, *event)?;
            }
            return Ok(ready.len() as isize);
        }
        if deadline_us.is_some_and(|deadline_us| get_time_us() >= deadline_us) {
            return Ok(0);
        }
        if current_has_interrupting_signal() {
            return Err(SysError::EINTR);
        }
        suspend_current_and_run_next();
    }
}

pub fn sys_epoll_pwait(
    epfd: usize,
    events: *mut u8,
    maxevents: i32,
    timeout_ms: i32,
    sigmask: *const u8,
    _sigsetsize: usize,
) -> SysResult {
    // UNFINISHED: Linux epoll_pwait() temporarily installs the supplied signal
    // mask while sleeping. This first implementation accepts the argument as a
    // no-op so libc epoll_wait() and readiness-oriented LTP cases can run.
    let _ = sigmask;
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
    _sigsetsize: usize,
) -> SysResult {
    // UNFINISHED: See sys_epoll_pwait(); per-call signal-mask installation is
    // not implemented yet.
    let _ = sigmask;
    let token = current_user_token();
    sys_epoll_wait_until(
        epfd,
        events,
        maxevents,
        deadline_from_timeout_timespec(token, timeout)?,
    )
}
