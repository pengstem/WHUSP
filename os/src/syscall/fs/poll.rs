use crate::fs::PollEvents;
use crate::task::{current_user_token, suspend_current_and_run_next};
use crate::timer::get_time_ms;
use alloc::vec::Vec;
use core::mem::size_of;

use super::super::errno::{SysError, SysResult};
use super::super::sync::{timespec_to_ms_ceil, validate_timespec};
use super::fd::get_file_by_fd;
use super::uapi::{LinuxPollFd, LinuxTimeSpec, PPOLL_MAX_NFDS};
use super::user_ptr::{read_user_value, write_user_value};

fn read_user_pollfds(
    token: usize,
    fds: *const LinuxPollFd,
    nfds: usize,
) -> SysResult<Vec<LinuxPollFd>> {
    if nfds == 0 {
        return Ok(Vec::new());
    }
    if fds.is_null() {
        return Err(SysError::EFAULT);
    }
    if nfds > PPOLL_MAX_NFDS {
        return Err(SysError::EINVAL);
    }
    nfds.checked_mul(size_of::<LinuxPollFd>())
        .ok_or(SysError::EINVAL)?;

    let mut pollfds = Vec::with_capacity(nfds);
    for index in 0..nfds {
        let entry_addr = (fds as usize)
            .checked_add(
                index
                    .checked_mul(size_of::<LinuxPollFd>())
                    .ok_or(SysError::EFAULT)?,
            )
            .ok_or(SysError::EFAULT)?;
        pollfds.push(read_user_value(token, entry_addr as *const LinuxPollFd)?);
    }
    Ok(pollfds)
}

fn write_user_pollfds(token: usize, fds: *mut LinuxPollFd, pollfds: &[LinuxPollFd]) -> SysResult {
    for (index, pollfd) in pollfds.iter().enumerate() {
        let entry_addr = (fds as usize)
            .checked_add(
                index
                    .checked_mul(size_of::<LinuxPollFd>())
                    .ok_or(SysError::EFAULT)?,
            )
            .ok_or(SysError::EFAULT)?;
        write_user_value(token, entry_addr as *mut LinuxPollFd, pollfd)?;
    }
    Ok(0)
}

fn poll_events_from_user(events: i16) -> PollEvents {
    PollEvents::from_bits_truncate(events as u16)
}

fn poll_events_to_user(events: PollEvents) -> i16 {
    events.bits() as i16
}

fn timeout_deadline_ms(token: usize, timeout: *const LinuxTimeSpec) -> SysResult<Option<usize>> {
    if timeout.is_null() {
        return Ok(None);
    }
    let timeout = validate_timespec(read_user_value(token, timeout)?)?;
    let timeout_ms = timespec_to_ms_ceil(timeout)?;
    let deadline_ms = get_time_ms()
        .checked_add(timeout_ms)
        .ok_or(SysError::EINVAL)?;
    Ok(Some(deadline_ms))
}

fn scan_pollfds(pollfds: &mut [LinuxPollFd]) -> usize {
    let mut ready = 0usize;
    for pollfd in pollfds.iter_mut() {
        pollfd.revents = 0;
        if pollfd.fd < 0 {
            continue;
        }

        let events = poll_events_from_user(pollfd.events);
        match get_file_by_fd(pollfd.fd as usize) {
            Ok(file) => {
                let revents = file.poll(events);
                pollfd.revents = poll_events_to_user(revents);
                if !revents.is_empty() {
                    ready += 1;
                }
            }
            Err(SysError::EBADF) => {
                pollfd.revents = poll_events_to_user(PollEvents::POLLNVAL);
                ready += 1;
            }
            Err(_) => {
                pollfd.revents = poll_events_to_user(PollEvents::POLLERR);
                ready += 1;
            }
        }
    }
    ready
}

pub fn sys_ppoll(
    fds: *mut LinuxPollFd,
    nfds: usize,
    timeout: *const LinuxTimeSpec,
    sigmask: *const u8,
    _sigsetsize: usize,
) -> SysResult {
    // UNFINISHED: ppoll currently ignores per-call signal-mask installation and EINTR wakeups.
    if !sigmask.is_null() {
        return Err(SysError::ENOSYS);
    }

    let token = current_user_token();
    let mut pollfds = read_user_pollfds(token, fds.cast_const(), nfds)?;
    let deadline_ms = timeout_deadline_ms(token, timeout)?;

    loop {
        let ready = scan_pollfds(&mut pollfds);
        if ready > 0 {
            write_user_pollfds(token, fds, &pollfds)?;
            return Ok(ready as isize);
        }
        if let Some(deadline_ms) = deadline_ms {
            if get_time_ms() >= deadline_ms {
                write_user_pollfds(token, fds, &pollfds)?;
                return Ok(0);
            }
        }
        suspend_current_and_run_next();
    }
}
