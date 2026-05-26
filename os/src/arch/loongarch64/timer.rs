use core::cmp::Ordering;
use core::sync::atomic::AtomicU64;

use crate::config::clock_freq;
use crate::sbi::set_timer;
use crate::sync::UPIntrFreeCell;
use crate::task::{
    ProcessControlBlock, SignalFlags, SignalInfo, TaskControlBlock, queue_signal_to_task,
    wakeup_timer_task,
};
use alloc::collections::BinaryHeap;
use alloc::sync::{Arc, Weak};
use lazy_static::*;
use loongArch64::time::Time;

pub const TICKS_PER_SEC: usize = 100;
// CONTEXT: Keep Linux-visible clock ticks at 100 Hz, but drive scheduler
// timer interrupts at 1 kHz so 1 ms clock_nanosleep workloads can wake on
// time instead of waiting for the next 10 ms accounting tick.
const TIMER_INTERRUPTS_PER_SEC: usize = 1000;
const MSEC_PER_SEC: usize = 1000;
const USEC_PER_SEC: usize = 1_000_000;
const NSEC_PER_SEC: u64 = 1_000_000_000;

static EPOCH_OFFSET_NS: AtomicU64 = AtomicU64::new(0);

fn get_time_nanos() -> u64 {
    let ticks = Time::read() as u64;
    let freq = clock_freq() as u64;
    let secs = ticks / freq;
    let rem_ticks = ticks % freq;
    secs * NSEC_PER_SEC + rem_ticks * NSEC_PER_SEC / freq
}

pub fn monotonic_time_nanos() -> u64 {
    get_time_nanos()
}

pub fn init_wall_clock() {
    // UNFINISHED: LoongArch RTC discovery not yet implemented
    // CONTEXT: Until platform RTC probing seeds EPOCH_OFFSET_NS, wall-clock
    // syscalls use monotonic time with a zero epoch offset or a value supplied
    // later through set_wall_time_nanos().
    let _ = &EPOCH_OFFSET_NS;
}

pub fn wall_time_nanos() -> u64 {
    monotonic_time_nanos().wrapping_add(EPOCH_OFFSET_NS.load(core::sync::atomic::Ordering::Relaxed))
}

pub fn set_wall_time_nanos(wall_nanos: u64) {
    EPOCH_OFFSET_NS.store(
        wall_nanos.wrapping_sub(monotonic_time_nanos()),
        core::sync::atomic::Ordering::Relaxed,
    );
}

pub fn get_time() -> usize {
    Time::read()
}

pub fn get_time_ms() -> usize {
    Time::read() / (clock_freq() / MSEC_PER_SEC)
}

pub fn get_time_us() -> usize {
    let ticks = Time::read();
    let freq = clock_freq();
    ticks / freq * USEC_PER_SEC + ticks % freq * USEC_PER_SEC / freq
}

pub fn us_to_clock_ticks(us: usize) -> usize {
    us / (USEC_PER_SEC / TICKS_PER_SEC)
}

pub fn get_time_clock_ticks() -> usize {
    us_to_clock_ticks(get_time_us())
}

pub fn set_next_trigger() {
    set_timer(get_time() + clock_freq() / TIMER_INTERRUPTS_PER_SEC);
}

pub struct TimerCondVar {
    pub expire_ms: usize,
    pub task: Arc<TaskControlBlock>,
}

pub struct RealTimerEvent {
    pub expire_us: usize,
    pub generation: u64,
    pub process: Weak<ProcessControlBlock>,
}

pub struct PosixTimerEvent {
    pub expire_us: usize,
    pub timer_id: usize,
    pub generation: u64,
    pub process: Weak<ProcessControlBlock>,
}

impl PartialEq for TimerCondVar {
    fn eq(&self, other: &Self) -> bool {
        self.expire_ms == other.expire_ms
    }
}

impl Eq for TimerCondVar {}

impl PartialOrd for TimerCondVar {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerCondVar {
    fn cmp(&self, other: &Self) -> Ordering {
        other.expire_ms.cmp(&self.expire_ms)
    }
}

impl PartialEq for RealTimerEvent {
    fn eq(&self, other: &Self) -> bool {
        self.expire_us == other.expire_us && self.generation == other.generation
    }
}

impl Eq for RealTimerEvent {}

impl PartialOrd for RealTimerEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RealTimerEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .expire_us
            .cmp(&self.expire_us)
            .then_with(|| other.generation.cmp(&self.generation))
    }
}

impl PartialEq for PosixTimerEvent {
    fn eq(&self, other: &Self) -> bool {
        self.expire_us == other.expire_us
            && self.timer_id == other.timer_id
            && self.generation == other.generation
    }
}

impl Eq for PosixTimerEvent {}

impl PartialOrd for PosixTimerEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PosixTimerEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .expire_us
            .cmp(&self.expire_us)
            .then_with(|| other.generation.cmp(&self.generation))
            .then_with(|| other.timer_id.cmp(&self.timer_id))
    }
}

lazy_static! {
    static ref TIMERS: UPIntrFreeCell<BinaryHeap<TimerCondVar>> =
        unsafe { UPIntrFreeCell::new(BinaryHeap::<TimerCondVar>::new()) };
    static ref REAL_TIMERS: UPIntrFreeCell<BinaryHeap<RealTimerEvent>> =
        unsafe { UPIntrFreeCell::new(BinaryHeap::<RealTimerEvent>::new()) };
    static ref POSIX_TIMERS: UPIntrFreeCell<BinaryHeap<PosixTimerEvent>> =
        unsafe { UPIntrFreeCell::new(BinaryHeap::<PosixTimerEvent>::new()) };
}

pub fn add_timer(expire_ms: usize, task: Arc<TaskControlBlock>) {
    let mut timers = TIMERS.exclusive_access();
    timers.push(TimerCondVar { expire_ms, task });
}

pub fn add_real_timer(expire_us: usize, generation: u64, process: Arc<ProcessControlBlock>) {
    let mut timers = REAL_TIMERS.exclusive_access();
    timers.push(RealTimerEvent {
        expire_us,
        generation,
        process: Arc::downgrade(&process),
    });
}

pub fn add_posix_timer(
    expire_us: usize,
    timer_id: usize,
    generation: u64,
    process: Arc<ProcessControlBlock>,
) {
    let mut timers = POSIX_TIMERS.exclusive_access();
    timers.push(PosixTimerEvent {
        expire_us,
        timer_id,
        generation,
        process: Arc::downgrade(&process),
    });
}

fn check_real_timers(current_us: usize) {
    loop {
        let event = {
            let mut timers = REAL_TIMERS.exclusive_access();
            let Some(timer) = timers.peek() else {
                return;
            };
            if timer.expire_us > current_us {
                return;
            }
            timers.pop().unwrap()
        };
        let Some(process) = event.process.upgrade() else {
            continue;
        };
        let Some((task, next_timer)) = process.expire_real_timer(event.generation, current_us)
        else {
            continue;
        };
        queue_signal_to_task(task, SignalFlags::SIGALRM, SignalInfo::user(14, 0));
        if let Some((next_expire_us, generation)) = next_timer {
            add_real_timer(next_expire_us, generation, process);
        }
    }
}

fn check_posix_timers(current_us: usize) {
    loop {
        let event = {
            let mut timers = POSIX_TIMERS.exclusive_access();
            let Some(timer) = timers.peek() else {
                return;
            };
            if timer.expire_us > current_us {
                return;
            }
            timers.pop().unwrap()
        };
        let Some(process) = event.process.upgrade() else {
            continue;
        };
        let Some((task, signal, next_timer)) =
            process.expire_posix_timer(event.timer_id, event.generation, current_us)
        else {
            continue;
        };
        let signum = signal;
        if let Some(signal) = SignalFlags::from_signum(signum) {
            queue_signal_to_task(task, signal, SignalInfo::user(signum as i32, 0));
        }
        if let Some((next_expire_us, generation)) = next_timer {
            add_posix_timer(next_expire_us, event.timer_id, generation, process);
        }
    }
}

pub fn check_timer() {
    let current_ms = get_time_ms();
    TIMERS.exclusive_session(|timers| {
        while let Some(timer) = timers.peek() {
            if timer.expire_ms <= current_ms {
                wakeup_timer_task(Arc::clone(&timer.task));
                timers.pop();
            } else {
                break;
            }
        }
    });
    let current_us = get_time_us();
    check_real_timers(current_us);
    check_posix_timers(current_us);
}
