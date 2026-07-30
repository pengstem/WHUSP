use crate::sync::SleepRwLock;
use crate::uapi::errno::{Errno, KResult};
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;

const CMD_RESET: usize = 0;
const CMD_READ: usize = 1;
const CMD_WRITE: usize = 2;
const CMD_WRITE_INTERRUPTIBLE: usize = 3;
const CMD_STAT: usize = 4;
const CMD_TRY_READ: usize = 5;
const CMD_TRY_WRITE: usize = 6;

const TAG_WRITER: usize = 1;
const TAG_LATE_READER: usize = 2;

const STAT_ACTIVE_READERS: usize = 0;
const STAT_ACTIVE_WRITERS: usize = 1;
const STAT_MAX_ACTIVE_READERS: usize = 2;
const STAT_VIOLATIONS: usize = 3;
const STAT_COMPLETIONS: usize = 4;
const STAT_WRITER_SEQUENCE: usize = 5;
const STAT_LATE_READER_SEQUENCE: usize = 6;
const STAT_WAITING_READERS: usize = 7;
const STAT_WAITING_WRITERS: usize = 8;
const STAT_MAX_WAITERS: usize = 9;

lazy_static! {
    static ref PROBE_LOCK: SleepRwLock<usize> = SleepRwLock::new(0);
}

static ACTIVE_READERS: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_WRITERS: AtomicUsize = AtomicUsize::new(0);
static MAX_ACTIVE_READERS: AtomicUsize = AtomicUsize::new(0);
static VIOLATIONS: AtomicUsize = AtomicUsize::new(0);
static COMPLETIONS: AtomicUsize = AtomicUsize::new(0);
static ENTRY_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
static WRITER_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
static LATE_READER_SEQUENCE: AtomicUsize = AtomicUsize::new(usize::MAX);

fn update_max(target: &AtomicUsize, value: usize) {
    let mut current = target.load(Ordering::Relaxed);
    while value > current {
        match target.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

fn update_min(target: &AtomicUsize, value: usize) {
    let mut current = target.load(Ordering::Relaxed);
    while value < current {
        match target.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

fn hold_for(duration_us: usize) {
    let deadline = crate::timer::get_time_us().saturating_add(duration_us);
    while crate::timer::get_time_us() < deadline {
        crate::task::suspend_current_and_run_next();
    }
}

fn record_reader_entry(tag: usize) {
    let sequence = ENTRY_SEQUENCE.fetch_add(1, Ordering::AcqRel) + 1;
    let active = ACTIVE_READERS.fetch_add(1, Ordering::AcqRel) + 1;
    update_max(&MAX_ACTIVE_READERS, active);
    if ACTIVE_WRITERS.load(Ordering::Acquire) != 0 {
        VIOLATIONS.fetch_add(1, Ordering::Relaxed);
    }
    if tag == TAG_LATE_READER {
        update_min(&LATE_READER_SEQUENCE, sequence);
    }
}

fn record_writer_entry(tag: usize) {
    let sequence = ENTRY_SEQUENCE.fetch_add(1, Ordering::AcqRel) + 1;
    if ACTIVE_WRITERS.swap(1, Ordering::AcqRel) != 0 || ACTIVE_READERS.load(Ordering::Acquire) != 0
    {
        VIOLATIONS.fetch_add(1, Ordering::Relaxed);
    }
    if tag == TAG_WRITER {
        WRITER_SEQUENCE.store(sequence, Ordering::Release);
    }
}

fn run_reader(duration_us: usize, tag: usize) -> KResult {
    let _guard = PROBE_LOCK.read();
    record_reader_entry(tag);
    hold_for(duration_us);
    ACTIVE_READERS.fetch_sub(1, Ordering::AcqRel);
    COMPLETIONS.fetch_add(1, Ordering::Relaxed);
    Ok(0)
}

fn run_writer(duration_us: usize, tag: usize, interruptible: bool) -> KResult {
    if interruptible {
        let mut guard = PROBE_LOCK.write_interruptible().map_err(|_| Errno::EINTR)?;
        record_writer_entry(tag);
        *guard = guard.wrapping_add(1);
        hold_for(duration_us);
        ACTIVE_WRITERS.store(0, Ordering::Release);
    } else {
        let mut guard = PROBE_LOCK.write();
        record_writer_entry(tag);
        *guard = guard.wrapping_add(1);
        hold_for(duration_us);
        ACTIVE_WRITERS.store(0, Ordering::Release);
    }
    COMPLETIONS.fetch_add(1, Ordering::Relaxed);
    Ok(0)
}

fn reset() -> KResult {
    let Some(mut guard) = PROBE_LOCK.try_write() else {
        return Err(Errno::EBUSY);
    };
    *guard = 0;
    ACTIVE_READERS.store(0, Ordering::Relaxed);
    ACTIVE_WRITERS.store(0, Ordering::Relaxed);
    MAX_ACTIVE_READERS.store(0, Ordering::Relaxed);
    VIOLATIONS.store(0, Ordering::Relaxed);
    COMPLETIONS.store(0, Ordering::Relaxed);
    ENTRY_SEQUENCE.store(0, Ordering::Relaxed);
    WRITER_SEQUENCE.store(0, Ordering::Relaxed);
    LATE_READER_SEQUENCE.store(usize::MAX, Ordering::Relaxed);
    PROBE_LOCK.reset_max_waiters_for_probe();
    Ok(0)
}

fn stat(metric: usize) -> KResult {
    let lock = PROBE_LOCK.stats();
    let value = match metric {
        STAT_ACTIVE_READERS => ACTIVE_READERS.load(Ordering::Acquire),
        STAT_ACTIVE_WRITERS => ACTIVE_WRITERS.load(Ordering::Acquire),
        STAT_MAX_ACTIVE_READERS => MAX_ACTIVE_READERS.load(Ordering::Acquire),
        STAT_VIOLATIONS => VIOLATIONS.load(Ordering::Acquire),
        STAT_COMPLETIONS => COMPLETIONS.load(Ordering::Acquire),
        STAT_WRITER_SEQUENCE => WRITER_SEQUENCE.load(Ordering::Acquire),
        STAT_LATE_READER_SEQUENCE => LATE_READER_SEQUENCE.load(Ordering::Acquire),
        STAT_WAITING_READERS => lock.waiting_readers,
        STAT_WAITING_WRITERS => lock.waiting_writers,
        STAT_MAX_WAITERS => lock.max_waiters,
        _ => return Err(Errno::EINVAL),
    };
    isize::try_from(value).map_err(|_| Errno::EOVERFLOW)
}

pub(super) fn sys_fs4_sleep_rwlock_probe(command: usize, arg: usize, tag: usize) -> KResult {
    match command {
        CMD_RESET => reset(),
        CMD_READ => run_reader(arg, tag),
        CMD_WRITE => run_writer(arg, tag, false),
        CMD_WRITE_INTERRUPTIBLE => run_writer(arg, tag, true),
        CMD_STAT => stat(arg),
        CMD_TRY_READ => Ok(PROBE_LOCK.try_read().is_some() as isize),
        CMD_TRY_WRITE => Ok(PROBE_LOCK.try_write().is_some() as isize),
        _ => Err(Errno::EINVAL),
    }
}
