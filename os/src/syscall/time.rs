use crate::sync::UPIntrFreeCell;
use crate::task::{
    ProcessCpuTimesSnapshot, TaskControlBlock, block_current_task_no_schedule,
    current_has_deliverable_signal, current_process, current_user_token, pid2process,
    processes_snapshot, schedule,
};
use crate::timer::{
    add_posix_timer, add_real_timer, add_timer, get_time_clock_ticks, get_time_ms, get_time_us,
    monotonic_time_nanos, set_wall_time_nanos, us_to_clock_ticks, wall_time_nanos,
};
use alloc::sync::Arc;
use lazy_static::*;

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
const CPUCLOCK_PROF: i32 = 0;
const CPUCLOCK_VIRT: i32 = 1;
const CPUCLOCK_SCHED: i32 = 2;
const CPUCLOCK_CLOCK_MASK: i32 = 3;
const CPUCLOCK_PERTHREAD_MASK: i32 = 4;
const TIMER_ABSTIME: u32 = 1;
const NSEC_PER_SEC: isize = 1_000_000_000;
const NSEC_PER_MSEC: usize = 1_000_000;
const USEC_PER_SEC: usize = 1_000_000;
const ITIMER_REAL: i32 = 0;
const ITIMER_VIRTUAL: i32 = 1;
const ITIMER_PROF: i32 = 2;
const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGALRM: u32 = 14;
const ADJ_OFFSET: u32 = 0x0001;
const ADJ_FREQUENCY: u32 = 0x0002;
const ADJ_MAXERROR: u32 = 0x0004;
const ADJ_ESTERROR: u32 = 0x0008;
const ADJ_STATUS: u32 = 0x0010;
const ADJ_TIMECONST: u32 = 0x0020;
const ADJ_MICRO: u32 = 0x1000;
const ADJ_NANO: u32 = 0x2000;
const ADJ_TICK: u32 = 0x4000;
const ADJ_OFFSET_SINGLESHOT: u32 = 0x8001;
const ADJ_OFFSET_SS_READ: u32 = 0xa001;
const ADJ_ALL: u32 = ADJ_OFFSET
    | ADJ_FREQUENCY
    | ADJ_MAXERROR
    | ADJ_ESTERROR
    | ADJ_STATUS
    | ADJ_TIMECONST
    | ADJ_TICK;
const ADJ_SINGLESHOT_FLAG: u32 = ADJ_OFFSET_SINGLESHOT & !ADJ_OFFSET;
const STA_PLL: i32 = 0x0001;
const STA_PPSFREQ: i32 = 0x0002;
const STA_PPSTIME: i32 = 0x0004;
const STA_FLL: i32 = 0x0008;
const STA_INS: i32 = 0x0010;
const STA_DEL: i32 = 0x0020;
const STA_UNSYNC: i32 = 0x0040;
const STA_FREQHOLD: i32 = 0x0080;
const STA_NANO: i32 = 0x2000;
const STA_MODE: i32 = 0x4000;
const TIME_OK: isize = 0;
const TIME_ERROR: isize = 5;
const TIMEX_SETTABLE_STATUS_BITS: i32 = STA_PLL
    | STA_PPSFREQ
    | STA_PPSTIME
    | STA_FLL
    | STA_INS
    | STA_DEL
    | STA_UNSYNC
    | STA_FREQHOLD
    | STA_MODE;
const TIMEX_SETTABLE_BIT_MODES: u32 = ADJ_ALL | ADJ_MICRO | ADJ_NANO;
const DEFAULT_TAI_OFFSET_SECS: i32 = 37;

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

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxITimerSpec {
    it_interval: LinuxTimeSpec,
    it_value: LinuxTimeSpec,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxSigeventPrefix {
    value: usize,
    signo: i32,
    notify: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxTimex {
    modes: u32,
    offset: isize,
    freq: isize,
    maxerror: isize,
    esterror: isize,
    status: i32,
    constant: isize,
    precision: isize,
    tolerance: isize,
    time: LinuxTimeVal,
    tick: isize,
    ppsfreq: isize,
    jitter: isize,
    shift: i32,
    stabil: isize,
    jitcnt: isize,
    calcnt: isize,
    errcnt: isize,
    stbcnt: isize,
    tai: i32,
    _padding: [i32; 11],
}

#[derive(Clone, Copy, Debug)]
struct TimexState {
    offset: isize,
    freq: isize,
    maxerror: isize,
    esterror: isize,
    status: i32,
    constant: isize,
    precision: isize,
    tolerance: isize,
    tick: isize,
    tai: i32,
}

impl TimexState {
    const fn new() -> Self {
        Self {
            offset: 0,
            freq: 0,
            maxerror: 0,
            esterror: 0,
            status: 0,
            constant: 0,
            precision: 1,
            tolerance: 0,
            tick: (USEC_PER_SEC / crate::timer::TICKS_PER_SEC) as isize,
            tai: DEFAULT_TAI_OFFSET_SECS,
        }
    }

    fn resolution_mode(self) -> u32 {
        if self.status & STA_NANO != 0 {
            ADJ_NANO
        } else {
            ADJ_MICRO
        }
    }

    fn time_state(self) -> isize {
        if self.status & STA_UNSYNC != 0 {
            TIME_ERROR
        } else {
            TIME_OK
        }
    }
}

lazy_static! {
    static ref TIMEX_STATE: UPIntrFreeCell<TimexState> =
        unsafe { UPIntrFreeCell::new(TimexState::new()) };
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
    Ok(Some(relative_timeout_deadline_ms_from_nanos(
        timespec_to_nanos(request)?,
    )?))
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

fn relative_sleep_deadline_ms(time: LinuxTimeSpec) -> SysResult<usize> {
    relative_timeout_deadline_ms_from_nanos(timespec_to_nanos(time)?)
}

fn us_to_ms_ceil(us: usize) -> SysResult<usize> {
    us.checked_add(999)
        .map(|us| us / 1000)
        .ok_or(SysError::EINVAL)
}

fn nanos_to_us_ceil(nanos: u64) -> SysResult<usize> {
    let us = nanos / 1000 + if nanos % 1000 == 0 { 0 } else { 1 };
    if us > usize::MAX as u64 {
        return Err(SysError::EINVAL);
    }
    Ok(us as usize)
}

pub(crate) fn relative_timeout_deadline_ms_from_nanos(duration_nanos: u64) -> SysResult<usize> {
    let duration_us = nanos_to_us_ceil(duration_nanos)?;
    let deadline_us = get_time_us()
        .checked_add(duration_us)
        .ok_or(SysError::EINVAL)?;
    us_to_ms_ceil(deadline_us)
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

fn itimerspec_from_us(interval_us: usize, value_us: usize) -> LinuxITimerSpec {
    LinuxITimerSpec {
        it_interval: us_to_timespec(interval_us),
        it_value: us_to_timespec(value_us),
    }
}

fn itimerval_from_us(interval_us: usize, value_us: usize) -> LinuxITimerVal {
    LinuxITimerVal {
        it_interval: us_to_timeval(interval_us),
        it_value: us_to_timeval(value_us),
    }
}

fn decode_sigevent_signal(sevp: *const u8) -> SysResult<u32> {
    if sevp.is_null() {
        return Ok(SIGALRM);
    }
    let event = read_user_value(current_user_token(), sevp.cast::<LinuxSigeventPrefix>())?;
    match event.notify {
        SIGEV_SIGNAL => {
            let signal = event.signo as u32;
            if signal == 0 || signal as usize >= crate::task::SIGNAL_INFO_SLOTS {
                return Err(SysError::EINVAL);
            }
            Ok(signal)
        }
        SIGEV_NONE => Ok(0),
        _ => Err(SysError::EINVAL),
    }
}

fn itimerspec_to_us(value: LinuxITimerSpec) -> SysResult<(usize, usize)> {
    Ok((
        nanos_to_us_ceil(timespec_to_nanos(value.it_interval)?)?,
        nanos_to_us_ceil(timespec_to_nanos(value.it_value)?)?,
    ))
}

fn posix_timer_deadline_us(clock_id: i32, flags: i32, value_us: usize) -> SysResult<usize> {
    if value_us == 0 {
        return Ok(0);
    }
    if flags & TIMER_ABSTIME as i32 != 0 {
        let now_nanos = current_clock_nanos(ClockKind::from_raw(clock_id)?.gettime_backend()?);
        let target_nanos = (value_us as u64)
            .checked_mul(1_000)
            .ok_or(SysError::EINVAL)?;
        let remaining_us = nanos_to_us_ceil(target_nanos.saturating_sub(now_nanos))?;
        get_time_us()
            .checked_add(remaining_us)
            .ok_or(SysError::EINVAL)
    } else {
        get_time_us().checked_add(value_us).ok_or(SysError::EINVAL)
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
            ItimerKind::Real => &inner.timers.real,
            ItimerKind::Virtual => &inner.timers.virtual_timer,
            ItimerKind::Prof => &inner.timers.prof,
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
            ItimerKind::Real => &inner.timers.real,
            ItimerKind::Virtual => &inner.timers.virtual_timer,
            ItimerKind::Prof => &inner.timers.prof,
        };
        itimerval_from_us(timer.interval_us, timer.remaining_us(now_us))
    };
    if !old_value.is_null() {
        write_user_value(token, old_value.cast::<LinuxITimerVal>(), &old)?;
    }
    let event = {
        let mut inner = process.inner_exclusive_access();
        let timer = match kind {
            ItimerKind::Real => &mut inner.timers.real,
            ItimerKind::Virtual => &mut inner.timers.virtual_timer,
            ItimerKind::Prof => &mut inner.timers.prof,
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

pub fn sys_timer_create(clock_id: i32, sevp: *const u8, timerid: *mut i32) -> SysResult {
    if timerid.is_null() {
        return Err(SysError::EFAULT);
    }
    // UNFINISHED: POSIX CPU timers, alarm clocks, SIGEV_THREAD, and
    // SIGEV_THREAD_ID are not modeled yet. This covers the signal timers used
    // by LTP clock_settime03 and keeps unsupported clocks fail-loud.
    match ClockKind::from_raw(clock_id)? {
        ClockKind::Realtime | ClockKind::Monotonic | ClockKind::Boottime => {}
        _ => return Err(SysError::ENOTSUP),
    }
    let signal = decode_sigevent_signal(sevp)?;
    let id = current_process().create_posix_timer(clock_id, signal);
    write_user_value(current_user_token(), timerid, &(id as i32))?;
    Ok(0)
}

pub fn sys_timer_settime(
    timerid: i32,
    flags: i32,
    new_value: *const LinuxITimerSpec,
    old_value: *mut LinuxITimerSpec,
) -> SysResult {
    if timerid < 0 || flags & !(TIMER_ABSTIME as i32) != 0 {
        return Err(SysError::EINVAL);
    }
    if new_value.is_null() {
        return Err(SysError::EFAULT);
    }
    let new_value = read_user_value(current_user_token(), new_value)?;
    let (interval_us, value_us) = itimerspec_to_us(new_value)?;
    let process = current_process();
    let now_us = get_time_us();
    let clock_id = process
        .posix_timer_clock(timerid as usize)
        .ok_or(SysError::EINVAL)?;
    let next_expire_us = posix_timer_deadline_us(clock_id, flags, value_us)?;
    let (old_interval_us, old_remaining_us, generation) = process
        .set_posix_timer(timerid as usize, interval_us, next_expire_us, now_us)
        .ok_or(SysError::EINVAL)?;
    if !old_value.is_null() {
        let old = itimerspec_from_us(old_interval_us, old_remaining_us);
        write_user_value(current_user_token(), old_value, &old)?;
    }
    if next_expire_us != 0 {
        add_posix_timer(next_expire_us, timerid as usize, generation, process);
    }
    Ok(0)
}

pub fn sys_timer_gettime(timerid: i32, curr_value: *mut LinuxITimerSpec) -> SysResult {
    if timerid < 0 {
        return Err(SysError::EINVAL);
    }
    if curr_value.is_null() {
        return Err(SysError::EFAULT);
    }
    let (interval_us, remaining_us) = current_process()
        .posix_timer_snapshot(timerid as usize, get_time_us())
        .ok_or(SysError::EINVAL)?;
    let current = itimerspec_from_us(interval_us, remaining_us);
    write_user_value(current_user_token(), curr_value, &current)?;
    Ok(0)
}

pub fn sys_timer_getoverrun(timerid: i32) -> SysResult {
    if timerid < 0 {
        return Err(SysError::EINVAL);
    }
    current_process()
        .posix_timer_snapshot(timerid as usize, get_time_us())
        .ok_or(SysError::EINVAL)?;
    Ok(0)
}

pub fn sys_timer_delete(timerid: i32) -> SysResult {
    if timerid < 0 {
        return Err(SysError::EINVAL);
    }
    current_process()
        .delete_posix_timer(timerid as usize)
        .ok_or(SysError::EINVAL)?;
    Ok(0)
}

pub fn sys_gettimeofday(tv: *mut LinuxTimeVal, tz: *mut LinuxTimezone) -> SysResult {
    let token = current_user_token();
    if !tv.is_null() {
        write_user_value(token, tv, &wall_timeval())?;
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
    let (task, task_cx_ptr) = block_current_task_no_schedule();
    add_timer(expire_ms, task);
    schedule(task_cx_ptr);
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
    let expire_ms = relative_timeout_deadline_ms_from_nanos(deadline_nanos - now_nanos)?;
    sleep_until_ms(expire_ms)
}

fn nanos_to_timespec(nanos: u64) -> LinuxTimeSpec {
    LinuxTimeSpec {
        tv_sec: (nanos / (NSEC_PER_SEC as u64)) as isize,
        tv_nsec: (nanos % (NSEC_PER_SEC as u64)) as isize,
    }
}

fn wall_timeval() -> LinuxTimeVal {
    let wall_ns = wall_time_nanos();
    LinuxTimeVal {
        tv_sec: (wall_ns / 1_000_000_000) as isize,
        tv_usec: ((wall_ns % 1_000_000_000) / 1_000) as isize,
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

fn task_with_linux_tid(tid: usize) -> Option<Arc<TaskControlBlock>> {
    processes_snapshot()
        .into_iter()
        .flat_map(|process| process.tasks_snapshot())
        .find(|task| task.linux_tid() == tid)
}

fn cpu_clock_target_id(clock_id: i32) -> SysResult<(bool, usize)> {
    if clock_id >= 0 {
        return Err(SysError::EINVAL);
    }
    match clock_id & CPUCLOCK_CLOCK_MASK {
        CPUCLOCK_PROF | CPUCLOCK_VIRT | CPUCLOCK_SCHED => {}
        _ => return Err(SysError::EINVAL),
    }
    let id = !(clock_id >> 3);
    if id < 0 {
        return Err(SysError::EINVAL);
    }
    Ok((clock_id & CPUCLOCK_PERTHREAD_MASK != 0, id as usize))
}

fn dynamic_cpu_clock_timespec(clock_id: i32) -> SysResult<LinuxTimeSpec> {
    let (per_thread, id) = cpu_clock_target_id(clock_id)?;
    if per_thread {
        let task = task_with_linux_tid(id).ok_or(SysError::EINVAL)?;
        Ok(us_to_timespec(task.cpu_time_us()))
    } else {
        let process = if id == 0 {
            current_process()
        } else {
            pid2process(id).ok_or(SysError::EINVAL)?
        };
        let times = process.cpu_times_snapshot();
        Ok(us_to_timespec(
            times.user_us.saturating_add(times.system_us),
        ))
    }
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
    let expire_ms = relative_sleep_deadline_ms(request)?;
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
    if clock_id < 0 {
        let timespec = dynamic_cpu_clock_timespec(clock_id)?;
        write_user_value(current_user_token(), tp, &timespec)?;
        return Ok(0);
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

pub fn sys_clock_settime(clock_id: i32, tp: *const LinuxTimeSpec) -> SysResult {
    if clock_id != CLOCK_REALTIME {
        return Err(SysError::EINVAL);
    }
    if tp.is_null() {
        return Err(SysError::EFAULT);
    }
    let request = validate_timespec(read_user_value(current_user_token(), tp)?)?;
    let credentials = current_process().credentials();
    if credentials.euid != 0 {
        return Err(SysError::EPERM);
    }
    set_wall_time_nanos(timespec_to_nanos(request)?);
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
        let expire_ms = relative_sleep_deadline_ms(request)?;
        match sleep_until_ms(expire_ms) {
            Err(SysError::EINTR) => {
                write_remaining_sleep_time(current_user_token(), rem, expire_ms)?;
                Err(SysError::EINTR)
            }
            result => result,
        }
    }
}

fn timex_modes_supported(modes: u32) -> bool {
    match modes {
        0 | ADJ_OFFSET_SINGLESHOT | ADJ_OFFSET_SS_READ => true,
        _ if modes & ADJ_SINGLESHOT_FLAG != 0 => false,
        _ => modes & !TIMEX_SETTABLE_BIT_MODES == 0,
    }
}

fn timex_tick_bounds() -> (isize, isize) {
    let hz = crate::timer::TICKS_PER_SEC as isize;
    (900_000 / hz, 1_100_000 / hz)
}

fn timex_resolution_is_nanos(modes: u32, state: TimexState) -> bool {
    if modes & ADJ_NANO != 0 {
        true
    } else if modes & ADJ_MICRO != 0 {
        false
    } else {
        state.status & STA_NANO != 0
    }
}

fn fill_timex_output(timex: &mut LinuxTimex, state: TimexState) {
    timex.modes = state.resolution_mode();
    timex.offset = state.offset;
    timex.freq = state.freq;
    timex.maxerror = state.maxerror;
    timex.esterror = state.esterror;
    timex.status = state.status;
    timex.constant = state.constant;
    timex.precision = state.precision;
    timex.tolerance = state.tolerance;
    timex.time = wall_timeval();
    timex.tick = state.tick;
    timex.ppsfreq = 0;
    timex.jitter = 0;
    timex.shift = 0;
    timex.stabil = 0;
    timex.jitcnt = 0;
    timex.calcnt = 0;
    timex.errcnt = 0;
    timex.stbcnt = 0;
    timex.tai = state.tai;
}

fn update_timex_state(state: &mut TimexState, timex: LinuxTimex) -> SysResult<()> {
    let modes = timex.modes;
    if !timex_modes_supported(modes) {
        return Err(SysError::EINVAL);
    }
    if modes & ADJ_MICRO != 0 {
        state.status &= !STA_NANO;
    }
    if modes & ADJ_NANO != 0 {
        state.status |= STA_NANO;
    }
    if modes & ADJ_STATUS != 0 {
        if timex.status & !TIMEX_SETTABLE_STATUS_BITS != 0 {
            return Err(SysError::EINVAL);
        }
        state.status = (state.status & !TIMEX_SETTABLE_STATUS_BITS)
            | (timex.status & TIMEX_SETTABLE_STATUS_BITS);
    }
    if modes & ADJ_OFFSET != 0 {
        let limit = if timex_resolution_is_nanos(modes, *state) {
            500_000isize * 1000
        } else {
            500_000isize
        };
        if timex.offset <= -limit || timex.offset >= limit {
            return Err(SysError::EINVAL);
        }
        state.offset = timex.offset;
    }
    if modes & ADJ_FREQUENCY != 0 {
        state.freq = timex.freq.clamp(-32_768_000, 32_768_000);
    }
    if modes & ADJ_MAXERROR != 0 {
        state.maxerror = timex.maxerror;
    }
    if modes & ADJ_ESTERROR != 0 {
        state.esterror = timex.esterror;
    }
    if modes & ADJ_TIMECONST != 0 {
        state.constant = timex.constant;
    }
    if modes & ADJ_TICK != 0 {
        let (min_tick, max_tick) = timex_tick_bounds();
        if timex.tick < min_tick || timex.tick > max_tick {
            return Err(SysError::EINVAL);
        }
        state.tick = timex.tick;
    }
    if modes == ADJ_OFFSET_SINGLESHOT {
        state.offset = timex.offset;
    }
    Ok(())
}

pub fn sys_clock_adjtime(clock_id: i32, timex: *mut LinuxTimex) -> SysResult {
    if clock_id != CLOCK_REALTIME {
        return Err(SysError::EINVAL);
    }
    if timex.is_null() {
        return Err(SysError::EFAULT);
    }
    let token = current_user_token();
    let mut user_timex = read_user_value(token, timex)?;
    let modes = user_timex.modes;
    let credentials = current_process().credentials();
    if credentials.euid != 0 && modes != 0 && modes != ADJ_OFFSET_SS_READ {
        return Err(SysError::EPERM);
    }
    let ret = {
        let mut state = TIMEX_STATE.exclusive_access();
        if modes != 0 && modes != ADJ_OFFSET_SS_READ {
            update_timex_state(&mut state, user_timex)?;
        }
        fill_timex_output(&mut user_timex, *state);
        state.time_state()
    };
    write_user_value(token, timex, &user_timex)?;
    Ok(ret)
}
