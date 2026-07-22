use crate::cpu::{AtomicCpuMask, CpuMask};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const NO_LEADER: usize = usize::MAX;
const STATE_RUNNING: usize = 0;
const STATE_STOPPING: usize = 1;
const STATE_RESETTING: usize = 2;

static LEADER: AtomicUsize = AtomicUsize::new(NO_LEADER);
static STATE: AtomicUsize = AtomicUsize::new(STATE_RUNNING);
static STOPPED_CPUS: AtomicCpuMask = AtomicCpuMask::new(CpuMask::empty());
static FAILURE: AtomicBool = AtomicBool::new(false);

pub(crate) struct ShutdownLeader {
    cpu: usize,
    requested: CpuMask,
    stopped: CpuMask,
}

#[inline]
pub(crate) fn stop_requested() -> bool {
    STATE.load(Ordering::Acquire) != STATE_RUNNING
}

pub(crate) fn stop_current_cpu() -> ! {
    crate::arch::interrupt::disable_supervisor_interrupt();
    if let Some(cpu) = crate::cpu::try_current_id() {
        STOPPED_CPUS.insert(cpu, Ordering::Release);
    }
    crate::arch::smp::park_without_interrupts()
}

pub(crate) fn begin(failure: bool) -> ShutdownLeader {
    FAILURE.fetch_or(failure, Ordering::AcqRel);
    let cpu = crate::cpu::try_current_id().unwrap_or(0);
    if let Err(owner) = LEADER.compare_exchange(NO_LEADER, cpu, Ordering::AcqRel, Ordering::Acquire)
    {
        if owner == cpu {
            // A recursive panic on the shutdown leader cannot safely restart
            // coordination. Preserve failure status and use the raw backend.
            crate::sbi::shutdown(true);
        }
        stop_current_cpu();
    }

    crate::arch::interrupt::disable_supervisor_interrupt();
    let online = crate::cpu::online_mask();
    let mut requested = online;
    requested.remove(cpu);
    STATE.store(STATE_STOPPING, Ordering::Release);
    for target in 0..crate::cpu::topology().possible_count() {
        if requested.contains(target) {
            let _ = crate::arch::smp::send_ipi(target);
        }
    }

    let start = crate::timer::get_time();
    let timeout = (crate::config::clock_freq() / 2).max(1);
    loop {
        let stopped = STOPPED_CPUS.load(Ordering::Acquire);
        if stopped.bits() & requested.bits() == requested.bits()
            || crate::timer::get_time().wrapping_sub(start) >= timeout
        {
            return ShutdownLeader {
                cpu,
                requested,
                stopped,
            };
        }
        core::hint::spin_loop();
    }
}

pub(crate) fn complete(leader: ShutdownLeader) -> ! {
    let missing = leader.requested.bits() & !leader.stopped.bits();
    let failure = FAILURE.load(Ordering::Acquire);
    crate::console::emergency_print(format_args!(
        "smp shutdown: leader={} requested={:#x} stopped={:#x} missing={:#x} failure={}\n",
        leader.cpu,
        leader.requested.bits(),
        leader.stopped.bits() & leader.requested.bits(),
        missing,
        failure,
    ));
    STATE.store(STATE_RESETTING, Ordering::Release);
    crate::sbi::shutdown(failure)
}

pub fn shutdown(failure: bool) -> ! {
    complete(begin(failure))
}
