use crate::mm::MemorySet;
use crate::perf;
use crate::syscall::SyscallContext;
use crate::syscall::user_ptr::{write_user_value, write_user_value_in_memory_set};
use crate::task::{
    CLD_CONTINUED, CLD_STOPPED, ProcessControlBlock, ProcessCpuTimesSnapshot, SIGCHLD, SIGCONT,
    SignalInfo, block_current_task_no_schedule, current_process, current_task, current_user_token,
    pid2process, ptrace_take_wait_status, remove_from_pid2process, schedule, signal_child_status,
    signal_wait_status, task_has_wait_interrupt_signal, wakeup_task,
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
const RUSAGE_CHILDREN: i32 = -1;
const RUSAGE_SELF: i32 = 0;
const RUSAGE_THREAD: i32 = 1;
const USEC_PER_SEC: usize = 1_000_000;

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
    pub(crate) si_signo: i32,
    pub(crate) si_errno: i32,
    pub(crate) si_code: i32,
    pub(crate) si_trapno: i32,
    pub(crate) si_pid: i32,
    pub(crate) si_uid: u32,
    pub(crate) si_status: i32,
    pub(crate) si_utime: u32,
    pub(crate) si_stime: u32,
    pub(crate) si_value: u64,
    pub(crate) pad: [u32; 20],
    pub(crate) align: [u64; 0],
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
        // UNFINISHED: Linux fills child resource usage here. This kernel only
        // accounts waited-child CPU time internally for times(2), so wait4()
        // and waitid() still expose a zeroed rusage structure.
        write_user_value_in_memory_set(memory_set, rusage, &RUsage::default())?;
    }
    Ok(())
}

fn usize_to_isize_saturating(value: usize) -> isize {
    value.min(isize::MAX as usize) as isize
}

fn timeval_from_us(us: usize) -> TimeVal {
    TimeVal {
        sec: usize_to_isize_saturating(us / USEC_PER_SEC),
        usec: usize_to_isize_saturating(us % USEC_PER_SEC),
    }
}

fn process_self_rusage() -> RUsage {
    let times = current_process().cpu_times_snapshot();
    RUsage {
        utime: timeval_from_us(times.user_us),
        stime: timeval_from_us(times.system_us),
        maxrss: usize_to_isize_saturating(times.self_maxrss_kb),
        ..RUsage::default()
    }
}

fn process_children_rusage() -> RUsage {
    let times = current_process().cpu_times_snapshot();
    RUsage {
        utime: timeval_from_us(times.children_user_us),
        stime: timeval_from_us(times.children_system_us),
        maxrss: usize_to_isize_saturating(times.children_maxrss_kb),
        ..RUsage::default()
    }
}

fn current_thread_rusage() -> RUsage {
    let thread_cpu_us = current_task().map_or(0, |task| task.cpu_time_us());
    let mut usage = process_self_rusage();
    // UNFINISHED: Per-thread accounting currently records combined CPU time,
    // so RUSAGE_THREAD exposes it as user time until scheduler-grade user vs.
    // system attribution is available.
    usage.utime = timeval_from_us(thread_cpu_us);
    usage.stime = TimeVal::default();
    usage
}

pub fn sys_getrusage(who: i32, usage: *mut RUsage) -> SysResult {
    let rusage = match who {
        RUSAGE_SELF => process_self_rusage(),
        RUSAGE_CHILDREN => process_children_rusage(),
        RUSAGE_THREAD => current_thread_rusage(),
        _ => return Err(SysError::EINVAL),
    };
    write_user_value(current_user_token(), usage, &rusage)?;
    Ok(0)
}

fn ptrace_wait4_target(pid: isize, waiter_pid: usize) -> Option<(usize, i32)> {
    if pid <= 0 {
        return None;
    }
    let tracee = pid2process(pid as usize)?;
    let status = ptrace_take_wait_status(&tracee, waiter_pid, false)?;
    Some((tracee.getpid(), status))
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

struct WaitZombie {
    // Index into the caller's locked children slice. It is valid only until
    // the parent PCB lock is released, so reap paths must remove the child
    // before dropping that lock.
    idx: usize,
    pid: usize,
    exit_code: i32,
    child_times: ProcessCpuTimesSnapshot,
}

struct Wait4ChildScan {
    matched: bool,
    stopped: Option<(usize, i32)>,
    zombie: Option<WaitZombie>,
}

fn scan_wait4_children(
    children: &[Arc<ProcessControlBlock>],
    pid: isize,
    caller_pgid: usize,
    waiter_pid: usize,
    include_untraced: bool,
) -> Wait4ChildScan {
    // Keep stop and zombie discovery in one parent-child-list pass. Ptrace
    // stop status is observable without reaping, while the zombie record
    // carries the removal index for the later wait4 reap boundary.
    perf::record_wait_child_scan(children.len());
    let mut scan = Wait4ChildScan {
        matched: false,
        stopped: None,
        zombie: None,
    };
    for (idx, child) in children.iter().enumerate() {
        if !wait4_child_matches(child, pid, caller_pgid) {
            continue;
        }
        scan.matched = true;
        if scan.stopped.is_none()
            && let Some(status) = ptrace_take_wait_status(child, waiter_pid, include_untraced)
        {
            scan.stopped = Some((child.getpid(), status));
        }
        if scan.zombie.is_none() {
            let child_inner = child.inner_exclusive_access();
            if child_inner.is_zombie {
                scan.zombie = Some(WaitZombie {
                    idx,
                    pid: child.getpid(),
                    exit_code: child_inner.exit_code,
                    child_times: child_inner.cpu_times.snapshot(),
                });
            }
        }
    }
    scan
}

struct WaitidChildScan {
    matched: bool,
    stopped: Option<(usize, usize, i32)>,
    continued: Option<(usize, usize)>,
    zombie: Option<WaitZombie>,
}

fn scan_waitid_children(
    children: &[Arc<ProcessControlBlock>],
    idtype: i32,
    id: i32,
    caller_pgid: usize,
    options: i32,
) -> WaitidChildScan {
    // waitid distinguishes stopped, continued, and exited observations. Record
    // only the first matching state of each kind so WNOWAIT can observe without
    // consuming the wrong child-list entry.
    perf::record_wait_child_scan(children.len());
    let mut scan = WaitidChildScan {
        matched: false,
        stopped: None,
        continued: None,
        zombie: None,
    };
    for (idx, child) in children.iter().enumerate() {
        if !waitid_child_matches(child, idtype, id, caller_pgid) {
            continue;
        }
        scan.matched = true;
        let child_inner = child.inner_exclusive_access();
        if scan.stopped.is_none()
            && options & WUNTRACED != 0
            && let Some(status) = child_inner.wait_stop_status
        {
            scan.stopped = Some((idx, child.getpid(), status));
        }
        if scan.continued.is_none() && options & WCONTINUED != 0 && child_inner.wait_continued {
            scan.continued = Some((idx, child.getpid()));
        }
        if scan.zombie.is_none() && options & WEXITED != 0 && child_inner.is_zombie {
            scan.zombie = Some(WaitZombie {
                idx,
                pid: child.getpid(),
                exit_code: child_inner.exit_code,
                child_times: child_inner.cpu_times.snapshot(),
            });
        }
    }
    scan
}

pub fn sys_wait4_ctx(
    ctx: &SyscallContext,
    pid: isize,
    wstatus: *mut i32,
    options: i32,
    rusage: *mut RUsage,
) -> SysResult {
    sys_wait4_for_process(ctx.process().clone(), pid, wstatus, options, rusage)
}

fn sys_wait4_for_process(
    process: Arc<ProcessControlBlock>,
    pid: isize,
    wstatus: *mut i32,
    options: i32,
    rusage: *mut RUsage,
) -> SysResult {
    // CONTEXT: __WALL is accepted for ptrace/LTP compatibility. This process
    // model stores waitable children in one process child list, so there is no
    // separate thread-vs-process wait domain for the flag to widen.
    if options < 0 || options & !(WNOHANG | WUNTRACED | WCONTINUED | WALL) != 0 {
        return Err(SysError::EINVAL);
    }
    if pid == i32::MIN as isize {
        // CONTEXT: Linux rejects INT_MIN before treating negative pid values as
        // process-group selectors because abs(INT_MIN) cannot be represented.
        return Err(SysError::ESRCH);
    }

    loop {
        let waiter_pid = process.getpid();
        let caller_pgid = process.process_group_id();
        let mut inner = process.inner_exclusive_access();
        let scan = scan_wait4_children(
            &inner.children,
            pid,
            caller_pgid,
            waiter_pid,
            options & WUNTRACED != 0,
        );
        if !scan.matched {
            if let Some((tracee_pid, status)) = ptrace_wait4_target(pid, waiter_pid) {
                if !wstatus.is_null() {
                    write_user_value_in_memory_set(&mut inner.memory_set, wstatus, &status)?;
                }
                write_rusage(&mut inner.memory_set, rusage)?;
                return Ok(tracee_pid as isize);
            }
            return Err(SysError::ECHILD);
        }

        if let Some((found_pid, status)) = scan.stopped {
            if !wstatus.is_null() {
                write_user_value_in_memory_set(&mut inner.memory_set, wstatus, &status)?;
            }
            write_rusage(&mut inner.memory_set, rusage)?;
            return Ok(found_pid as isize);
        }

        if let Some(zombie) = scan.zombie {
            if !wstatus.is_null() {
                write_user_value_in_memory_set(
                    &mut inner.memory_set,
                    wstatus,
                    &wait_status(zombie.exit_code),
                )?;
            }
            write_rusage(&mut inner.memory_set, rusage)?;
            inner.cpu_times.add_waited_child(zombie.child_times);

            // CONTEXT: Linux keeps a zombie PID visible until the parent reaps it.
            // Remove the process from PID lookup only at the wait/reap boundary.
            remove_from_pid2process(zombie.pid);
            // CONTEXT: Reaping completes when the child is removed from both
            // PID lookup and the parent's child list. Other internal kernel
            // references must not turn a successful wait into a panic.
            drop(inner.children.remove(zombie.idx));
            return Ok(zombie.pid as isize);
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
        let scan = scan_waitid_children(&inner.children, idtype, id, caller_pgid, options);
        if !scan.matched {
            return Err(SysError::ECHILD);
        }

        if let Some((idx, child_pid, stop_status)) = scan.stopped {
            write_waitid_siginfo(
                &mut inner.memory_set,
                infop,
                child_pid,
                CLD_STOPPED,
                stop_status,
            )?;
            write_rusage(&mut inner.memory_set, rusage)?;
            if options & WNOWAIT == 0
                && let Some(child) = inner.children.get(idx)
            {
                child.inner_exclusive_access().wait_stop_status = None;
            }
            return Ok(0);
        }

        if let Some((idx, child_pid)) = scan.continued {
            write_waitid_siginfo(
                &mut inner.memory_set,
                infop,
                child_pid,
                CLD_CONTINUED,
                SIGCONT as i32,
            )?;
            write_rusage(&mut inner.memory_set, rusage)?;
            if options & WNOWAIT == 0
                && let Some(child) = inner.children.get(idx)
            {
                child.inner_exclusive_access().wait_continued = false;
            }
            return Ok(0);
        }

        if let Some(zombie) = scan.zombie {
            let (si_code, si_status) = waitid_code_and_status(zombie.exit_code);
            write_waitid_siginfo(&mut inner.memory_set, infop, zombie.pid, si_code, si_status)?;
            write_rusage(&mut inner.memory_set, rusage)?;

            if options & WNOWAIT == 0 {
                inner.cpu_times.add_waited_child(zombie.child_times);
                // CONTEXT: WNOWAIT observes the zombie without reaping it; only
                // the actual reap removes the PID from process lookup.
                remove_from_pid2process(zombie.pid);
                // CONTEXT: waitid reaping has the same user-visible boundary
                // as wait4 and must not assert on Arc ownership.
                drop(inner.children.remove(zombie.idx));
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
        // The blocked state must be published before dropping the child-list
        // lock so exit-time wakeups cannot be missed.
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
