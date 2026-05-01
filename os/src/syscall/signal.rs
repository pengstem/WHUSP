use crate::task::{SignalFlags, SignalInfo, current_process, current_user_token};
use crate::timer::get_time_ms;

use super::errno::{SysError, SysResult};
use super::fs::LinuxTimeSpec;
use super::fs::user_ptr::{read_user_value, write_user_value};

const LINUX_RT_SIGSET_SIZE: usize = 8;
const NSEC_PER_SEC: isize = 1_000_000_000;
const NSEC_PER_MSEC: usize = 1_000_000;
const SIGKILL: u32 = 9;
const SIGSTOP: u32 = 19;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RtSigInfo {
    si_signo: i32,
    si_errno: i32,
    si_code: i32,
    si_trapno: i32,
    si_pid: i32,
    si_uid: u32,
    si_status: i32,
    si_utime: u32,
    si_stime: u32,
    si_value: u64,
    pad: [u32; 20],
    align: [u64; 0],
}

impl RtSigInfo {
    fn from_signal_info(info: SignalInfo) -> Self {
        Self {
            si_signo: info.signo,
            si_code: info.code,
            si_pid: info.pid,
            si_uid: info.uid,
            si_status: info.status,
            ..Self::default()
        }
    }
}

fn validate_timespec(time: LinuxTimeSpec) -> SysResult<LinuxTimeSpec> {
    if time.tv_sec < 0 || !(0..NSEC_PER_SEC).contains(&time.tv_nsec) {
        return Err(SysError::EINVAL);
    }
    Ok(time)
}

fn timespec_to_ms_ceil(time: LinuxTimeSpec) -> SysResult<usize> {
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

fn linux_sigset_to_flags(raw: u64) -> SignalFlags {
    let mut flags = SignalFlags::empty();
    for signum in 1..32 {
        if raw & (1u64 << (signum - 1)) != 0 {
            if let Some(flag) = SignalFlags::from_signum(signum) {
                flags |= flag;
            }
        }
    }
    flags
}

fn read_signal_set(token: usize, set: *const u8, sigsetsize: usize) -> SysResult<SignalFlags> {
    if sigsetsize != LINUX_RT_SIGSET_SIZE {
        return Err(SysError::EINVAL);
    }
    let raw_set = read_user_value(token, set.cast::<u64>())?;
    let mut flags = linux_sigset_to_flags(raw_set);
    if let Some(sigkill) = SignalFlags::from_signum(SIGKILL) {
        flags.remove(sigkill);
    }
    if let Some(sigstop) = SignalFlags::from_signum(SIGSTOP) {
        flags.remove(sigstop);
    }
    // UNFINISHED: This kernel currently tracks ordinary signals 1..31 in a
    // process-wide bitset. Linux realtime signal queues, per-thread pending
    // sets, and full signal-mask interaction are not modeled yet.
    Ok(flags)
}

fn lowest_signal(flags: SignalFlags) -> Option<u32> {
    if flags.is_empty() {
        None
    } else {
        Some(flags.bits().trailing_zeros())
    }
}

fn default_signal_info(signum: u32) -> SignalInfo {
    SignalInfo::user(signum as i32, 0)
}

fn peek_pending_signal(wanted: SignalFlags) -> Option<(u32, SignalInfo)> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    let signum = lowest_signal(inner.signals & wanted)?;
    let info = inner
        .signal_infos
        .get(signum as usize)
        .copied()
        .flatten()
        .unwrap_or_else(|| default_signal_info(signum));
    Some((signum, info))
}

fn consume_pending_signal(signum: u32) {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if let Some(flag) = SignalFlags::from_signum(signum) {
        inner.signals.remove(flag);
    }
    if let Some(info) = inner.signal_infos.get_mut(signum as usize) {
        *info = None;
    }
}

fn try_return_pending_signal(
    token: usize,
    wanted: SignalFlags,
    info_ptr: *mut RtSigInfo,
) -> SysResult<Option<isize>> {
    let Some((signum, info)) = peek_pending_signal(wanted) else {
        return Ok(None);
    };

    if !info_ptr.is_null() {
        let info = RtSigInfo::from_signal_info(info);
        write_user_value(token, info_ptr, &info)?;
    }
    consume_pending_signal(signum);
    Ok(Some(signum as isize))
}

pub fn sys_rt_sigtimedwait(
    set: *const u8,
    info: *mut RtSigInfo,
    timeout: *const LinuxTimeSpec,
    sigsetsize: usize,
) -> SysResult {
    let token = current_user_token();
    let wanted = read_signal_set(token, set, sigsetsize)?;
    let deadline_ms = timeout_deadline_ms(token, timeout)?;

    loop {
        if let Some(signum) = try_return_pending_signal(token, wanted, info)? {
            return Ok(signum);
        }

        if let Some(deadline_ms) = deadline_ms {
            if get_time_ms() >= deadline_ms {
                return Err(SysError::EAGAIN);
            }
        }

        // UNFINISHED: A real Linux implementation sleeps interruptibly and is
        // woken by signal delivery. Until this kernel has signal wait queues,
        // yield cooperatively so child exit and kill paths can run.
        crate::task::suspend_current_and_run_next();
    }
}
