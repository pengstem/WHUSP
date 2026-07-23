use super::{
    ProcessControlBlock, ProcessProcSnapshot, SCHED_RR_INTERVAL_US, TaskControlBlock, TaskStatus,
};
use crate::config::MAX_CPUS;
use crate::perf;
use crate::sync::{SpinNoIrqLock, UPIntrFreeCell};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use lazy_static::*;
use log::info;

const RT_PRIORITY_MAX: usize = 99;
const RT_QUEUE_COUNT: usize = RT_PRIORITY_MAX + 1;
const NICE_0_LOAD: u64 = 1024;
const NORMAL_PREEMPT_GRANULARITY_US: u64 = 1_000;
const NICE_TO_WEIGHT: [u64; 40] = [
    88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100, 4904,
    3906, 3121, 2501, 1991, 1586, 1277, 1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87,
    70, 56, 45, 36, 29, 23, 18, 15,
];

type NormalQueueKey = (u64, u64);

enum ClaimResult {
    Claimed,
    Ineligible,
    Exited,
}

pub struct TaskManager {
    cpu: crate::cpu::CpuId,
    normal_queue: BTreeMap<NormalQueueKey, Arc<TaskControlBlock>>,
    normal_enqueue_seq: u64,
    normal_min_vruntime: u64,
    rt_queues: Vec<VecDeque<Arc<TaskControlBlock>>>,
    rt_ready_bitmap: u128,
    ready_count: usize,
}

/// One CPU's run queue.
///
/// Realtime tasks use Linux-style static priority buckets, while normal tasks
/// use a nice-weighted vruntime key. Do not treat this as a full Linux
/// scheduler class implementation; syscall-visible SCHED_DEADLINE attributes
/// are stored elsewhere but are not enforced by this picker.
impl TaskManager {
    pub fn new(cpu: crate::cpu::CpuId) -> Self {
        Self {
            cpu,
            normal_queue: BTreeMap::new(),
            normal_enqueue_seq: 0,
            normal_min_vruntime: 0,
            rt_queues: (0..RT_QUEUE_COUNT).map(|_| VecDeque::new()).collect(),
            rt_ready_bitmap: 0,
            ready_count: 0,
        }
    }

    fn rt_priority(task: &TaskControlBlock) -> usize {
        task.realtime_priority().clamp(0, RT_PRIORITY_MAX as i32) as usize
    }

    fn ready_len(&self) -> usize {
        self.ready_count
    }

    fn min_vruntime(&self) -> u64 {
        self.normal_min_vruntime
    }

    fn nice_weight(nice: i8) -> u64 {
        let index = (nice.clamp(-20, 19) as i32 + 20) as usize;
        NICE_TO_WEIGHT[index]
    }

    fn vruntime_delta_for_nice(nice: i8, runtime_us: usize) -> u64 {
        let weight = Self::nice_weight(nice);
        let weighted_runtime = (runtime_us as u64).max(1).saturating_mul(NICE_0_LOAD);
        weighted_runtime.div_ceil(weight).max(1)
    }

    fn vruntime_delta_for_runtime(task: &TaskControlBlock, runtime_us: usize) -> u64 {
        Self::vruntime_delta_for_nice(task.nice_value(), runtime_us)
    }

    fn charge_normal_runtime(task: &TaskControlBlock) {
        if Self::rt_priority(task) != 0 {
            return;
        }
        let runtime_us = task.take_sched_runtime_us(crate::timer::get_time_us());
        let delta = Self::vruntime_delta_for_runtime(task, runtime_us);
        task.add_sched_vruntime(delta);
        perf::record_scheduler_normal_requeue(delta as usize);
    }

    fn task_is_exited(task: &TaskControlBlock) -> bool {
        task.inner_exclusive_access().task_status == TaskStatus::Exited
    }

    fn ready_rt_priority(task: &TaskControlBlock) -> Option<usize> {
        if Self::task_is_exited(task) {
            return None;
        }
        let priority = Self::rt_priority(task);
        (priority > 0).then_some(priority)
    }

    fn current_run_time_us(current: &TaskControlBlock) -> usize {
        current.sched_runtime_us(crate::timer::get_time_us())
    }

    fn rt_priority_bit(priority: usize) -> u128 {
        1u128 << priority
    }

    fn add(&mut self, task: Arc<TaskControlBlock>) {
        Self::mark_queued_on(&task, self.cpu);
        self.enqueue(task, false);
    }

    fn requeue_after_run(&mut self, task: Arc<TaskControlBlock>) {
        Self::charge_normal_runtime(&task);
        Self::mark_queued_on(&task, self.cpu);
        self.enqueue(task, false);
    }

    fn add_front(&mut self, task: Arc<TaskControlBlock>) {
        Self::mark_queued_on(&task, self.cpu);
        self.enqueue(task, true);
    }

    fn mark_queued_on(task: &TaskControlBlock, cpu: crate::cpu::CpuId) {
        let mut inner = task.inner_exclusive_access();
        assert_eq!(
            inner.task_status,
            TaskStatus::Ready,
            "only Ready tasks may enter the run queue"
        );
        assert!(!inner.on_rq, "task is already on a run queue");
        assert!(
            inner.on_cpu.is_none(),
            "running task cannot enter a run queue"
        );
        inner.on_rq = true;
        inner.queued_cpu = Some(cpu);
    }

    fn clear_queued(task: &TaskControlBlock) {
        let mut inner = task.inner_exclusive_access();
        assert!(inner.on_rq, "removed task was not marked on a run queue");
        inner.on_rq = false;
        inner.queued_cpu = None;
    }

    fn claim_for_cpu(&self, task: &TaskControlBlock, cpu: crate::cpu::CpuId) -> ClaimResult {
        let mut inner = task.inner_exclusive_access();
        if inner.task_status == TaskStatus::Exited {
            assert!(inner.on_rq, "exited run-queue task lost its queue marker");
            inner.on_rq = false;
            inner.queued_cpu = None;
            return ClaimResult::Exited;
        }
        assert_eq!(
            inner.task_status,
            TaskStatus::Ready,
            "run queue contained a non-ready task"
        );
        assert!(inner.on_rq, "run-queue task lost its queue marker");
        assert_eq!(
            inner.queued_cpu,
            Some(self.cpu),
            "run-queue task has the wrong queue owner"
        );
        assert!(inner.on_cpu.is_none(), "run-queue task is already running");
        if !inner.allowed_cpus.contains(cpu) {
            return ClaimResult::Ineligible;
        }
        let process = task
            .process
            .upgrade()
            .expect("runnable task process must outlive the task");
        if !process.try_claim_scheduler_task(task) {
            return ClaimResult::Ineligible;
        }
        inner.on_rq = false;
        inner.queued_cpu = None;
        inner.on_cpu = Some(cpu);
        inner.task_status = TaskStatus::Running;
        if inner.smp_sched_probe_active {
            super::smp_probe::record_run(cpu);
        }
        if inner.smp_wait_io_probe {
            super::smp_probe::record_wait_io_run(cpu);
        }
        ClaimResult::Claimed
    }

    fn enqueue(&mut self, task: Arc<TaskControlBlock>, front: bool) {
        let rt_priority = Self::rt_priority(&task);
        if rt_priority > 0 {
            self.rt_ready_bitmap |= Self::rt_priority_bit(rt_priority);
            let queue = &mut self.rt_queues[rt_priority];
            if front {
                queue.push_front(task);
            } else {
                queue.push_back(task);
            }
            self.ready_count += 1;
        } else {
            let vruntime = if front {
                self.normal_min_vruntime.saturating_sub(1)
            } else {
                task.floor_sched_vruntime(self.normal_min_vruntime)
            };
            self.normal_enqueue_seq = self.normal_enqueue_seq.wrapping_add(1);
            let old_task = self
                .normal_queue
                .insert((vruntime, self.normal_enqueue_seq), task);
            debug_assert!(old_task.is_none());
            if old_task.is_none() {
                self.ready_count += 1;
            }
        }
    }

    fn decrement_ready_count(&mut self) {
        debug_assert!(self.ready_count > 0);
        self.ready_count = self.ready_count.saturating_sub(1);
    }

    fn clear_rt_priority_if_empty(&mut self, priority: usize) {
        if priority > 0 && self.rt_queues[priority].is_empty() {
            self.rt_ready_bitmap &= !Self::rt_priority_bit(priority);
        }
    }

    fn highest_rt_priority(&self) -> Option<usize> {
        perf::record_scheduler_rt_priority_probes(1);
        if self.rt_ready_bitmap == 0 {
            return None;
        }
        Some((u128::BITS - 1 - self.rt_ready_bitmap.leading_zeros()) as usize)
    }

    fn fetch(&mut self, cpu: crate::cpu::CpuId) -> Option<Arc<TaskControlBlock>> {
        let _profile_scope = perf::time_scope(perf::ProfilePoint::SchedulerFetch);
        let queue_len = self.ready_len();
        let mut remaining = queue_len;
        let mut scanned = 0;
        let mut pruned_exited = 0;

        'select: loop {
            if remaining == 0 {
                perf::record_scheduler_fetch(queue_len, scanned, pruned_exited);
                return None;
            }
            while let Some(priority) = self.highest_rt_priority() {
                let Some(task) = self.rt_queues[priority].pop_front() else {
                    self.clear_rt_priority_if_empty(priority);
                    continue;
                };
                self.decrement_ready_count();
                remaining = remaining.saturating_sub(1);
                self.clear_rt_priority_if_empty(priority);
                if Self::task_is_exited(&task) {
                    Self::clear_queued(&task);
                    pruned_exited += 1;
                    continue;
                }
                scanned += 1;
                let current_priority = Self::rt_priority(&task);
                if current_priority == priority {
                    match self.claim_for_cpu(&task, cpu) {
                        ClaimResult::Claimed => {
                            perf::record_scheduler_fetch(queue_len, scanned, pruned_exited);
                            return Some(task);
                        }
                        ClaimResult::Ineligible => self.enqueue(task, false),
                        ClaimResult::Exited => pruned_exited += 1,
                    }
                    if remaining == 0 {
                        perf::record_scheduler_fetch(queue_len, scanned, pruned_exited);
                        return None;
                    }
                    continue;
                }
                // A concurrent policy update can leave a task in the old
                // priority bucket. Relocate it without dropping its on_rq
                // ownership between the two internal queues.
                self.enqueue(task, false);
                if remaining == 0 {
                    perf::record_scheduler_fetch(queue_len, scanned, pruned_exited);
                    return None;
                }
            }

            while let Some((key, task)) = self.normal_queue.pop_first() {
                self.decrement_ready_count();
                remaining = remaining.saturating_sub(1);
                self.normal_min_vruntime = self.normal_min_vruntime.max(key.0);
                if Self::task_is_exited(&task) {
                    Self::clear_queued(&task);
                    pruned_exited += 1;
                    continue;
                }
                scanned += 1;
                if Self::rt_priority(&task) > 0 {
                    self.enqueue(task, false);
                    continue 'select;
                }
                match self.claim_for_cpu(&task, cpu) {
                    ClaimResult::Claimed => {
                        perf::record_scheduler_fetch(queue_len, scanned, pruned_exited);
                        return Some(task);
                    }
                    ClaimResult::Ineligible => self.enqueue(task, false),
                    ClaimResult::Exited => pruned_exited += 1,
                }
                if remaining == 0 {
                    perf::record_scheduler_fetch(queue_len, scanned, pruned_exited);
                    return None;
                }
            }

            perf::record_scheduler_fetch(queue_len, scanned, pruned_exited);
            return None;
        }
    }

    fn should_preempt_current_on_tick(&self, current: &Arc<TaskControlBlock>) -> bool {
        if let Some(is_owner) = current
            .process
            .upgrade()
            .and_then(|process| process.scheduler_task_exclusion_owner(current))
        {
            // Keep the exclusive owner running while forcing every remote
            // sibling through switch completion so its resources become safe
            // to tear down.
            return !is_owner;
        }
        let current_rt_priority = Self::rt_priority(current);

        for priority in (current_rt_priority + 1..=RT_PRIORITY_MAX).rev() {
            if self.rt_ready_bitmap & Self::rt_priority_bit(priority) == 0 {
                continue;
            }
            if self.rt_queues[priority].iter().any(|task| {
                !Arc::ptr_eq(task, current)
                    && Self::ready_rt_priority(task)
                        .is_some_and(|task_priority| task_priority == priority)
            }) {
                return true;
            }
        }
        if current_rt_priority > 0 {
            let runtime_us = Self::current_run_time_us(current);
            if current.is_realtime_round_robin()
                && runtime_us >= SCHED_RR_INTERVAL_US
                && self.rt_queues[current_rt_priority].iter().any(|task| {
                    !Arc::ptr_eq(task, current)
                        && Self::ready_rt_priority(task)
                            .is_some_and(|task_priority| task_priority == current_rt_priority)
                })
            {
                return true;
            }
            return false;
        }

        let Some(best_normal_key) = self.normal_queue.iter().find_map(|(key, task)| {
            if Self::task_is_exited(task) || Self::rt_priority(task) > 0 {
                None
            } else {
                Some(*key)
            }
        }) else {
            return false;
        };

        let (sched_vruntime, nice) = {
            let inner = current.inner_exclusive_access();
            (inner.sched_vruntime, inner.nice)
        };
        let runtime_us = Self::current_run_time_us(current);
        let current_base_vruntime = sched_vruntime.max(self.normal_min_vruntime);
        let current_projected_vruntime =
            current_base_vruntime.saturating_add(Self::vruntime_delta_for_nice(nice, runtime_us));
        current_projected_vruntime
            > best_normal_key
                .0
                .saturating_add(NORMAL_PREEMPT_GRANULARITY_US)
    }

    fn remove_process_tasks(&mut self, process_id: usize) {
        self.normal_queue.retain(|_, task| {
            let keep = task
                .process
                .upgrade()
                .is_none_or(|process| process.getpid() != process_id);
            if !keep {
                Self::clear_queued(task);
            }
            keep
        });
        for queue in &mut self.rt_queues {
            queue.retain(|task| {
                let keep = task
                    .process
                    .upgrade()
                    .is_none_or(|process| process.getpid() != process_id);
                if !keep {
                    Self::clear_queued(task);
                }
                keep
            });
        }
        self.rebuild_ready_metadata();
    }

    fn rebuild_ready_metadata(&mut self) {
        self.rt_ready_bitmap = 0;
        self.ready_count = self.normal_queue.len();
        for (priority, queue) in self.rt_queues.iter().enumerate() {
            self.ready_count += queue.len();
            if priority > 0 && !queue.is_empty() {
                self.rt_ready_bitmap |= Self::rt_priority_bit(priority);
            }
        }
    }

    fn remove_ready_task(&mut self, task: &Arc<TaskControlBlock>) -> bool {
        if remove_task_from_normal_queue(&mut self.normal_queue, task) {
            self.decrement_ready_count();
            return true;
        }
        for priority in 1..=RT_PRIORITY_MAX {
            if remove_task_from_queue(&mut self.rt_queues[priority], task) {
                self.decrement_ready_count();
                self.clear_rt_priority_if_empty(priority);
                return true;
            }
        }
        false
    }

    fn reprioritize_ready_task(&mut self, task: Arc<TaskControlBlock>) {
        if task.inner_exclusive_access().task_status != TaskStatus::Ready {
            return;
        }
        if self.remove_ready_task(&task) {
            self.enqueue(task, false);
        }
    }
}

fn remove_task_from_normal_queue(
    queue: &mut BTreeMap<NormalQueueKey, Arc<TaskControlBlock>>,
    task: &Arc<TaskControlBlock>,
) -> bool {
    let key = queue
        .iter()
        .find_map(|(key, candidate)| Arc::ptr_eq(candidate, task).then_some(*key));
    let Some(key) = key else {
        return false;
    };
    queue.remove(&key);
    true
}

fn remove_task_from_queue(
    queue: &mut VecDeque<Arc<TaskControlBlock>>,
    task: &Arc<TaskControlBlock>,
) -> bool {
    let Some(index) = queue
        .iter()
        .position(|candidate| Arc::ptr_eq(candidate, task))
    else {
        return false;
    };
    queue.remove(index);
    true
}

lazy_static! {
    static ref RUN_QUEUES: Vec<SpinNoIrqLock<TaskManager>> = (0..MAX_CPUS)
        .map(|cpu| SpinNoIrqLock::new(TaskManager::new(cpu)))
        .collect();
    static ref PID2PCB: UPIntrFreeCell<BTreeMap<usize, Arc<ProcessControlBlock>>> =
        unsafe { UPIntrFreeCell::new(BTreeMap::new()) };
    static ref LINUX_TID2TASK: UPIntrFreeCell<BTreeMap<usize, Weak<TaskControlBlock>>> =
        unsafe { UPIntrFreeCell::new(BTreeMap::new()) };
}

static RUNNABLE_QUEUES: crate::cpu::AtomicCpuMask =
    crate::cpu::AtomicCpuMask::new(crate::cpu::CpuMask::empty());

fn publish_run_queue_transition(cpu: crate::cpu::CpuId, was_runnable: bool, ready: usize) {
    let is_runnable = ready != 0;
    if was_runnable == is_runnable {
        return;
    }
    if is_runnable {
        RUNNABLE_QUEUES.insert(cpu, Ordering::Release);
    } else {
        RUNNABLE_QUEUES.remove(cpu, Ordering::Release);
    }
}

fn with_run_queue<R>(cpu: crate::cpu::CpuId, operation: impl FnOnce(&mut TaskManager) -> R) -> R {
    assert!(cpu < MAX_CPUS, "run-queue CPU exceeds MAX_CPUS");
    let probe = super::smp_probe::cpu_probe_active();
    let wait_start = probe.then(crate::timer::get_time);
    let mut manager = RUN_QUEUES[cpu].lock();
    let acquired = probe.then(crate::timer::get_time);
    let was_runnable = manager.ready_len() != 0;
    let result = operation(&mut manager);
    publish_run_queue_transition(cpu, was_runnable, manager.ready_len());
    drop(manager);
    if let (Some(wait_start), Some(acquired)) = (wait_start, acquired) {
        let released = crate::timer::get_time();
        super::smp_probe::record_cpu_probe_run_queue(
            acquired.saturating_sub(wait_start),
            released.saturating_sub(acquired),
        );
    }
    result
}

fn run_queue_load(cpu: crate::cpu::CpuId) -> usize {
    let ready = RUN_QUEUES[cpu].lock().ready_len();
    ready + usize::from(!super::processor_is_idle(cpu))
}

fn choose_run_queue(
    allowed: crate::cpu::CpuMask,
    preferred: Option<crate::cpu::CpuId>,
) -> crate::cpu::CpuId {
    let online = crate::cpu::online_mask();
    let eligible = crate::cpu::CpuMask::from_bits(allowed.bits() & online.bits());
    assert!(eligible.bits() != 0, "runnable task has no online CPU");

    let mut best = None;
    for cpu in 0..crate::cpu::topology().possible_count() {
        if !eligible.contains(cpu) {
            continue;
        }
        let load = run_queue_load(cpu);
        let prefer = usize::from(preferred == Some(cpu));
        if best.is_none_or(|(_, best_load, best_prefer)| {
            load < best_load || (load == best_load && prefer > best_prefer)
        }) {
            best = Some((cpu, load, prefer));
        }
    }
    best.expect("eligible CPU mask contained no topology CPU").0
}

fn enqueue_task_on_best_queue(
    task: Arc<TaskControlBlock>,
    front: bool,
    charge_runtime: bool,
) -> crate::cpu::CpuId {
    let allowed = task.inner_exclusive_access().allowed_cpus;
    let preferred = crate::cpu::try_current_id().filter(|cpu| allowed.contains(*cpu));
    let target = choose_run_queue(allowed, preferred);
    with_run_queue(target, |manager| {
        if charge_runtime {
            manager.requeue_after_run(task);
        } else if front {
            manager.add_front(task);
        } else {
            manager.add(task);
        }
    });
    crate::cpu::wake_scheduler_cpu(crate::cpu::CpuMask::single(target));
    target
}

pub fn add_task(task: Arc<TaskControlBlock>) {
    enqueue_task_on_best_queue(task, false, false);
}

pub(crate) fn requeue_task_after_run(task: Arc<TaskControlBlock>) {
    enqueue_task_on_best_queue(task, false, true);
}

pub(super) fn charge_task_after_run(task: &TaskControlBlock) {
    TaskManager::charge_normal_runtime(task);
}

pub(super) fn should_preempt_current_on_tick(current: &Arc<TaskControlBlock>) -> bool {
    with_run_queue(crate::cpu::current_id(), |manager| {
        manager.should_preempt_current_on_tick(current)
    })
}

fn wakeup_task_with_placement(task: Arc<TaskControlBlock>, front: bool) -> bool {
    let mut task_inner = task.inner_exclusive_access();
    if task_inner.task_status == TaskStatus::Blocked {
        assert!(!task_inner.on_rq, "blocked task is still on a run queue");
        if task_inner.on_cpu.is_some() {
            if task_inner.wake_pending {
                task_inner.wake_front |= front;
                return false;
            }
            task_inner.wake_pending = true;
            task_inner.wake_front = front;
            let probe = task_inner.smp_sched_probe_active;
            drop(task_inner);
            if probe {
                super::smp_probe::record_wake(true);
            }
            perf::record_task_wakeup(front);
            return true;
        }
        assert!(!task_inner.wake_pending, "off-CPU task retained a wakeup");
        task_inner.task_status = TaskStatus::Ready;
        let probe = task_inner.smp_sched_probe_active;
        drop(task_inner);
        enqueue_woken_task(task, front);
        if probe {
            super::smp_probe::record_wake(false);
        }
        perf::record_task_wakeup(front);
        true
    } else {
        false
    }
}

pub(super) fn enqueue_woken_task(task: Arc<TaskControlBlock>, front: bool) {
    enqueue_task_on_best_queue(task, front, false);
}

pub fn wakeup_task(task: Arc<TaskControlBlock>) -> bool {
    wakeup_task_with_placement(task, false)
}

pub(crate) fn wakeup_front_task(task: Arc<TaskControlBlock>) -> bool {
    wakeup_task_with_placement(task, true)
}

pub(crate) fn wakeup_timer_task(task: Arc<TaskControlBlock>) -> bool {
    // CONTEXT: Timer-expired sleepers need to compete promptly with runnable
    // load; otherwise shell sleeps and cyclictest wakeups sit behind hundreds
    // of hackbench workers even after their timeout has expired.
    wakeup_front_task(task)
}

pub(super) fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    let cpu = crate::cpu::current_id();
    if let Some(task) = with_run_queue(cpu, |manager| manager.fetch(cpu)) {
        return Some(task);
    }

    let cpu_count = crate::cpu::topology().possible_count();
    let candidates = crate::cpu::CpuMask::from_bits(
        RUNNABLE_QUEUES.load(Ordering::Acquire).bits()
            & crate::cpu::online_mask().bits()
            & !crate::cpu::CpuMask::single(cpu).bits(),
    );
    if candidates.bits() == 0 {
        return None;
    }
    for offset in 1..cpu_count {
        let victim = (cpu + offset) % cpu_count;
        if !candidates.contains(victim) {
            continue;
        }
        let stolen = with_run_queue(victim, |manager| {
            let source_min = manager.min_vruntime();
            manager.fetch(cpu).map(|task| (task, source_min))
        });
        if let Some((task, source_min)) = stolen {
            let target_min = with_run_queue(cpu, |manager| manager.min_vruntime());
            task.migrate_sched_vruntime(source_min, target_min);
            super::smp_probe::record_cpu_probe_steal();
            return Some(task);
        }
    }
    None
}

pub(super) fn remove_ready_tasks_of_process(process_id: usize) {
    for cpu in 0..crate::cpu::topology().possible_count() {
        with_run_queue(cpu, |manager| manager.remove_process_tasks(process_id));
    }
}

pub(crate) fn reprioritize_ready_task(task: Arc<TaskControlBlock>) {
    let queued_cpu = task.inner_exclusive_access().queued_cpu;
    if let Some(cpu) = queued_cpu {
        with_run_queue(cpu, |manager| manager.reprioritize_ready_task(task));
        crate::cpu::wake_scheduler_cpu(crate::cpu::CpuMask::single(cpu));
    }
}

pub(crate) fn migrate_ready_task(task: Arc<TaskControlBlock>) {
    let queued_cpu = task.inner_exclusive_access().queued_cpu;
    let Some(source) = queued_cpu else {
        return;
    };
    let allowed = task.inner_exclusive_access().allowed_cpus;
    let target = choose_run_queue(allowed, None);
    if source == target {
        return;
    }

    let moved = if source < target {
        let mut source_queue = RUN_QUEUES[source].lock();
        let mut target_queue = RUN_QUEUES[target].lock();
        let source_was_runnable = source_queue.ready_len() != 0;
        let target_was_runnable = target_queue.ready_len() != 0;
        let moved = migrate_ready_task_locked(&mut source_queue, &mut target_queue, task);
        publish_run_queue_transition(source, source_was_runnable, source_queue.ready_len());
        publish_run_queue_transition(target, target_was_runnable, target_queue.ready_len());
        moved
    } else {
        let mut target_queue = RUN_QUEUES[target].lock();
        let mut source_queue = RUN_QUEUES[source].lock();
        let source_was_runnable = source_queue.ready_len() != 0;
        let target_was_runnable = target_queue.ready_len() != 0;
        let moved = migrate_ready_task_locked(&mut source_queue, &mut target_queue, task);
        publish_run_queue_transition(source, source_was_runnable, source_queue.ready_len());
        publish_run_queue_transition(target, target_was_runnable, target_queue.ready_len());
        moved
    };
    if moved {
        super::smp_probe::record_cpu_probe_migration();
        crate::cpu::wake_scheduler_cpu(crate::cpu::CpuMask::single(target));
    }
}

/// Take one ordered snapshot of every possible CPU's ready queue.
///
/// This is a bounded phase-gate assertion, not a scheduler hot path. Holding
/// every queue in ascending CPU-ID order prevents a concurrent steal or
/// affinity migration from making an empty snapshot appear accidentally.
pub(super) fn assert_run_queues_drained() {
    let cpu_count = crate::cpu::topology().possible_count();
    let mut queues = Vec::with_capacity(cpu_count);
    for cpu in 0..cpu_count {
        queues.push(RUN_QUEUES[cpu].lock());
    }
    let ready = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        queues.get(cpu).map_or(0, |queue| queue.ready_len())
    });
    let total = ready[..cpu_count].iter().sum::<usize>();
    info!(
        "smp run-queue drain: ready={:?} total={total}",
        &ready[..cpu_count]
    );
    assert_eq!(total, 0, "Phase 6 gate found runnable tasks on run queues");
}

fn migrate_ready_task_locked(
    source: &mut TaskManager,
    target: &mut TaskManager,
    task: Arc<TaskControlBlock>,
) -> bool {
    if !source.remove_ready_task(&task) {
        return false;
    }
    let mut inner = task.inner_exclusive_access();
    if inner.task_status == TaskStatus::Exited {
        inner.on_rq = false;
        inner.queued_cpu = None;
        return false;
    }
    assert_eq!(inner.task_status, TaskStatus::Ready);
    assert!(inner.on_rq);
    assert_eq!(inner.queued_cpu, Some(source.cpu));
    assert!(inner.on_cpu.is_none());
    let relative = inner.sched_vruntime.saturating_sub(source.min_vruntime());
    inner.sched_vruntime = target.min_vruntime().saturating_add(relative);
    inner.queued_cpu = Some(target.cpu);
    drop(inner);
    target.enqueue(task, false);
    true
}

pub fn pid2process(pid: usize) -> Option<Arc<ProcessControlBlock>> {
    let map = PID2PCB.exclusive_access();
    map.get(&pid).map(Arc::clone)
}

pub(crate) fn processes_snapshot() -> Vec<Arc<ProcessControlBlock>> {
    let map = PID2PCB.exclusive_access();
    map.values().cloned().collect()
}

pub(crate) fn task_with_linux_tid(tid: usize) -> Option<Arc<TaskControlBlock>> {
    let mut stale_index_entry = false;

    let indexed_task = {
        let map = LINUX_TID2TASK.exclusive_access();
        map.get(&tid).cloned()
    };
    if let Some(task_ref) = indexed_task {
        if let Some(task) = task_ref.upgrade()
            && task.linux_tid() == tid
            && task.inner_exclusive_access().task_status != TaskStatus::Exited
        {
            perf::record_tid_lookup(0, 0, true, true, false);
            return Some(task);
        }
        {
            let mut map = LINUX_TID2TASK.exclusive_access();
            map.remove(&tid);
            stale_index_entry = true;
        }
    }

    let mut process_visits = 0;
    let mut task_visits = 0;
    for process in processes_snapshot() {
        process_visits += 1;
        for task in process.tasks_snapshot() {
            task_visits += 1;
            if task.linux_tid() == tid
                && task.inner_exclusive_access().task_status != TaskStatus::Exited
            {
                register_task_linux_tid(&task);
                perf::record_tid_lookup(
                    process_visits,
                    task_visits,
                    true,
                    false,
                    stale_index_entry,
                );
                return Some(task);
            }
        }
    }
    perf::record_tid_lookup(process_visits, task_visits, false, false, stale_index_entry);
    None
}

pub(super) fn register_task_linux_tid(task: &Arc<TaskControlBlock>) {
    let tid = task.linux_tid();
    LINUX_TID2TASK
        .exclusive_access()
        .insert(tid, Arc::downgrade(task));
}

pub(super) fn unregister_task_linux_tid(tid: usize) {
    LINUX_TID2TASK.exclusive_access().remove(&tid);
}

pub(crate) fn list_process_snapshots() -> Vec<ProcessProcSnapshot> {
    let processes = {
        let map = PID2PCB.exclusive_access();
        map.values().cloned().collect::<Vec<_>>()
    };
    processes
        .into_iter()
        .map(|process| process.proc_snapshot())
        .collect()
}

pub(crate) fn any_process_references_mount(mount_id: crate::fs::MountId) -> bool {
    let processes = {
        let map = PID2PCB.exclusive_access();
        map.values().cloned().collect::<Vec<_>>()
    };
    processes
        .iter()
        .any(|process| process.references_vfs_mount(mount_id))
}

pub(super) fn register_process(process: &Arc<ProcessControlBlock>) {
    PID2PCB
        .exclusive_access()
        .insert(process.getpid(), Arc::clone(process));
    for task in process.tasks_snapshot() {
        register_task_linux_tid(&task);
    }
}

pub fn remove_from_pid2process(pid: usize) {
    let mut map = PID2PCB.exclusive_access();
    let Some(process) = map.remove(&pid) else {
        panic!("cannot find pid {} in pid2task!", pid);
    };
    drop(map);
    for task in process.tasks_snapshot() {
        unregister_task_linux_tid(task.linux_tid());
    }
}
