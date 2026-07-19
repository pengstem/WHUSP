use crate::config::MAX_CPUS;
use core::{
    hint::spin_loop,
    sync::atomic::{AtomicUsize, Ordering},
};
use log::info;

pub(crate) const WORKERS: usize = 8;
pub(crate) const CYCLES: usize = 128;

static RUNS: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static YIELD_SYSCALLS: AtomicUsize = AtomicUsize::new(0);
static BLOCKS: AtomicUsize = AtomicUsize::new(0);
static PENDING_WAKES: AtomicUsize = AtomicUsize::new(0);
static DIRECT_WAKES: AtomicUsize = AtomicUsize::new(0);
static EXITS: AtomicUsize = AtomicUsize::new(0);

const CPU_PROBE_INACTIVE: usize = 0;
const CPU_PROBE_INITIALIZING: usize = 1;
const CPU_PROBE_ACTIVE: usize = 2;

#[repr(C, align(64))]
struct CpuProbeCounters {
    context_switches: AtomicUsize,
    local_wakes: AtomicUsize,
    remote_wakes: AtomicUsize,
    reschedule_ipis: AtomicUsize,
    run_queue_accesses: AtomicUsize,
    run_queue_wait_ticks: AtomicUsize,
    run_queue_hold_ticks: AtomicUsize,
    runnable_us: AtomicUsize,
}

impl CpuProbeCounters {
    const fn new() -> Self {
        Self {
            context_switches: AtomicUsize::new(0),
            local_wakes: AtomicUsize::new(0),
            remote_wakes: AtomicUsize::new(0),
            reschedule_ipis: AtomicUsize::new(0),
            run_queue_accesses: AtomicUsize::new(0),
            run_queue_wait_ticks: AtomicUsize::new(0),
            run_queue_hold_ticks: AtomicUsize::new(0),
            runnable_us: AtomicUsize::new(0),
        }
    }

    fn reset(&self) {
        self.context_switches.store(0, Ordering::Relaxed);
        self.local_wakes.store(0, Ordering::Relaxed);
        self.remote_wakes.store(0, Ordering::Relaxed);
        self.reschedule_ipis.store(0, Ordering::Relaxed);
        self.run_queue_accesses.store(0, Ordering::Relaxed);
        self.run_queue_wait_ticks.store(0, Ordering::Relaxed);
        self.run_queue_hold_ticks.store(0, Ordering::Relaxed);
        self.runnable_us.store(0, Ordering::Relaxed);
    }
}

static CPU_PROBE_STATE: AtomicUsize = AtomicUsize::new(CPU_PROBE_INACTIVE);
static CPU_PROBE_SAMPLE: AtomicUsize = AtomicUsize::new(0);
static CPU_PROBE_STARTS: AtomicUsize = AtomicUsize::new(0);
static CPU_PROBE_EXITS: AtomicUsize = AtomicUsize::new(0);
static CPU_PROBE_COUNTERS: [CpuProbeCounters; MAX_CPUS] =
    [const { CpuProbeCounters::new() }; MAX_CPUS];

static WAIT_IO_STATE: AtomicUsize = AtomicUsize::new(CPU_PROBE_INACTIVE);
static WAIT_IO_STARTS: AtomicUsize = AtomicUsize::new(0);
static WAIT_IO_EXITS: AtomicUsize = AtomicUsize::new(0);
static WAIT_IO_RUNS: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];

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

pub(crate) fn start_cpu_probe() {
    loop {
        match CPU_PROBE_STATE.load(Ordering::Acquire) {
            CPU_PROBE_INACTIVE => {
                if CPU_PROBE_STATE
                    .compare_exchange(
                        CPU_PROBE_INACTIVE,
                        CPU_PROBE_INITIALIZING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    continue;
                }
                for counters in &CPU_PROBE_COUNTERS {
                    counters.reset();
                }
                CPU_PROBE_STARTS.store(1, Ordering::Relaxed);
                CPU_PROBE_EXITS.store(0, Ordering::Relaxed);
                CPU_PROBE_SAMPLE.fetch_add(1, Ordering::Relaxed);
                CPU_PROBE_STATE.store(CPU_PROBE_ACTIVE, Ordering::Release);
                return;
            }
            CPU_PROBE_INITIALIZING => spin_loop(),
            CPU_PROBE_ACTIVE => {
                let starts = CPU_PROBE_STARTS.fetch_add(1, Ordering::AcqRel) + 1;
                assert!(
                    starts <= crate::cpu::topology().possible_count(),
                    "too many Phase 3 CPU-sentinel starts"
                );
                return;
            }
            state => panic!("invalid Phase 3 CPU-sentinel state {state}"),
        }
    }
}

#[inline]
pub(crate) fn cpu_probe_active() -> bool {
    CPU_PROBE_STATE.load(Ordering::Acquire) == CPU_PROBE_ACTIVE
}

#[inline]
fn current_cpu_counters() -> &'static CpuProbeCounters {
    &CPU_PROBE_COUNTERS[crate::cpu::current_id()]
}

pub(crate) fn record_cpu_probe_switch(runnable_us: usize) {
    if !cpu_probe_active() {
        return;
    }
    let counters = current_cpu_counters();
    counters.context_switches.fetch_add(1, Ordering::Relaxed);
    counters
        .runnable_us
        .fetch_add(runnable_us, Ordering::Relaxed);
}

pub(crate) fn record_cpu_probe_scheduler_wake(remote: bool, sent_ipi: bool) {
    if !cpu_probe_active() {
        return;
    }
    let counters = current_cpu_counters();
    if remote {
        counters.remote_wakes.fetch_add(1, Ordering::Relaxed);
    } else {
        counters.local_wakes.fetch_add(1, Ordering::Relaxed);
    }
    if sent_ipi {
        counters.reschedule_ipis.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn record_cpu_probe_run_queue(wait_ticks: usize, hold_ticks: usize) {
    if !cpu_probe_active() {
        return;
    }
    let counters = current_cpu_counters();
    counters.run_queue_accesses.fetch_add(1, Ordering::Relaxed);
    counters
        .run_queue_wait_ticks
        .fetch_add(wait_ticks, Ordering::Relaxed);
    counters
        .run_queue_hold_ticks
        .fetch_add(hold_ticks, Ordering::Relaxed);
}

pub(super) fn record_cpu_probe_exit() {
    if !cpu_probe_active() {
        return;
    }
    let exits = CPU_PROBE_EXITS.fetch_add(1, Ordering::AcqRel) + 1;
    let expected = crate::cpu::topology().possible_count();
    assert!(exits <= expected, "too many Phase 3 CPU-sentinel exits");
    if exits != expected {
        return;
    }
    assert_eq!(
        CPU_PROBE_STARTS.load(Ordering::Acquire),
        expected,
        "Phase 3 CPU-sentinel exited before every worker started"
    );

    let cpu_count = crate::cpu::topology().possible_count();
    let context_switches = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        CPU_PROBE_COUNTERS[cpu]
            .context_switches
            .load(Ordering::Acquire)
    });
    let local_wakes = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        CPU_PROBE_COUNTERS[cpu].local_wakes.load(Ordering::Acquire)
    });
    let remote_wakes = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        CPU_PROBE_COUNTERS[cpu].remote_wakes.load(Ordering::Acquire)
    });
    let reschedule_ipis = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        CPU_PROBE_COUNTERS[cpu]
            .reschedule_ipis
            .load(Ordering::Acquire)
    });
    let run_queue_accesses = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        CPU_PROBE_COUNTERS[cpu]
            .run_queue_accesses
            .load(Ordering::Acquire)
    });
    let run_queue_wait_ticks = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        CPU_PROBE_COUNTERS[cpu]
            .run_queue_wait_ticks
            .load(Ordering::Acquire)
    });
    let run_queue_hold_ticks = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        CPU_PROBE_COUNTERS[cpu]
            .run_queue_hold_ticks
            .load(Ordering::Acquire)
    });
    let runnable_us = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        CPU_PROBE_COUNTERS[cpu].runnable_us.load(Ordering::Acquire)
    });
    info!(
        "smp cpu-sentinel: sample={} workers={} clock_freq={} switches={:?} local_wakes={:?} remote_wakes={:?} resched_ipis={:?} rq_accesses={:?} rq_wait_ticks={:?} rq_hold_ticks={:?} runnable_us={:?}",
        CPU_PROBE_SAMPLE.load(Ordering::Acquire),
        expected,
        crate::config::clock_freq(),
        &context_switches[..cpu_count],
        &local_wakes[..cpu_count],
        &remote_wakes[..cpu_count],
        &reschedule_ipis[..cpu_count],
        &run_queue_accesses[..cpu_count],
        &run_queue_wait_ticks[..cpu_count],
        &run_queue_hold_ticks[..cpu_count],
        &runnable_us[..cpu_count],
    );
    CPU_PROBE_STATE.store(CPU_PROBE_INACTIVE, Ordering::Release);
}

pub(crate) fn start_wait_io_probe() {
    loop {
        match WAIT_IO_STATE.load(Ordering::Acquire) {
            CPU_PROBE_INACTIVE => {
                if WAIT_IO_STATE
                    .compare_exchange(
                        CPU_PROBE_INACTIVE,
                        CPU_PROBE_INITIALIZING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    continue;
                }
                for runs in &WAIT_IO_RUNS {
                    runs.store(0, Ordering::Relaxed);
                }
                WAIT_IO_STARTS.store(1, Ordering::Relaxed);
                WAIT_IO_EXITS.store(0, Ordering::Relaxed);
                WAIT_IO_STATE.store(CPU_PROBE_ACTIVE, Ordering::Release);
                return;
            }
            CPU_PROBE_INITIALIZING => spin_loop(),
            CPU_PROBE_ACTIVE => {
                let starts = WAIT_IO_STARTS.fetch_add(1, Ordering::AcqRel) + 1;
                assert!(
                    starts <= crate::cpu::topology().possible_count(),
                    "too many Phase 4 wait-io starts"
                );
                return;
            }
            state => panic!("invalid Phase 4 wait-io state {state}"),
        }
    }
}

pub(super) fn record_wait_io_run(cpu: usize) {
    if WAIT_IO_STATE.load(Ordering::Acquire) == CPU_PROBE_ACTIVE {
        WAIT_IO_RUNS[cpu].fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) fn record_wait_io_exit() {
    if WAIT_IO_STATE.load(Ordering::Acquire) != CPU_PROBE_ACTIVE {
        return;
    }
    let exits = WAIT_IO_EXITS.fetch_add(1, Ordering::AcqRel) + 1;
    let expected = crate::cpu::topology().possible_count();
    assert!(exits <= expected, "too many Phase 4 wait-io exits");
    if exits != expected {
        return;
    }
    assert_eq!(
        WAIT_IO_STARTS.load(Ordering::Acquire),
        expected,
        "Phase 4 wait-io exited before every worker started"
    );
    let runs =
        core::array::from_fn::<_, MAX_CPUS, _>(|cpu| WAIT_IO_RUNS[cpu].load(Ordering::Acquire));
    info!(
        "smp wait-io block: workers={} runs={:?} exits={}",
        expected,
        &runs[..expected],
        exits,
    );
    WAIT_IO_STATE.store(CPU_PROBE_INACTIVE, Ordering::Release);
}
