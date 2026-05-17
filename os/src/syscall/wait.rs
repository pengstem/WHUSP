use crate::mm::MemorySet;
use crate::syscall::user_ptr::write_user_value_in_memory_set;
use crate::task::{
    CLD_CONTINUED, CLD_STOPPED, ProcessControlBlock, SIGCHLD, SIGCONT, SignalInfo,
    block_current_task_no_schedule, current_process, remove_from_pid2process, schedule,
    signal_child_status, signal_wait_status, task_has_wait_interrupt_signal, wakeup_task,
};
use alloc::sync::Arc;

use super::errno::{SysError, SysResult};

const WNOHANG: i32 = 1;
const WUNTRACED: i32 = 2;
const WEXITED: i32 = 4;
const WCONTINUED: i32 = 8;
const WNOWAIT: i32 = 0x01000000;
const WALL: i32 = 0x40000000;

const P_ALL: i32 = 0;
const P_PID: i32 = 1;
const P_PGID: i32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TimeVal {
    sec: isize,
    usec: isize,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RUsage {
    utime: TimeVal,
    stime: TimeVal,
    maxrss: isize,
    ixrss: isize,
    idrss: isize,
    isrss: isize,
    minflt: isize,
    majflt: isize,
    nswap: isize,
    inblock: isize,
    oublock: isize,
    msgsnd: isize,
    msgrcv: isize,
    nsignals: isize,
    nvcsw: isize,
    nivcsw: isize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxSigInfo {
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

impl From<SignalInfo> for LinuxSigInfo {
    fn from(info: SignalInfo) -> Self {
        Self {
            si_signo: info.signo,
            si_code: info.code,
            si_pid: info.pid,
            si_uid: info.uid,
            si_status: info.status,
            si_value: info.value,
            ..Self::default()
        }
    }
}

impl LinuxSigInfo {
    pub(crate) fn to_signal_info(self, fallback_signum: u32, fallback_pid: i32) -> SignalInfo {
        SignalInfo {
            signo: if self.si_signo == 0 {
                fallback_signum as i32
            } else {
                self.si_signo
            },
            code: self.si_code,
            pid: if self.si_pid == 0 {
                fallback_pid
            } else {
                self.si_pid
            },
            uid: self.si_uid,
            status: self.si_status,
            value: self.si_value,
        }
    }
}

fn wait_status(exit_code: i32) -> i32 {
    if let Some(status) = signal_wait_status(exit_code) {
        status
    } else {
        (exit_code & 0xff) << 8
    }
}

fn waitid_code_and_status(exit_code: i32) -> (i32, i32) {
    signal_child_status(exit_code)
}

fn wait4_child_matches(child: &Arc<ProcessControlBlock>, pid: isize, caller_pgid: usize) -> bool {
    match pid {
        -1 => true,
        0 => child.process_group_id() == caller_pgid,
        pid if pid > 0 => child.getpid() == pid as usize,
        pid => child.process_group_id() == pid.wrapping_neg() as usize,
    }
}

fn waitid_child_matches(
    child: &Arc<ProcessControlBlock>,
    idtype: i32,
    id: i32,
    caller_pgid: usize,
) -> bool {
    match idtype {
        P_ALL => true,
        P_PID => child.getpid() == id as usize,
        P_PGID => {
            let pgid = if id == 0 { caller_pgid } else { id as usize };
            child.process_group_id() == pgid
        }
        _ => false,
    }
}

fn write_rusage(memory_set: &mut MemorySet, rusage: *mut RUsage) -> SysResult<()> {
    if !rusage.is_null() {
        write_user_value_in_memory_set(memory_set, rusage, &RUsage::default())?;
    }
    Ok(())
}

fn write_waitid_siginfo(
    memory_set: &mut MemorySet,
    infop: *mut LinuxSigInfo,
    child_pid: usize,
    si_code: i32,
    si_status: i32,
) -> SysResult<()> {
    if !infop.is_null() {
        write_user_value_in_memory_set(
            memory_set,
            infop,
            &LinuxSigInfo {
                si_signo: SIGCHLD as i32,
                si_code,
                si_pid: child_pid as i32,
                si_status,
                ..LinuxSigInfo::default()
            },
        )?;
    }
    Ok(())
}

/// Waits for and reaps a matching child process using Linux wait4 status rules.
///
/// Reaping removes the child from both PID lookup and the parent's child list;
/// `WNOHANG` observes the current state without blocking.
pub fn sys_wait4(pid: isize, wstatus: *mut i32, options: i32, rusage: *mut RUsage) -> SysResult {
    if options < 0 || options & !(WNOHANG | WUNTRACED | WCONTINUED | WALL) != 0 {
        return Err(SysError::EINVAL);
    }
    if pid == i32::MIN as isize {
        // CONTEXT: Linux rejects INT_MIN before treating negative pid values as
        // process-group selectors because abs(INT_MIN) cannot be represented.
        return Err(SysError::ESRCH);
    }

    loop {
        let process = current_process();
        let caller_pgid = process.process_group_id();
        let mut inner = process.inner_exclusive_access();
        if !inner
            .children
            .iter()
            .any(|child| wait4_child_matches(child, pid, caller_pgid))
        {
            return Err(SysError::ECHILD);
        }

        let zombie = inner.children.iter().enumerate().find(|(_, child)| {
            wait4_child_matches(child, pid, caller_pgid) && child.inner_exclusive_access().is_zombie
        });
        if let Some((idx, child)) = zombie {
            let (found_pid, exit_code, child_times) = {
                let child_inner = child.inner_exclusive_access();
                (
                    child.getpid(),
                    child_inner.exit_code,
                    child_inner.cpu_times.snapshot(),
                )
            };
            if !wstatus.is_null() {
                write_user_value_in_memory_set(
                    &mut inner.memory_set,
                    wstatus,
                    &wait_status(exit_code),
                )?;
            }
            write_rusage(&mut inner.memory_set, rusage)?;
            inner.cpu_times.add_waited_child(child_times);

            // CONTEXT: Linux keeps a zombie PID visible until the parent reaps it.
            // Remove the process from PID lookup only at the wait/reap boundary.
            remove_from_pid2process(found_pid);
            // CONTEXT: Reaping completes when the child is removed from both
            // PID lookup and the parent's child list. Other internal kernel
            // references must not turn a successful wait into a panic.
            drop(inner.children.remove(idx));
            return Ok(found_pid as isize);
        }

        if options & WNOHANG != 0 {
            return Ok(0);
        }
        // CONTEXT: LTP's heartbeat uses SIGUSR1 through musl signal(2), which
        // installs SA_RESTART. Keep that heartbeat from aborting the harness,
        // but return to trap delivery for timeout handlers and fatal signals.
        // UNFINISHED: Linux can run restartable handlers and transparently
        // restart wait4(); this kernel only preserves the SIGUSR1 heartbeat.
        // CONTEXT: Mark the task blocked before releasing the parent child-list
        // lock. Otherwise a timer tick can run the exiting child between the
        // zombie scan and the sleep transition, causing the child's wakeup to be
        // lost and the parent to sleep forever.
        let (task, task_cx_ptr) = block_current_task_no_schedule();
        drop(inner);
        let interrupted = task_has_wait_interrupt_signal(&task, &process);
        if interrupted {
            wakeup_task(task);
        }
        drop(process);
        schedule(task_cx_ptr);
        if interrupted {
            return Err(SysError::EINTR);
        }
    }
}

/// Waits for child state changes and optionally leaves the zombie unreaped.
///
/// `WNOWAIT` fills `siginfo_t`/`rusage` without removing the child; all other
/// successful zombie observations complete the same reap boundary as wait4.
pub fn sys_waitid(
    idtype: i32,
    id: i32,
    infop: *mut LinuxSigInfo,
    options: i32,
    rusage: *mut RUsage,
) -> SysResult {
    if options < 0
        || options & !(WNOHANG | WEXITED | WNOWAIT | WUNTRACED | WCONTINUED) != 0
        || options & (WEXITED | WUNTRACED | WCONTINUED) == 0
    {
        return Err(SysError::EINVAL);
    }
    if idtype != P_ALL && idtype != P_PID && idtype != P_PGID {
        return Err(SysError::ECHILD);
    }
    if idtype == P_PID && id <= 0 {
        return Err(SysError::EINVAL);
    }
    if idtype == P_PGID && id < 0 {
        return Err(SysError::EINVAL);
    }

    loop {
        let process = current_process();
        let caller_pgid = process.process_group_id();
        let mut inner = process.inner_exclusive_access();
        if !inner
            .children
            .iter()
            .any(|child| waitid_child_matches(child, idtype, id, caller_pgid))
        {
            return Err(SysError::ECHILD);
        }

        if options & WUNTRACED != 0 {
            let stopped = inner.children.iter().find_map(|child| {
                if !waitid_child_matches(child, idtype, id, caller_pgid) {
                    return None;
                }
                let child_inner = child.inner_exclusive_access();
                child_inner
                    .wait_stop_status
                    .map(|status| (child.getpid(), status))
            });
            if let Some((child_pid, stop_status)) = stopped {
                write_waitid_siginfo(
                    &mut inner.memory_set,
                    infop,
                    child_pid,
                    CLD_STOPPED,
                    stop_status,
                )?;
                write_rusage(&mut inner.memory_set, rusage)?;
                if options & WNOWAIT == 0
                    && let Some(child) = inner
                        .children
                        .iter()
                        .find(|child| child.getpid() == child_pid)
                {
                    child.inner_exclusive_access().wait_stop_status = None;
                }
                return Ok(0);
            }
        }

        if options & WCONTINUED != 0 {
            let continued = inner.children.iter().find_map(|child| {
                if !waitid_child_matches(child, idtype, id, caller_pgid) {
                    return None;
                }
                if child.inner_exclusive_access().wait_continued {
                    Some(child.getpid())
                } else {
                    None
                }
            });
            if let Some(child_pid) = continued {
                write_waitid_siginfo(
                    &mut inner.memory_set,
                    infop,
                    child_pid,
                    CLD_CONTINUED,
                    SIGCONT as i32,
                )?;
                write_rusage(&mut inner.memory_set, rusage)?;
                if options & WNOWAIT == 0
                    && let Some(child) = inner
                        .children
                        .iter()
                        .find(|child| child.getpid() == child_pid)
                {
                    child.inner_exclusive_access().wait_continued = false;
                }
                return Ok(0);
            }
        }

        let zombie = inner.children.iter().enumerate().find(|(_, child)| {
            waitid_child_matches(child, idtype, id, caller_pgid)
                && child.inner_exclusive_access().is_zombie
        });
        if let Some((idx, child)) = zombie {
            let (child_pid, exit_code, child_times) = {
                let child_inner = child.inner_exclusive_access();
                (
                    child.getpid(),
                    child_inner.exit_code,
                    child_inner.cpu_times.snapshot(),
                )
            };
            let (si_code, si_status) = waitid_code_and_status(exit_code);
            write_waitid_siginfo(&mut inner.memory_set, infop, child_pid, si_code, si_status)?;
            write_rusage(&mut inner.memory_set, rusage)?;

            if options & WNOWAIT == 0 {
                inner.cpu_times.add_waited_child(child_times);
                // CONTEXT: WNOWAIT observes the zombie without reaping it; only
                // the actual reap removes the PID from process lookup.
                remove_from_pid2process(child_pid);
                // CONTEXT: See sys_wait4(); waitid reaping has the same
                // user-visible boundary and must not assert on Arc ownership.
                drop(inner.children.remove(idx));
            }
            return Ok(0);
        }

        if options & WNOHANG != 0 {
            if !infop.is_null() {
                write_user_value_in_memory_set(
                    &mut inner.memory_set,
                    infop,
                    &LinuxSigInfo::default(),
                )?;
            }
            write_rusage(&mut inner.memory_set, rusage)?;
            return Ok(0);
        }
        // See sys_wait4(): the blocked state must be published before dropping
        // the child-list lock so exit-time wakeups cannot be missed.
        let (task, task_cx_ptr) = block_current_task_no_schedule();
        drop(inner);
        let interrupted = task_has_wait_interrupt_signal(&task, &process);
        if interrupted {
            wakeup_task(task);
        }
        drop(process);
        schedule(task_cx_ptr);
        if interrupted {
            return Err(SysError::EINTR);
        }
    }
}
