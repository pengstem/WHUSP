use core::cmp::Ordering;
use core::sync::atomic::AtomicU64;

use crate::config::clock_freq;
use crate::sbi::set_timer;
use crate::sync::SpinNoIrqLock;
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
const NSEC_PER_TENTH_SEC: u64 = 100_000_000;

const LS7A_TOY_READ0: usize = 0x2c;
const LS7A_TOY_READ1: usize = 0x30;
const LS7A_RTC_CTRL: usize = 0x40;
const LS7A_RTC_CTRL_OSC_ENABLE: u32 = 1 << 8;
const LS7A_RTC_CTRL_TOY_ENABLE: u32 = 1 << 11;

static EPOCH_OFFSET_NS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct LoongsonToyTime {
    year_since_1900: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    tenth_second: u32,
}

pub fn monotonic_time_sec_nsec() -> (u64, u32) {
    let ticks = Time::read() as u64;
    let freq = clock_freq() as u64;
    let secs = ticks / freq;
    let rem_ticks = ticks % freq;
    let nsecs = (rem_ticks * NSEC_PER_SEC / freq) as u32;
    (secs, nsecs)
}

fn get_time_nanos() -> u64 {
    let (secs, nsecs) = monotonic_time_sec_nsec();
    secs * NSEC_PER_SEC + nsecs as u64
}

pub fn monotonic_time_nanos() -> u64 {
    get_time_nanos()
}

pub fn init_wall_clock() {
    let base = crate::board::rtc_base();
    if base == 0 {
        return;
    }
    let Some(rtc_nanos) = read_ls7a_toy_time(base).and_then(LoongsonToyTime::to_unix_nanos) else {
        return;
    };
    let offset = rtc_nanos.wrapping_sub(monotonic_time_nanos());
    EPOCH_OFFSET_NS.store(offset, core::sync::atomic::Ordering::Relaxed);
}

pub fn wall_time_nanos() -> u64 {
    monotonic_time_nanos().wrapping_add(EPOCH_OFFSET_NS.load(core::sync::atomic::Ordering::Relaxed))
}

pub fn wall_time_offset_nanos() -> u64 {
    EPOCH_OFFSET_NS.load(core::sync::atomic::Ordering::Relaxed)
}

pub fn set_wall_time_nanos(wall_nanos: u64) {
    let offset = wall_nanos.wrapping_sub(monotonic_time_nanos());
    EPOCH_OFFSET_NS.store(offset, core::sync::atomic::Ordering::Relaxed);
    crate::vdso::refresh_wall_time_offset(offset);
}

fn read_ls7a_toy_time(base: usize) -> Option<LoongsonToyTime> {
    let ctrl = read_mmio_u32(base + LS7A_RTC_CTRL);
    write_mmio_u32(
        base + LS7A_RTC_CTRL,
        ctrl | LS7A_RTC_CTRL_OSC_ENABLE | LS7A_RTC_CTRL_TOY_ENABLE,
    );

    // CONTEXT: QEMU's LS7A RTC follows the Linux driver layout: TOY_READ0
    // carries month/day/time fields and TOY_READ1 carries `tm_year`.
    let year_before = read_mmio_u32(base + LS7A_TOY_READ1);
    let mut low = read_mmio_u32(base + LS7A_TOY_READ0);
    let mut year = read_mmio_u32(base + LS7A_TOY_READ1);
    if year_before != year {
        low = read_mmio_u32(base + LS7A_TOY_READ0);
        year = read_mmio_u32(base + LS7A_TOY_READ1);
    }

    LoongsonToyTime {
        year_since_1900: year,
        month: (low >> 26) & 0x3f,
        day: (low >> 21) & 0x1f,
        hour: (low >> 16) & 0x1f,
        minute: (low >> 10) & 0x3f,
        second: (low >> 4) & 0x3f,
        tenth_second: low & 0x0f,
    }
    .valid()
}

fn read_mmio_u32(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

fn write_mmio_u32(addr: usize, value: u32) {
    unsafe {
        core::ptr::write_volatile(addr as *mut u32, value);
    }
}

impl LoongsonToyTime {
    fn valid(self) -> Option<Self> {
        let year = self.year_since_1900.checked_add(1900)?;
        if year < 1970
            || !(1..=12).contains(&self.month)
            || self.day == 0
            || self.day > days_in_month(year, self.month)
            || self.hour > 23
            || self.minute > 59
            || self.second > 59
            || self.tenth_second > 9
        {
            return None;
        }
        Some(self)
    }

    fn to_unix_nanos(self) -> Option<u64> {
        let year = self.year_since_1900.checked_add(1900)?;
        let days = days_from_civil(year as i64, self.month, self.day);
        if days < 0 {
            return None;
        }
        let seconds = (days as u64)
            .saturating_mul(86_400)
            .saturating_add((self.hour as u64) * 3_600)
            .saturating_add((self.minute as u64) * 60)
            .saturating_add(self.second as u64);
        Some(
            seconds
                .saturating_mul(NSEC_PER_SEC)
                .saturating_add((self.tenth_second as u64) * NSEC_PER_TENTH_SEC),
        )
    }
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - if month <= 2 { 1 } else { 0 };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let month_prime = (if month > 2 { month - 3 } else { month + 9 }) as i64;
    let day_of_year = (153 * month_prime + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
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
    pub task: Weak<TaskControlBlock>,
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
    static ref TIMERS: SpinNoIrqLock<BinaryHeap<TimerCondVar>> =
        SpinNoIrqLock::new(BinaryHeap::<TimerCondVar>::new());
    static ref REAL_TIMERS: SpinNoIrqLock<BinaryHeap<RealTimerEvent>> =
        SpinNoIrqLock::new(BinaryHeap::<RealTimerEvent>::new());
    static ref POSIX_TIMERS: SpinNoIrqLock<BinaryHeap<PosixTimerEvent>> =
        SpinNoIrqLock::new(BinaryHeap::<PosixTimerEvent>::new());
}

pub fn add_timer(expire_ms: usize, task: Arc<TaskControlBlock>) {
    let mut timers = TIMERS.lock();
    timers.push(TimerCondVar {
        expire_ms,
        task: Arc::downgrade(&task),
    });
}

pub fn add_real_timer(expire_us: usize, generation: u64, process: Arc<ProcessControlBlock>) {
    let mut timers = REAL_TIMERS.lock();
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
    let mut timers = POSIX_TIMERS.lock();
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
            let mut timers = REAL_TIMERS.lock();
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
            let mut timers = POSIX_TIMERS.lock();
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
    assert!(
        crate::cpu::is_timer_expiry_owner(),
        "global timer heaps checked by a non-owner CPU"
    );
    let current_ms = get_time_ms();
    loop {
        let timer = {
            let mut timers = TIMERS.lock();
            match timers.peek() {
                Some(timer) if timer.expire_ms <= current_ms => timers.pop(),
                _ => None,
            }
        };
        let Some(timer) = timer else {
            break;
        };
        // Waking may take task/run-queue locks and send a remote reschedule
        // IPI. Never perform that handoff while holding the global timer heap.
        if let Some(task) = timer.task.upgrade() {
            wakeup_timer_task(task);
        }
    }
    let current_us = get_time_us();
    check_real_timers(current_us);
    check_posix_timers(current_us);
}
