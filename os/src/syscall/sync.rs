use crate::task::{block_current_and_run_next, current_task, current_user_token};
use crate::timer::{add_timer, get_time_ms};

use super::errno::{SysError, SysResult};
use super::fs::LinuxTimeSpec;
use super::fs::user_ptr::read_user_value;

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
const TIMER_ABSTIME: u32 = 1;
pub(crate) const NSEC_PER_SEC: isize = 1_000_000_000;
pub(crate) const NSEC_PER_MSEC: usize = 1_000_000;

pub(crate) fn validate_timespec(time: LinuxTimeSpec) -> SysResult<LinuxTimeSpec> {
    if time.tv_sec < 0 || !(0..NSEC_PER_SEC).contains(&time.tv_nsec) {
        return Err(SysError::EINVAL);
    }
    Ok(time)
}

pub(crate) fn timespec_to_ms_ceil(time: LinuxTimeSpec) -> SysResult<usize> {
    let sec_ms = (time.tv_sec as usize)
        .checked_mul(1000)
        .ok_or(SysError::EINVAL)?;
    let nsec_ms = if time.tv_nsec == 0 {
        0
    } else {
        ((time.tv_nsec as usize) + NSEC_PER_MSEC - 1) / NSEC_PER_MSEC
    };
    sec_ms.checked_add(nsec_ms).ok_or(SysError::EINVAL)
}

fn sleep_until_ms(expire_ms: usize) {
    if get_time_ms() >= expire_ms {
        return;
    }
    let task = current_task().unwrap();
    add_timer(expire_ms, task);
    block_current_and_run_next();
}

fn sleep_for_ms(duration_ms: usize) -> SysResult {
    if duration_ms == 0 {
        return Ok(0);
    }
    let expire_ms = get_time_ms()
        .checked_add(duration_ms)
        .ok_or(SysError::EINVAL)?;
    sleep_until_ms(expire_ms);
    Ok(0)
}

pub fn sys_nanosleep(req: *const LinuxTimeSpec, _rem: *mut LinuxTimeSpec) -> SysResult {
    if req.is_null() {
        return Err(SysError::EFAULT);
    }
    let request = validate_timespec(read_user_value(current_user_token(), req)?)?;
    // UNFINISHED: Linux nanosleep returns EINTR and writes the remaining time
    // to rem when interrupted by a handled signal. This kernel currently lacks
    // non-fatal signal delivery and signal-driven wakeups for sleeping tasks.
    sleep_for_ms(timespec_to_ms_ceil(request)?)
}

pub fn sys_clock_nanosleep(
    clock_id: i32,
    flags: u32,
    req: *const LinuxTimeSpec,
    _rem: *mut LinuxTimeSpec,
) -> SysResult {
    if flags & !TIMER_ABSTIME != 0 {
        return Err(SysError::EINVAL);
    }
    if matches!(clock_id, CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID) {
        return Err(SysError::ENOTSUP);
    }
    if !matches!(clock_id, CLOCK_REALTIME | CLOCK_MONOTONIC) {
        return Err(SysError::EINVAL);
    }
    if req.is_null() {
        return Err(SysError::EFAULT);
    }

    let request = validate_timespec(read_user_value(current_user_token(), req)?)?;
    let request_ms = timespec_to_ms_ceil(request)?;
    // UNFINISHED: CLOCK_REALTIME is backed by the same monotonic machine timer
    // as gettimeofday because this kernel has no RTC-backed wall-clock state.
    // Signal interruption and rem writeback are also not implemented yet.
    if flags & TIMER_ABSTIME != 0 {
        sleep_until_ms(request_ms);
        Ok(0)
    } else {
        sleep_for_ms(request_ms)
    }
}
