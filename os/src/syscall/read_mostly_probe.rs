use crate::sync::{ReadMostlySnapshot, SleepMutex};
use crate::uapi::errno::{Errno, KResult};
use core::hint::spin_loop;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;

const CMD_RESET: usize = 0;
const CMD_READ: usize = 1;
const CMD_PUBLISH: usize = 2;
const CMD_STAT: usize = 3;
const CMD_NESTED_READ: usize = 4;
const CMD_GATED_READ: usize = 5;
const CMD_GATED_NESTED_READ: usize = 6;
const CMD_RELEASE_READERS: usize = 7;

const STAT_ACTIVE_READERS: usize = 0;
const STAT_MAX_ACTIVE_READERS: usize = 1;
const STAT_READ_COMPLETIONS: usize = 2;
const STAT_PUBLISH_COMPLETIONS: usize = 3;
const STAT_CURRENT_VALUE: usize = 4;
const STAT_NESTED_INNER_DONE: usize = 5;
const STAT_VIOLATIONS: usize = 6;
const STAT_PUBLISH_ATTEMPTS: usize = 7;

lazy_static! {
    static ref PROBE_SNAPSHOT: ReadMostlySnapshot<usize> = ReadMostlySnapshot::new(0);
    static ref PROBE_WRITER: SleepMutex<()> = SleepMutex::new(());
}

static ACTIVE_READERS: AtomicUsize = AtomicUsize::new(0);
static MAX_ACTIVE_READERS: AtomicUsize = AtomicUsize::new(0);
static READ_COMPLETIONS: AtomicUsize = AtomicUsize::new(0);
static PUBLISH_COMPLETIONS: AtomicUsize = AtomicUsize::new(0);
static NESTED_INNER_DONE: AtomicUsize = AtomicUsize::new(0);
static VIOLATIONS: AtomicUsize = AtomicUsize::new(0);
static PUBLISH_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
static RELEASE_READERS: AtomicUsize = AtomicUsize::new(0);

fn update_max(target: &AtomicUsize, value: usize) {
    let mut current = target.load(Ordering::Relaxed);
    while value > current {
        match target.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

fn hold_without_scheduling(duration_us: usize) {
    let deadline = crate::timer::get_time_us().saturating_add(duration_us);
    while crate::timer::get_time_us() < deadline {
        crate::cpu::handle_remote_sync_ipi();
        spin_loop();
    }
}

fn hold_until_released() {
    while RELEASE_READERS.load(Ordering::Acquire) == 0 {
        crate::cpu::handle_remote_sync_ipi();
        spin_loop();
    }
}

fn enter_reader() {
    let active = ACTIVE_READERS.fetch_add(1, Ordering::AcqRel) + 1;
    update_max(&MAX_ACTIVE_READERS, active);
}

fn exit_reader() {
    let previous = ACTIVE_READERS.fetch_sub(1, Ordering::AcqRel);
    if previous == 0 {
        VIOLATIONS.fetch_add(1, Ordering::Relaxed);
    }
    READ_COMPLETIONS.fetch_add(1, Ordering::Relaxed);
}

fn run_reader(duration_us: usize) -> KResult {
    let value = PROBE_SNAPSHOT.read(|snapshot| {
        enter_reader();
        let value = *snapshot;
        hold_without_scheduling(duration_us);
        if *snapshot != value {
            VIOLATIONS.fetch_add(1, Ordering::Relaxed);
        }
        exit_reader();
        value
    });
    isize::try_from(value).map_err(|_| Errno::EOVERFLOW)
}

fn run_gated_reader() -> KResult {
    let value = PROBE_SNAPSHOT.read(|snapshot| {
        enter_reader();
        let value = *snapshot;
        hold_until_released();
        if *snapshot != value {
            VIOLATIONS.fetch_add(1, Ordering::Relaxed);
        }
        exit_reader();
        value
    });
    isize::try_from(value).map_err(|_| Errno::EOVERFLOW)
}

fn run_nested_reader(duration_us: usize) -> KResult {
    let value = PROBE_SNAPSHOT.read(|outer| {
        enter_reader();
        let value = *outer;
        let inner = PROBE_SNAPSHOT.read(|inner| *inner);
        if inner != value {
            VIOLATIONS.fetch_add(1, Ordering::Relaxed);
        }
        NESTED_INNER_DONE.store(1, Ordering::Release);
        hold_without_scheduling(duration_us);
        if *outer != value {
            VIOLATIONS.fetch_add(1, Ordering::Relaxed);
        }
        exit_reader();
        value
    });
    isize::try_from(value).map_err(|_| Errno::EOVERFLOW)
}

fn run_gated_nested_reader() -> KResult {
    let value = PROBE_SNAPSHOT.read(|outer| {
        enter_reader();
        let value = *outer;
        let inner = PROBE_SNAPSHOT.read(|inner| *inner);
        if inner != value {
            VIOLATIONS.fetch_add(1, Ordering::Relaxed);
        }
        NESTED_INNER_DONE.store(1, Ordering::Release);
        hold_until_released();
        if *outer != value {
            VIOLATIONS.fetch_add(1, Ordering::Relaxed);
        }
        exit_reader();
        value
    });
    isize::try_from(value).map_err(|_| Errno::EOVERFLOW)
}

fn publish(value: usize) -> KResult {
    let _writer = PROBE_WRITER.lock();
    PUBLISH_ATTEMPTS.fetch_add(1, Ordering::Release);
    PROBE_SNAPSHOT.publish(value);
    if ACTIVE_READERS.load(Ordering::Acquire) != 0 {
        VIOLATIONS.fetch_add(1, Ordering::Relaxed);
    }
    PUBLISH_COMPLETIONS.fetch_add(1, Ordering::Release);
    Ok(0)
}

fn reset() -> KResult {
    let _writer = PROBE_WRITER.lock();
    PROBE_SNAPSHOT.publish(0);
    ACTIVE_READERS.store(0, Ordering::Relaxed);
    MAX_ACTIVE_READERS.store(0, Ordering::Relaxed);
    READ_COMPLETIONS.store(0, Ordering::Relaxed);
    PUBLISH_COMPLETIONS.store(0, Ordering::Relaxed);
    NESTED_INNER_DONE.store(0, Ordering::Relaxed);
    VIOLATIONS.store(0, Ordering::Relaxed);
    PUBLISH_ATTEMPTS.store(0, Ordering::Relaxed);
    RELEASE_READERS.store(0, Ordering::Relaxed);
    Ok(0)
}

fn stat(metric: usize) -> KResult {
    let value = match metric {
        STAT_ACTIVE_READERS => ACTIVE_READERS.load(Ordering::Acquire),
        STAT_MAX_ACTIVE_READERS => MAX_ACTIVE_READERS.load(Ordering::Acquire),
        STAT_READ_COMPLETIONS => READ_COMPLETIONS.load(Ordering::Acquire),
        STAT_PUBLISH_COMPLETIONS => PUBLISH_COMPLETIONS.load(Ordering::Acquire),
        STAT_CURRENT_VALUE => PROBE_SNAPSHOT.read(|value| *value),
        STAT_NESTED_INNER_DONE => NESTED_INNER_DONE.load(Ordering::Acquire),
        STAT_VIOLATIONS => VIOLATIONS.load(Ordering::Acquire),
        STAT_PUBLISH_ATTEMPTS => PUBLISH_ATTEMPTS.load(Ordering::Acquire),
        _ => return Err(Errno::EINVAL),
    };
    isize::try_from(value).map_err(|_| Errno::EOVERFLOW)
}

pub(super) fn sys_read_mostly_probe(command: usize, arg: usize) -> KResult {
    match command {
        CMD_RESET => reset(),
        CMD_READ => run_reader(arg),
        CMD_PUBLISH => publish(arg),
        CMD_STAT => stat(arg),
        CMD_NESTED_READ => run_nested_reader(arg),
        CMD_GATED_READ => run_gated_reader(),
        CMD_GATED_NESTED_READ => run_gated_nested_reader(),
        CMD_RELEASE_READERS => {
            RELEASE_READERS.store(1, Ordering::Release);
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}
