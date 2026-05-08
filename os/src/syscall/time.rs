use crate::task::{
    block_current_and_run_next, current_has_deliverable_signal, current_process, current_task,
    current_user_token, ProcessCpuTimesSnapshot,
};
use crate::timer::{
    add_real_timer, add_timer, get_time_clock_ticks, get_time_ms, get_time_us,
    monotonic_time_nanos, us_to_clock_ticks, wall_time_nanos,
};

use super::errno::{SysError, SysResult};
use super::uapi::LinuxTimeSpec;
use super::user_ptr::{read_user_value, write_user_value};

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
const CLOCK_MONOTONIC_RAW: i32 = 4;
const CLOCK_REALTIME_COARSE: i32 = 5;
const CLOCK_MONOTONIC_COARSE: i32 = 6;
const CLOCK_BOOTTIME: i32 = 7;
const CLOCK_REALTIME_ALARM: i32 = 8;
const CLOCK_BOOTTIME_ALARM: i32 = 9;
const TIMER_ABSTIME: u32 = 1;
const NSEC_PER_SEC: isize = 1_000_000_000;
const NSEC_PER_MSEC: usize = 1_000_000;
const USEC_PER_SEC: usize = 1_000_000;
const ITIMER_REAL: i32 = 0;
const ITIMER_VIRTUAL: i32 = 1;
const ITIMER_PROF: i32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxTimeVal {
    tv_sec: isize,
    tv_usec: isize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxTimezone {
    tz_minuteswest: i32,
    tz_dsttime: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxTms {
    tms_utime: isize,
    tms_stime: isize,
    tms_cutime: isize,
    tms_cstime: isize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxITimerVal {
    it_interval: LinuxTimeVal,
    it_value: LinuxTimeVal,
}

#[derive(Clone, Copy)]
pub(crate) enum ClockBackend {
    Wall,
    Monotonic,
}

#[derive(Clone, Copy)]
enum ItimerKind {
    Real,
    Virtual,
    Prof,
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

impl ItimerKind {
    fn from_raw(which: i32) -> SysResult<Self> {
        match which {
            ITIMER_REAL => Ok(Self::Real),
            ITIMER_VIRTUAL => Ok(Self::Virtual),
            ITIMER_PROF => Ok(Self::Prof),
            _ => Err(SysError::EINVAL),
        }
    }
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
            Self::ProcessCpu | Self::ThreadCpu => Err(SysError::EINVAL),
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

pub(crate) fn current_clock_nanos(backend: ClockBackend) -> u64 {
    match backend {
        ClockBackend::Wall => wall_time_nanos(),
        ClockBackend::Monotonic => monotonic_time_nanos(),
    }
}

pub(crate) fn relative_timeout_deadline_ms(
    token: usize,
    timeout: *const LinuxTimeSpec,
) -> SysResult<Option<usize>> {
    if timeout.is_null() {
        return Ok(None);
    }
    let request = validate_timespec(read_user_value(token, timeout)?)?;
    let duration_ms = timespec_to_ms_ceil(request)?;
    Ok(Some(
        get_time_ms()
            .checked_add(duration_ms)
            .ok_or(SysError::EINVAL)?,
    ))
}

pub(crate) fn validate_timespec(time: LinuxTimeSpec) -> SysResult<LinuxTimeSpec> {
    if time.tv_sec < 0 || !(0..NSEC_PER_SEC).contains(&time.tv_nsec) {
        return Err(SysError::EINVAL);
    }
    Ok(time)
}

pub(crate) fn timespec_to_nanos(time: LinuxTimeSpec) -> SysResult<u64> {
    let time = validate_timespec(time)?;
    let sec_nanos = (time.tv_sec as u64)
        .checked_mul(NSEC_PER_SEC as u64)
        .ok_or(SysError::EINVAL)?;
    sec_nanos
        .checked_add(time.tv_nsec as u64)
        .ok_or(SysError::EINVAL)
}

pub(crate) fn nanos_to_ms_ceil(nanos: u64) -> SysResult<usize> {
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

fn clock_ticks_to_isize(ticks: usize) -> isize {
    ticks.min(isize::MAX as usize) as isize
}

impl LinuxTms {
    fn from_cpu_times(times: ProcessCpuTimesSnapshot) -> Self {
        Self {
            tms_utime: clock_ticks_to_isize(us_to_clock_ticks(times.user_us)),
            tms_stime: clock_ticks_to_isize(us_to_clock_ticks(times.system_us)),
            tms_cutime: clock_ticks_to_isize(us_to_clock_ticks(times.children_user_us)),
            tms_cstime: clock_ticks_to_isize(us_to_clock_ticks(times.children_system_us)),
        }
    }
}

fn timeval_to_us(time: LinuxTimeVal) -> SysResult<usize> {
    if time.tv_sec < 0 || time.tv_usec < 0 || time.tv_usec >= USEC_PER_SEC as isize {
        return Err(SysError::EINVAL);
    }
    (time.tv_sec as usize)
        .checked_mul(USEC_PER_SEC)
        .and_then(|sec_us| sec_us.checked_add(time.tv_usec as usize))
        .ok_or(SysError::EINVAL)
}

fn us_to_timeval(us: usize) -> LinuxTimeVal {
    LinuxTimeVal {
        tv_sec: (us / USEC_PER_SEC) as isize,
        tv_usec: (us % USEC_PER_SEC) as isize,
    }
}

fn us_to_timespec(us: usize) -> LinuxTimeSpec {
    LinuxTimeSpec {
        tv_sec: (us / USEC_PER_SEC).min(isize::MAX as usize) as isize,
        tv_nsec: ((us % USEC_PER_SEC) * 1_000) as isize,
    }
}

fn itimerval_from_us(interval_us: usize, value_us: usize) -> LinuxITimerVal {
    LinuxITimerVal {
        it_interval: us_to_timeval(interval_us),
        it_value: us_to_timeval(value_us),
    }
}

pub fn sys_getitimer(which: i32, value: *mut u8) -> SysResult {
    let kind = ItimerKind::from_raw(which)?;
    if value.is_null() {
        return Err(SysError::EFAULT);
    }
    let now_us = get_time_us();
    let process = current_process();
    let current = {
        let inner = process.inner_exclusive_access();
        let timer = match kind {
            ItimerKind::Real => &inner.real_timer,
            ItimerKind::Virtual => &inner.virtual_timer,
            ItimerKind::Prof => &inner.prof_timer,
        };
        itimerval_from_us(timer.interval_us, timer.remaining_us(now_us))
    };
    write_user_value(
        current_user_token(),
        value.cast::<LinuxITimerVal>(),
        &current,
    )?;
    Ok(0)
}

pub fn sys_setitimer(which: i32, value: *const u8, old_value: *mut u8) -> SysResult {
    let kind = ItimerKind::from_raw(which)?;
    let token = current_user_token();
    // CONTEXT: Linux treats a NULL new_value as a zero itimerval, disabling
    // the timer. The man page calls this nonportable, but it is Linux ABI.
    let new_value = if value.is_null() {
        LinuxITimerVal::default()
    } else {
        read_user_value(token, value.cast::<LinuxITimerVal>())?
    };
    let interval_us = timeval_to_us(new_value.it_interval)?;
    let value_us = timeval_to_us(new_value.it_value)?;
    let now_us = get_time_us();
    let next_expire_us = if value_us == 0 {
        0
    } else {
        now_us.checked_add(value_us).ok_or(SysError::EINVAL)?
    };
    let process = current_process();
    let old = {
        let inner = process.inner_exclusive_access();
        let timer = match kind {
            ItimerKind::Real => &inner.real_timer,
            ItimerKind::Virtual => &inner.virtual_timer,
            ItimerKind::Prof => &inner.prof_timer,
        };
        itimerval_from_us(timer.interval_us, timer.remaining_us(now_us))
    };
    if !old_value.is_null() {
        write_user_value(token, old_value.cast::<LinuxITimerVal>(), &old)?;
    }
    let event = {
        let mut inner = process.inner_exclusive_access();
        let timer = match kind {
            ItimerKind::Real => &mut inner.real_timer,
            ItimerKind::Virtual => &mut inner.virtual_timer,
            ItimerKind::Prof => &mut inner.prof_timer,
        };
        timer.generation = timer.generation.wrapping_add(1);
        timer.interval_us = interval_us;
        timer.next_expire_us = next_expire_us;
        match kind {
            ItimerKind::Real if next_expire_us != 0 => Some((next_expire_us, timer.generation)),
            ItimerKind::Virtual | ItimerKind::Prof if next_expire_us != 0 => {
                // UNFINISHED: Linux ITIMER_VIRTUAL and ITIMER_PROF count CPU
                // time and deliver SIGVTALRM/SIGPROF. This kernel currently
                // stores their set/get state but does not drive CPU-time
                // expiration or signal delivery.
                None
            }
            _ => None,
        }
    };
    if let Some((expire_us, generation)) = event {
        add_real_timer(expire_us, generation, process);
    }
    Ok(0)
}

pub fn sys_gettimeofday(tv: *mut LinuxTimeVal, tz: *mut LinuxTimezone) -> SysResult {
    let token = current_user_token();
    if !tv.is_null() {
        let wall_ns = wall_time_nanos();
        let time = LinuxTimeVal {
            tv_sec: (wall_ns / 1_000_000_000) as isize,
            tv_usec: ((wall_ns % 1_000_000_000) / 1_000) as isize,
        };
        write_user_value(token, tv, &time)?;
    }
    if !tz.is_null() {
        // CONTEXT: Linux keeps the timezone argument only for legacy callers.
        // This kernel has no timezone state, so report UTC-compatible zeroes.
        write_user_value(token, tz, &LinuxTimezone::default())?;
    }
    Ok(0)
}

pub fn sys_times(tms: *mut LinuxTms) -> SysResult {
    if !tms.is_null() {
        let linux_tms = LinuxTms::from_cpu_times(current_process().cpu_times_snapshot());
        write_user_value(current_user_token(), tms, &linux_tms)?;
    }
    Ok(clock_ticks_to_isize(get_time_clock_ticks()))
}

fn sleep_until_ms(expire_ms: usize) -> SysResult {
    if get_time_ms() >= expire_ms {
        return Ok(0);
    }
    if current_has_deliverable_signal() {
        return Err(SysError::EINTR);
    }
    let task = current_task().unwrap();
    add_timer(expire_ms, task);
    block_current_and_run_next();
    if get_time_ms() < expire_ms && current_has_deliverable_signal() {
        return Err(SysError::EINTR);
    }
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
    sleep_until_ms(expire_ms)
}

fn nanos_to_timespec(nanos: u64) -> LinuxTimeSpec {
    LinuxTimeSpec {
        tv_sec: (nanos / (NSEC_PER_SEC as u64)) as isize,
        tv_nsec: (nanos % (NSEC_PER_SEC as u64)) as isize,
    }
}

fn clock_getres_resolution(clock_id: i32) -> SysResult<LinuxTimeSpec> {
    match clock_id {
        CLOCK_REALTIME
        | CLOCK_MONOTONIC
        | CLOCK_PROCESS_CPUTIME_ID
        | CLOCK_THREAD_CPUTIME_ID
        | CLOCK_MONOTONIC_RAW
        | CLOCK_REALTIME_COARSE
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_BOOTTIME
        | CLOCK_REALTIME_ALARM
        | CLOCK_BOOTTIME_ALARM => Ok(nanos_to_timespec(1)),
        _ => Err(SysError::EINVAL),
    }
}

fn process_cpu_timespec() -> LinuxTimeSpec {
    let times = current_process().cpu_times_snapshot();
    us_to_timespec(times.user_us.saturating_add(times.system_us))
}

fn remaining_until_timespec(expire_ms: usize) -> LinuxTimeSpec {
    let remaining_ms = expire_ms.saturating_sub(get_time_ms());
    let remaining_nanos = (remaining_ms as u64).saturating_mul(NSEC_PER_MSEC as u64);
    nanos_to_timespec(remaining_nanos)
}

fn write_remaining_sleep_time(
    token: usize,
    rem: *mut LinuxTimeSpec,
    expire_ms: usize,
) -> SysResult {
    if rem.is_null() {
        return Ok(0);
    }
    let remaining = remaining_until_timespec(expire_ms);
    write_user_value(token, rem, &remaining)?;
    Ok(0)
}

pub fn sys_nanosleep(req: *const LinuxTimeSpec, rem: *mut LinuxTimeSpec) -> SysResult {
    if req.is_null() {
        return Err(SysError::EFAULT);
    }
    let token = current_user_token();
    let request = validate_timespec(read_user_value(token, req)?)?;
    let duration_ms = timespec_to_ms_ceil(request)?;
    if duration_ms == 0 {
        return Ok(0);
    }
    let expire_ms = get_time_ms()
        .checked_add(duration_ms)
        .ok_or(SysError::EINVAL)?;
    match sleep_until_ms(expire_ms) {
        Err(SysError::EINTR) => {
            write_remaining_sleep_time(token, rem, expire_ms)?;
            Err(SysError::EINTR)
        }
        result => result,
    }
}

pub fn sys_clock_gettime(clock_id: i32, tp: *mut LinuxTimeSpec) -> SysResult {
    if tp.is_null() {
        return Err(SysError::EFAULT);
    }
    let clock = ClockKind::from_raw(clock_id)?;
    let timespec = match clock {
        ClockKind::ProcessCpu => process_cpu_timespec(),
        ClockKind::ThreadCpu => {
            // UNFINISHED: Thread CPU time currently reuses process-wide
            // trap-boundary accounting because per-thread CPU accounting is
            // not represented separately in the task model yet.
            process_cpu_timespec()
        }
        _ => nanos_to_timespec(current_clock_nanos(clock.gettime_backend()?)),
    };
    write_user_value(current_user_token(), tp, &timespec)?;
    Ok(0)
}

pub fn sys_clock_getres(clock_id: i32, res: *mut LinuxTimeSpec) -> SysResult {
    let resolution = clock_getres_resolution(clock_id)?;
    if !res.is_null() {
        write_user_value(current_user_token(), res, &resolution)?;
    }
    Ok(0)
}

pub fn sys_clock_nanosleep(
    clock_id: i32,
    flags: u32,
    req: *const LinuxTimeSpec,
    rem: *mut LinuxTimeSpec,
) -> SysResult {
    if flags & !TIMER_ABSTIME != 0 {
        return Err(SysError::EINVAL);
    }
    let backend = ClockKind::from_raw(clock_id)?.nanosleep_backend()?;
    if req.is_null() {
        return Err(SysError::EFAULT);
    }

    let request = validate_timespec(read_user_value(current_user_token(), req)?)?;
    if flags & TIMER_ABSTIME != 0 {
        sleep_until_clock(backend, request)
    } else {
        let duration_ms = timespec_to_ms_ceil(request)?;
        if duration_ms == 0 {
            return Ok(0);
        }
        let expire_ms = get_time_ms()
            .checked_add(duration_ms)
            .ok_or(SysError::EINVAL)?;
        match sleep_until_ms(expire_ms) {
            Err(SysError::EINTR) => {
                write_remaining_sleep_time(current_user_token(), rem, expire_ms)?;
                Err(SysError::EINTR)
            }
            result => result,
        }
    }
}
