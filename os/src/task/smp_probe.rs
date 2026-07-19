use crate::config::MAX_CPUS;
use core::sync::atomic::{AtomicUsize, Ordering};
use log::info;

pub(crate) const WORKERS: usize = 8;
pub(crate) const CYCLES: usize = 128;

static RUNS: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static YIELD_SYSCALLS: AtomicUsize = AtomicUsize::new(0);
static BLOCKS: AtomicUsize = AtomicUsize::new(0);
static PENDING_WAKES: AtomicUsize = AtomicUsize::new(0);
static DIRECT_WAKES: AtomicUsize = AtomicUsize::new(0);
static EXITS: AtomicUsize = AtomicUsize::new(0);

pub(super) fn record_run(cpu: usize) {
    RUNS[cpu].fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_yield_syscall() {
    let Some(task) = super::current_task() else {
        return;
    };
    if task.is_smp_sched_probe_active() {
        YIELD_SYSCALLS.fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) fn record_block() {
    BLOCKS.fetch_add(1, Ordering::Relaxed);
}

pub(super) fn record_wake(pending: bool) {
    if pending {
        PENDING_WAKES.fetch_add(1, Ordering::Relaxed);
    } else {
        DIRECT_WAKES.fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) fn record_exit() {
    let exits = EXITS.fetch_add(1, Ordering::AcqRel) + 1;
    assert!(exits <= WORKERS, "too many Phase 3 scheduler-probe exits");
    if exits != WORKERS {
        return;
    }
    let runs = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| RUNS[cpu].load(Ordering::Acquire));
    info!(
        "smp sched-life: workers={} cycles={} runs={:?} yield_syscalls={} blocks={} pending_wakes={} direct_wakes={} exits={}",
        WORKERS,
        CYCLES,
        &runs[..crate::cpu::topology().possible_count()],
        YIELD_SYSCALLS.load(Ordering::Acquire),
        BLOCKS.load(Ordering::Acquire),
        PENDING_WAKES.load(Ordering::Acquire),
        DIRECT_WAKES.load(Ordering::Acquire),
        exits,
    );
}
