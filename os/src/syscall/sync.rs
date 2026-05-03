use crate::task::{
    TaskControlBlock, TaskStatus, block_current_and_run_next, block_current_task_no_schedule,
    current_process, current_task, current_user_token, schedule, wakeup_task,
};
use crate::timer::{add_timer, get_time_ms, monotonic_time_nanos, wall_time_nanos};

use super::errno::{SysError, SysResult};
use super::fs::LinuxTimeSpec;
use super::fs::user_ptr::{read_user_value, write_user_value};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;

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

const FUTEX_WAIT: u32 = 0;
const FUTEX_WAKE: u32 = 1;
const FUTEX_REQUEUE: u32 = 3;
const FUTEX_CMP_REQUEUE: u32 = 4;
const FUTEX_WAIT_BITSET: u32 = 9;
const FUTEX_WAKE_BITSET: u32 = 10;
const FUTEX_CMD_MASK: u32 = 0x7f;
const FUTEX_PRIVATE_FLAG: u32 = 0x80;
const FUTEX_CLOCK_REALTIME: u32 = 0x100;
const FUTEX_BITSET_MATCH_ANY: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FutexKey {
    process_id: usize,
    addr: usize,
}

struct FutexWaiter {
    task: Arc<TaskControlBlock>,
    bitset: u32,
}

struct FutexManager {
    waiters: BTreeMap<FutexKey, VecDeque<FutexWaiter>>,
}

impl FutexManager {
    fn new() -> Self {
        Self {
            waiters: BTreeMap::new(),
        }
    }

    fn remove_waiter(&mut self, key: FutexKey, task: &Arc<TaskControlBlock>) -> bool {
        let removed = {
            let Some(queue) = self.waiters.get_mut(&key) else {
                return false;
            };
            let old_len = queue.len();
            queue.retain(|waiter| !Arc::ptr_eq(&waiter.task, task));
            old_len != queue.len()
        };
        self.remove_empty_queue(key);
        removed
    }

    fn remove_waiter_any(&mut self, task: &Arc<TaskControlBlock>) -> bool {
        let mut removed = false;
        self.waiters.retain(|_, queue| {
            let old_len = queue.len();
            queue.retain(|waiter| !Arc::ptr_eq(&waiter.task, task));
            removed |= old_len != queue.len();
            !queue.is_empty()
        });
        removed
    }

    fn wake(&mut self, key: FutexKey, limit: usize, bitset: u32) -> Vec<Arc<TaskControlBlock>> {
        let Some(queue) = self.waiters.get_mut(&key) else {
            return Vec::new();
        };
        let mut tasks = Vec::new();
        let mut kept = VecDeque::new();
        while let Some(waiter) = queue.pop_front() {
            if !waiter.is_blocked() {
                continue;
            }
            if waiter.bitset & bitset != 0 && tasks.len() < limit {
                tasks.push(waiter.task);
            } else {
                kept.push_back(waiter);
            }
        }
        *queue = kept;
        self.remove_empty_queue(key);
        tasks
    }

    fn requeue(
        &mut self,
        source: FutexKey,
        target: FutexKey,
        wake_limit: usize,
        requeue_limit: usize,
    ) -> (Vec<Arc<TaskControlBlock>>, usize) {
        let Some(queue) = self.waiters.get_mut(&source) else {
            return (Vec::new(), 0);
        };
        let mut tasks = Vec::new();
        let mut moved = VecDeque::new();
        let mut kept = VecDeque::new();
        while let Some(waiter) = queue.pop_front() {
            if !waiter.is_blocked() {
                continue;
            }
            if tasks.len() < wake_limit {
                tasks.push(waiter.task);
            } else if moved.len() < requeue_limit {
                moved.push_back(waiter);
            } else {
                kept.push_back(waiter);
            }
        }
        *queue = kept;
        self.remove_empty_queue(source);
        let moved_len = moved.len();
        if moved_len > 0 {
            self.waiters.entry(target).or_default().extend(moved);
        }
        (tasks, moved_len)
    }

    fn remove_process(&mut self, process_id: usize) {
        self.waiters.retain(|key, queue| {
            if key.process_id == process_id {
                return false;
            }
            queue.retain(|waiter| {
                waiter.is_blocked()
                    && waiter
                        .task
                        .process
                        .upgrade()
                        .is_some_and(|process| process.getpid() != process_id)
            });
            !queue.is_empty()
        });
    }

    fn remove_empty_queue(&mut self, key: FutexKey) {
        if matches!(self.waiters.get(&key), Some(queue) if queue.is_empty()) {
            self.waiters.remove(&key);
        }
    }
}

impl FutexWaiter {
    fn is_blocked(&self) -> bool {
        self.task.inner_exclusive_access().task_status == TaskStatus::Blocked
    }
}

lazy_static! {
    static ref FUTEX_MANAGER: crate::sync::UPIntrFreeCell<FutexManager> =
        unsafe { crate::sync::UPIntrFreeCell::new(FutexManager::new()) };
}

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

fn futex_key(addr: usize, private: bool) -> FutexKey {
    // UNFINISHED: Shared futex keys should be derived from the backing physical
    // object so unrelated processes can synchronize through shared mappings.
    // libctest and musl pthread paths exercised here use process-private
    // futexes, so virtual-address keys are sufficient for this compatibility
    // subset.
    futex_key_for_process(addr, private, current_process().getpid())
}

fn futex_key_for_process(addr: usize, private: bool, process_id: usize) -> FutexKey {
    FutexKey {
        process_id: if private { process_id } else { 0 },
        addr,
    }
}

fn validate_futex_addr(addr: usize) -> SysResult {
    if addr % core::mem::size_of::<u32>() != 0 {
        return Err(SysError::EINVAL);
    }
    Ok(0)
}

fn read_futex_word(addr: usize) -> SysResult<u32> {
    validate_futex_addr(addr)?;
    read_user_value(current_user_token(), addr as *const u32)
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

fn futex_timeout_absolute(
    timeout: *const LinuxTimeSpec,
    backend: ClockBackend,
) -> SysResult<Option<usize>> {
    if timeout.is_null() {
        return Ok(None);
    }
    let request = validate_timespec(read_user_value(current_user_token(), timeout)?)?;
    let deadline_nanos = timespec_to_nanos(request)?;
    let now_nanos = current_clock_nanos(backend);
    if deadline_nanos <= now_nanos {
        return Ok(Some(get_time_ms()));
    }
    let duration_ms = nanos_to_ms_ceil(deadline_nanos - now_nanos)?;
    Ok(Some(
        get_time_ms()
            .checked_add(duration_ms)
            .ok_or(SysError::EINVAL)?,
    ))
}

fn futex_timeout_expired(timeout_ms: Option<usize>) -> bool {
    matches!(timeout_ms, Some(deadline_ms) if deadline_ms <= get_time_ms())
}

fn futex_wait(
    addr: usize,
    private: bool,
    expected: u32,
    timeout_ms: Option<usize>,
    bitset: u32,
) -> SysResult {
    let key = futex_key(addr, private);
    let task = current_task().unwrap();

    let task_cx_ptr = {
        let mut manager = FUTEX_MANAGER.exclusive_access();
        if read_futex_word(addr)? != expected {
            return Err(SysError::EAGAIN);
        }
        if futex_timeout_expired(timeout_ms) {
            return Err(SysError::ETIMEDOUT);
        }
        let (blocked_task, task_cx_ptr) = block_current_task_no_schedule();
        manager
            .waiters
            .entry(key)
            .or_default()
            .push_back(FutexWaiter {
                task: blocked_task,
                bitset,
            });
        task_cx_ptr
    };

    if let Some(deadline_ms) = timeout_ms {
        add_timer(deadline_ms, Arc::clone(&task));
    }
    schedule(task_cx_ptr);

    if futex_timeout_expired(timeout_ms) {
        let mut manager = FUTEX_MANAGER.exclusive_access();
        if manager.remove_waiter(key, &task) || manager.remove_waiter_any(&task) {
            return Err(SysError::ETIMEDOUT);
        }
    }
    Ok(0)
}

fn futex_wake(addr: usize, private: bool, limit: usize, bitset: u32) -> usize {
    futex_wake_for_process(addr, private, current_process().getpid(), limit, bitset)
}

fn futex_wake_for_process(
    addr: usize,
    private: bool,
    process_id: usize,
    limit: usize,
    bitset: u32,
) -> usize {
    let key = futex_key_for_process(addr, private, process_id);
    let tasks = FUTEX_MANAGER.exclusive_access().wake(key, limit, bitset);
    let mut count = 0;
    for task in tasks {
        if wakeup_task(task) {
            count += 1;
        }
    }
    count
}

pub(crate) fn clear_child_tid_and_wake(token: usize, process_id: usize, addr: usize) {
    if addr == 0 {
        return;
    }
    if write_user_value(token, addr as *mut i32, &0).is_err() {
        return;
    }
    let _ = futex_wake_for_process(addr, false, process_id, 1, FUTEX_BITSET_MATCH_ANY);
    // CONTEXT: Linux specifies FUTEX_WAKE without FUTEX_PRIVATE_FLAG for
    // clear_child_tid. This kernel keeps private and shared futex wait queues
    // separate, so also wake the process-private key used by common libc paths.
    let _ = futex_wake_for_process(addr, true, process_id, 1, FUTEX_BITSET_MATCH_ANY);
}

fn futex_requeue(
    addr: usize,
    private: bool,
    wake_limit: usize,
    requeue_limit: usize,
    addr2: usize,
    count_requeued: bool,
) -> SysResult<usize> {
    validate_futex_addr(addr2)?;
    let source = futex_key(addr, private);
    let target = futex_key(addr2, private);
    if source == target {
        return Err(SysError::EINVAL);
    }
    let (tasks, moved) =
        FUTEX_MANAGER
            .exclusive_access()
            .requeue(source, target, wake_limit, requeue_limit);
    let mut count = if count_requeued { moved } else { 0 };
    for task in tasks {
        if wakeup_task(task) {
            count += 1;
        }
    }
    Ok(count)
}

pub(crate) fn remove_process_futex_waiters(process_id: usize) {
    FUTEX_MANAGER.exclusive_access().remove_process(process_id);
}

fn futex_count(raw: usize) -> SysResult<usize> {
    if raw > i32::MAX as usize {
        return Err(SysError::EINVAL);
    }
    Ok(raw)
}

fn validate_futex_clock_option(command: u32, futex_op: u32) -> SysResult<()> {
    if futex_op & FUTEX_CLOCK_REALTIME == 0 {
        return Ok(());
    }
    match command {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => Ok(()),
        // UNFINISHED: FUTEX_WAIT_REQUEUE_PI and FUTEX_LOCK_PI2 also accept
        // FUTEX_CLOCK_REALTIME on Linux, but PI futexes are outside the
        // classic pthread mutex/cond/join subset implemented here.
        _ => Err(SysError::ENOSYS),
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

pub fn sys_futex(
    uaddr: *mut u32,
    futex_op: u32,
    val: u32,
    timeout: *const LinuxTimeSpec,
    uaddr2: *mut u32,
    val3: u32,
) -> SysResult {
    let addr = uaddr as usize;
    validate_futex_addr(addr)?;
    if futex_op & !(FUTEX_CMD_MASK | FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME) != 0 {
        return Err(SysError::EINVAL);
    }

    let command = futex_op & FUTEX_CMD_MASK;
    validate_futex_clock_option(command, futex_op)?;
    if matches!(command, FUTEX_WAIT_BITSET | FUTEX_WAKE_BITSET) && val3 == 0 {
        return Err(SysError::EINVAL);
    }
    let private = futex_op & FUTEX_PRIVATE_FLAG != 0;
    let clock_backend = if futex_op & FUTEX_CLOCK_REALTIME != 0 {
        ClockBackend::Wall
    } else {
        ClockBackend::Monotonic
    };

    match command {
        FUTEX_WAIT => futex_wait(
            addr,
            private,
            val,
            relative_timeout_deadline_ms(current_user_token(), timeout)?,
            FUTEX_BITSET_MATCH_ANY,
        ),
        FUTEX_WAIT_BITSET => futex_wait(
            addr,
            private,
            val,
            futex_timeout_absolute(timeout, clock_backend)?,
            val3,
        ),
        FUTEX_WAKE => Ok(futex_wake(
            addr,
            private,
            futex_count(val as usize)?,
            FUTEX_BITSET_MATCH_ANY,
        ) as isize),
        FUTEX_WAKE_BITSET => {
            Ok(futex_wake(addr, private, futex_count(val as usize)?, val3) as isize)
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            let addr2 = uaddr2 as usize;
            // CONTEXT: FUTEX_REQUEUE/CMP_REQUEUE use the fourth syscall
            // register as val2, but syscall dispatch has already cast that
            // register to this pointer-typed timeout parameter.
            let requeue_limit = futex_count(timeout as usize)?;
            if command == FUTEX_CMP_REQUEUE && read_futex_word(addr)? != val3 {
                return Err(SysError::EAGAIN);
            }
            futex_requeue(
                addr,
                private,
                futex_count(val as usize)?,
                requeue_limit,
                addr2,
                command == FUTEX_CMP_REQUEUE,
            )
            .map(|count| count as isize)
        }
        // UNFINISHED: PI futexes, FUTEX_WAKE_OP, and futex_waitv are not
        // implemented. The libctest pthread and loader paths need the classic
        // wait/wake and requeue subset above.
        _ => Err(SysError::ENOSYS),
    }
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
