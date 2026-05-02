use crate::task::{SignalFlags, SignalInfo, current_process, current_user_token};
use crate::timer::get_time_ms;

use super::errno::{SysError, SysResult};
use super::fs::LinuxTimeSpec;
use super::fs::user_ptr::{read_user_value, write_user_value};
use super::sync::relative_timeout_deadline_ms;
use super::wait::LinuxSigInfo;

const LINUX_RT_SIGSET_SIZE: usize = 8;

fn linux_sigset_to_flags(raw: u64) -> SignalFlags {
    SignalFlags::from_bits_truncate((raw as u32) << 1)
}

fn read_signal_set(token: usize, set: *const u8, sigsetsize: usize) -> SysResult<SignalFlags> {
    if sigsetsize != LINUX_RT_SIGSET_SIZE {
        return Err(SysError::EINVAL);
    }
    let raw_set = read_user_value(token, set.cast::<u64>())?;
    let mut flags = linux_sigset_to_flags(raw_set);
    flags.remove(SignalFlags::SIGKILL);
    flags.remove(SignalFlags::SIGSTOP);
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
    info_ptr: *mut LinuxSigInfo,
) -> SysResult<Option<isize>> {
    let Some((signum, info)) = peek_pending_signal(wanted) else {
        return Ok(None);
    };

    if !info_ptr.is_null() {
        let info = LinuxSigInfo::from_signal_info(info);
        write_user_value(token, info_ptr, &info)?;
    }
    consume_pending_signal(signum);
    Ok(Some(signum as isize))
}

pub fn sys_rt_sigtimedwait(
    set: *const u8,
    info: *mut LinuxSigInfo,
    timeout: *const LinuxTimeSpec,
    sigsetsize: usize,
) -> SysResult {
    let token = current_user_token();
    let wanted = read_signal_set(token, set, sigsetsize)?;
    let deadline_ms = relative_timeout_deadline_ms(token, timeout)?;

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
