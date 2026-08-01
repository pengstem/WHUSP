use crate::fs::{File, OpenFlags, TimerFd, TimerFdClock, make_timerfd};
use crate::task::{current_process, current_user_token};
use crate::timer::get_time_us;
use alloc::sync::Arc;

use super::super::time::{
    ClockBackend, LinuxITimerSpec, current_clock_nanos, nanos_to_us_ceil, timespec_to_nanos,
    timespec_to_us_ceil, us_to_timespec,
};
use super::super::uapi::LinuxTimeSpec;
use super::super::user_ptr::{read_user_value, write_user_value};
use super::fd::get_file_by_fd;
use super::fd::install_file_fd;
use crate::uapi::errno::{Errno, KResult};

const FD_NONBLOCK: u32 = OpenFlags::NONBLOCK.bits();
const FD_CLOEXEC: u32 = OpenFlags::CLOEXEC.bits();
const TIMERFD_VALID_FLAGS: u32 = FD_NONBLOCK | FD_CLOEXEC;
const TFD_TIMER_ABSTIME: u32 = 1;
const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_BOOTTIME: i32 = 7;
const CLOCK_REALTIME_ALARM: i32 = 8;
const CLOCK_BOOTTIME_ALARM: i32 = 9;

fn timerfd_open_flags(flags: u32) -> KResult<OpenFlags> {
    if flags & !TIMERFD_VALID_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }

    let mut open_flags = OpenFlags::RDONLY;
    if flags & FD_NONBLOCK != 0 {
        open_flags |= OpenFlags::NONBLOCK;
    }
    if flags & FD_CLOEXEC != 0 {
        open_flags |= OpenFlags::CLOEXEC;
    }
    Ok(open_flags)
}

pub fn sys_timerfd_create(clockid: i32, flags: u32) -> KResult {
    let clock = timerfd_clock_from_id(clockid)?;
    let open_flags = timerfd_open_flags(flags)?;
    install_file_fd(make_timerfd(clock), open_flags, None)
}

fn timerfd_clock_from_id(clockid: i32) -> KResult<TimerFdClock> {
    match clockid {
        CLOCK_REALTIME => Ok(TimerFdClock::Realtime),
        CLOCK_MONOTONIC | CLOCK_BOOTTIME => Ok(TimerFdClock::Monotonic),
        CLOCK_REALTIME_ALARM => {
            if current_process().credentials().euid == 0 {
                Ok(TimerFdClock::Realtime)
            } else {
                Err(Errno::EPERM)
            }
        }
        CLOCK_BOOTTIME_ALARM => {
            if current_process().credentials().euid == 0 {
                Ok(TimerFdClock::Monotonic)
            } else {
                Err(Errno::EPERM)
            }
        }
        _ => Err(Errno::EINVAL),
    }
}

fn timerfd_backend(clock: TimerFdClock) -> ClockBackend {
    match clock {
        TimerFdClock::Realtime => ClockBackend::Wall,
        TimerFdClock::Monotonic => ClockBackend::Monotonic,
    }
}

fn itimerspec_from_us(interval_us: usize, value_us: usize) -> LinuxITimerSpec {
    LinuxITimerSpec {
        it_interval: us_to_timespec(interval_us),
        it_value: us_to_timespec(value_us),
    }
}

fn timerfd_next_expire_us(
    clock: TimerFdClock,
    flags: u32,
    value: LinuxTimeSpec,
) -> KResult<Option<usize>> {
    let value_nanos = timespec_to_nanos(value)?;
    if value_nanos == 0 {
        return Ok(None);
    }
    let remaining_us = if flags & TFD_TIMER_ABSTIME != 0 {
        let now_nanos = current_clock_nanos(timerfd_backend(clock));
        nanos_to_us_ceil(value_nanos.saturating_sub(now_nanos))?
    } else {
        nanos_to_us_ceil(value_nanos)?
    };
    Ok(Some(
        get_time_us()
            .checked_add(remaining_us)
            .ok_or(Errno::EINVAL)?,
    ))
}

fn timerfd_from_file(file: &Arc<dyn File + Send + Sync>) -> KResult<&TimerFd> {
    file.as_any().downcast_ref::<TimerFd>().ok_or(Errno::EINVAL)
}

pub fn sys_timerfd_settime(
    fd: i32,
    flags: u32,
    new_value: *const LinuxITimerSpec,
    old_value: *mut LinuxITimerSpec,
) -> KResult {
    if flags & !TFD_TIMER_ABSTIME != 0 {
        return Err(Errno::EINVAL);
    }
    if new_value.is_null() {
        return Err(Errno::EFAULT);
    }
    let request = read_user_value(current_user_token(), new_value)?;
    let interval_us = timespec_to_us_ceil(request.it_interval)?;
    let file = get_file_by_fd(fd.try_into().map_err(|_| Errno::EBADF)?)?;
    let timerfd = timerfd_from_file(&file)?;
    let next_expire_us = timerfd_next_expire_us(timerfd.clock(), flags, request.it_value)?;
    let (old_interval_us, old_remaining_us) = timerfd.set_time(interval_us, next_expire_us);
    if !old_value.is_null() {
        let old = itimerspec_from_us(old_interval_us, old_remaining_us);
        write_user_value(current_user_token(), old_value, &old)?;
    }
    Ok(0)
}

pub fn sys_timerfd_gettime(fd: i32, curr_value: *mut LinuxITimerSpec) -> KResult {
    if curr_value.is_null() {
        return Err(Errno::EFAULT);
    }
    let file = get_file_by_fd(fd.try_into().map_err(|_| Errno::EBADF)?)?;
    let timerfd = timerfd_from_file(&file)?;
    let (interval_us, remaining_us) = timerfd.get_time();
    let current = itimerspec_from_us(interval_us, remaining_us);
    write_user_value(current_user_token(), curr_value, &current)?;
    Ok(0)
}
