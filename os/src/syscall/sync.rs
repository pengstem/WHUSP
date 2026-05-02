use crate::task::{block_current_and_run_next, current_task, current_user_token};
use crate::timer::{add_timer, get_time_ms, monotonic_time_nanos, wall_time_nanos};

use super::errno::{SysError, SysResult};
use super::fs::LinuxTimeSpec;
use super::fs::user_ptr::{read_user_value, write_user_value};

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
const CLOCK_MONOTONIC_RAW: i32 = 4;
const CLOCK_REALTIME_COARSE: i32 = 5;
const CLOCK_MONOTONIC_COARSE: i32 = 6;
const CLOCK_BOOTTIME: i32 = 7;
const TIMER_ABSTIME: u32 = 1;
pub(crate) const NSEC_PER_SEC: isize = 1_000_000_000;
pub(crate) const NSEC_PER_MSEC: usize = 1_000_000;

#[derive(Clone, Copy)]
enum ClockBackend {
    Wall,
    Monotonic,
}

#[derive(Clone, Copy)]
enum ClockKind {
    Realtime,
    Monotonic,
    ProcessCpu,
    ThreadCpu,
    MonotonicRaw,
    RealtimeCoarse,
    MonotonicCoarse,
    Boottime,
}

impl ClockKind {
    fn from_raw(clock_id: i32) -> SysResult<Self> {
        match clock_id {
            CLOCK_REALTIME => Ok(Self::Realtime),
            CLOCK_MONOTONIC => Ok(Self::Monotonic),
            CLOCK_PROCESS_CPUTIME_ID => Ok(Self::ProcessCpu),
            CLOCK_THREAD_CPUTIME_ID => Ok(Self::ThreadCpu),
            CLOCK_MONOTONIC_RAW => Ok(Self::MonotonicRaw),
            CLOCK_REALTIME_COARSE => Ok(Self::RealtimeCoarse),
            CLOCK_MONOTONIC_COARSE => Ok(Self::MonotonicCoarse),
            CLOCK_BOOTTIME => Ok(Self::Boottime),
            _ => Err(SysError::EINVAL),
        }
    }

    fn gettime_backend(self) -> SysResult<ClockBackend> {
        match self {
            Self::Realtime | Self::RealtimeCoarse => Ok(ClockBackend::Wall),
            Self::Monotonic | Self::MonotonicRaw | Self::MonotonicCoarse | Self::Boottime => {
                Ok(ClockBackend::Monotonic)
            }
            Self::ProcessCpu | Self::ThreadCpu => {
                // UNFINISHED: CPU clocks require per-process and per-thread CPU
                // accounting with POSIX clock semantics; expose unsupported for now.
                Err(SysError::ENOTSUP)
            }
        }
    }

    fn nanosleep_backend(self) -> SysResult<ClockBackend> {
        match self {
            Self::Realtime => Ok(ClockBackend::Wall),
            Self::Monotonic | Self::Boottime => Ok(ClockBackend::Monotonic),
            Self::ProcessCpu | Self::ThreadCpu => {
                // UNFINISHED: CPU-clock sleeps require waking based on consumed
                // process/thread CPU time rather than wall-clock timer ticks.
                Err(SysError::ENOTSUP)
            }
            Self::MonotonicRaw | Self::RealtimeCoarse | Self::MonotonicCoarse => {
                // CONTEXT: Linux exposes these clocks through clock_gettime but
                // does not support sleeping against them; keep them distinct from
                // unknown clock IDs by returning ENOTSUP.
                Err(SysError::ENOTSUP)
            }
        }
    }
}

fn current_clock_nanos(backend: ClockBackend) -> u64 {
    match backend {
        ClockBackend::Wall => wall_time_nanos(),
        ClockBackend::Monotonic => monotonic_time_nanos(),
    }
}

pub(crate) fn validate_timespec(time: LinuxTimeSpec) -> SysResult<LinuxTimeSpec> {
    if time.tv_sec < 0 || !(0..NSEC_PER_SEC).contains(&time.tv_nsec) {
        return Err(SysError::EINVAL);
    }
    Ok(time)
}

fn timespec_to_nanos(time: LinuxTimeSpec) -> SysResult<u64> {
    let time = validate_timespec(time)?;
    let sec_nanos = (time.tv_sec as u64)
        .checked_mul(NSEC_PER_SEC as u64)
        .ok_or(SysError::EINVAL)?;
    sec_nanos
        .checked_add(time.tv_nsec as u64)
        .ok_or(SysError::EINVAL)
}

fn nanos_to_ms_ceil(nanos: u64) -> SysResult<usize> {
    let nsec_per_msec = NSEC_PER_MSEC as u64;
    let ms = nanos / nsec_per_msec + if nanos % nsec_per_msec == 0 { 0 } else { 1 };
    if ms > usize::MAX as u64 {
        return Err(SysError::EINVAL);
    }
    Ok(ms as usize)
}

pub(crate) fn timespec_to_ms_ceil(time: LinuxTimeSpec) -> SysResult<usize> {
    nanos_to_ms_ceil(timespec_to_nanos(time)?)
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

fn sleep_until_clock(backend: ClockBackend, request: LinuxTimeSpec) -> SysResult {
    let deadline_nanos = timespec_to_nanos(request)?;
    let now_nanos = current_clock_nanos(backend);
    if deadline_nanos <= now_nanos {
        return Ok(0);
    }
    let duration_ms = nanos_to_ms_ceil(deadline_nanos - now_nanos)?;
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

fn nanos_to_timespec(nanos: u64) -> LinuxTimeSpec {
    LinuxTimeSpec {
        tv_sec: (nanos / (NSEC_PER_SEC as u64)) as isize,
        tv_nsec: (nanos % (NSEC_PER_SEC as u64)) as isize,
    }
}

pub fn sys_clock_gettime(clock_id: i32, tp: *mut LinuxTimeSpec) -> SysResult {
    if tp.is_null() {
        return Err(SysError::EFAULT);
    }
    let nanos = current_clock_nanos(ClockKind::from_raw(clock_id)?.gettime_backend()?);
    write_user_value(current_user_token(), tp, &nanos_to_timespec(nanos))?;
    Ok(0)
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
    let backend = ClockKind::from_raw(clock_id)?.nanosleep_backend()?;
    if req.is_null() {
        return Err(SysError::EFAULT);
    }

    let request = validate_timespec(read_user_value(current_user_token(), req)?)?;
    // UNFINISHED: Signal interruption and rem writeback are not implemented yet.
    if flags & TIMER_ABSTIME != 0 {
        sleep_until_clock(backend, request)
    } else {
        sleep_for_ms(timespec_to_ms_ceil(request)?)
    }
}
