use core::cmp::Ordering;
use core::sync::atomic::AtomicU64;

use crate::config::clock_freq;
use crate::sbi::set_timer;
use crate::sync::UPIntrFreeCell;
use crate::task::{
    ProcessControlBlock, SignalFlags, SignalInfo, TaskControlBlock, queue_signal_to_task,
    wakeup_task,
};
use alloc::collections::BinaryHeap;
use alloc::sync::{Arc, Weak};
use lazy_static::*;
use riscv::register::time;

pub const TICKS_PER_SEC: usize = 100;
const MSEC_PER_SEC: usize = 1000;
const USEC_PER_SEC: usize = 1_000_000;
const NSEC_PER_SEC: u64 = 1_000_000_000;

static EPOCH_OFFSET_NS: AtomicU64 = AtomicU64::new(0);

fn get_time_nanos() -> u64 {
    let ticks = time::read() as u64;
    let freq = clock_freq() as u64;
    let secs = ticks / freq;
    let rem_ticks = ticks % freq;
    secs * NSEC_PER_SEC + rem_ticks * NSEC_PER_SEC / freq
}

pub fn monotonic_time_nanos() -> u64 {
    get_time_nanos()
}

pub fn init_wall_clock() {
    let base = crate::board::rtc_base();
    if base == 0 {
        return;
    }
    // Goldfish-RTC: TIME_LOW at +0x00, TIME_HIGH at +0x04 (nanoseconds since epoch)
    let time_low = unsafe { core::ptr::read_volatile(base as *const u32) } as u64;
    let time_high = unsafe { core::ptr::read_volatile((base + 0x04) as *const u32) } as u64;
    let rtc_nanos = (time_high << 32) | time_low;
    let monotonic_nanos = get_time_nanos();
    EPOCH_OFFSET_NS.store(
        rtc_nanos.wrapping_sub(monotonic_nanos),
        core::sync::atomic::Ordering::Relaxed,
    );
}

pub fn wall_time_nanos() -> u64 {
    monotonic_time_nanos().wrapping_add(EPOCH_OFFSET_NS.load(core::sync::atomic::Ordering::Relaxed))
}

pub fn get_time() -> usize {
    time::read()
}

pub fn get_time_ms() -> usize {
    time::read() / (clock_freq() / MSEC_PER_SEC)
}

pub fn get_time_us() -> usize {
    let ticks = time::read();
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
    set_timer(get_time() + clock_freq() / TICKS_PER_SEC);
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

impl PartialEq for TimerCondVar {
    fn eq(&self, other: &Self) -> bool {
        self.expire_ms == other.expire_ms
    }
}
impl Eq for TimerCondVar {}
impl PartialOrd for TimerCondVar {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let a = -(self.expire_ms as isize);
        let b = -(other.expire_ms as isize);
        Some(a.cmp(&b))
    }
}

impl Ord for TimerCondVar {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap()
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

lazy_static! {
    static ref TIMERS: UPIntrFreeCell<BinaryHeap<TimerCondVar>> =
        unsafe { UPIntrFreeCell::new(BinaryHeap::<TimerCondVar>::new()) };
    static ref REAL_TIMERS: UPIntrFreeCell<BinaryHeap<RealTimerEvent>> =
        unsafe { UPIntrFreeCell::new(BinaryHeap::<RealTimerEvent>::new()) };
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

pub fn check_timer() {
    let current_ms = get_time_ms();
    TIMERS.exclusive_session(|timers| {
        while let Some(timer) = timers.peek() {
            if timer.expire_ms <= current_ms {
                wakeup_task(Arc::clone(&timer.task));
                timers.pop();
            } else {
                break;
            }
        }
    });
    check_real_timers(get_time_us());
}
