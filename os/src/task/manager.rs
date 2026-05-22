use super::{ProcessControlBlock, ProcessProcSnapshot, TaskControlBlock, TaskStatus};
use crate::perf;
use crate::sync::UPIntrFreeCell;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;

const RT_PRIORITY_MAX: usize = 99;
const RT_QUEUE_COUNT: usize = RT_PRIORITY_MAX + 1;

pub struct TaskManager {
    normal_queue: VecDeque<Arc<TaskControlBlock>>,
    rt_queues: Vec<VecDeque<Arc<TaskControlBlock>>>,
    rt_ready_bitmap: u128,
}

/// A FIFO scheduler with separate realtime priority buckets.
impl TaskManager {
    pub fn new() -> Self {
        Self {
            normal_queue: VecDeque::new(),
            rt_queues: (0..RT_QUEUE_COUNT).map(|_| VecDeque::new()).collect(),
            rt_ready_bitmap: 0,
        }
    }

    fn rt_priority(task: &TaskControlBlock) -> usize {
        task.realtime_priority().clamp(0, RT_PRIORITY_MAX as i32) as usize
    }

    fn ready_len(&self) -> usize {
        self.normal_queue.len() + self.rt_queues.iter().map(VecDeque::len).sum::<usize>()
    }

    fn rt_priority_bit(priority: usize) -> u128 {
        1u128 << priority
    }

    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        self.enqueue(task, false);
    }

    pub fn add_front(&mut self, task: Arc<TaskControlBlock>) {
        self.enqueue(task, true);
    }

    fn enqueue(&mut self, task: Arc<TaskControlBlock>, front: bool) {
        let rt_priority = Self::rt_priority(&task);
        let queue = if rt_priority > 0 {
            self.rt_ready_bitmap |= Self::rt_priority_bit(rt_priority);
            &mut self.rt_queues[rt_priority]
        } else {
            &mut self.normal_queue
        };
        if front {
            queue.push_front(task);
        } else {
            queue.push_back(task);
        }
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

    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        let queue_len = self.ready_len();
        let mut scanned = 0;
        let mut pruned_exited = 0;

        'select: loop {
            while let Some(priority) = self.highest_rt_priority() {
                let Some(task) = self.rt_queues[priority].pop_front() else {
                    self.clear_rt_priority_if_empty(priority);
                    continue;
                };
                self.clear_rt_priority_if_empty(priority);
                if task.inner_exclusive_access().task_status == TaskStatus::Exited {
                    pruned_exited += 1;
                    continue;
                }
                scanned += 1;
                let current_priority = Self::rt_priority(&task);
                if current_priority == priority {
                    perf::record_scheduler_fetch(queue_len, scanned, pruned_exited);
                    return Some(task);
                }
                self.enqueue(task, false);
            }

            while let Some(task) = self.normal_queue.pop_front() {
                if task.inner_exclusive_access().task_status == TaskStatus::Exited {
                    pruned_exited += 1;
                    continue;
                }
                scanned += 1;
                if Self::rt_priority(&task) > 0 {
                    self.enqueue(task, false);
                    continue 'select;
                }
                perf::record_scheduler_fetch(queue_len, scanned, pruned_exited);
                return Some(task);
            }

            perf::record_scheduler_fetch(queue_len, scanned, pruned_exited);
            return None;
        }
    }

    pub fn remove_process_tasks(&mut self, process_id: usize) {
        self.normal_queue.retain(|task| {
            task.process
                .upgrade()
                .is_none_or(|process| process.getpid() != process_id)
        });
        for queue in &mut self.rt_queues {
            queue.retain(|task| {
                task.process
                    .upgrade()
                    .is_none_or(|process| process.getpid() != process_id)
            });
        }
        self.rebuild_rt_ready_bitmap();
    }

    fn rebuild_rt_ready_bitmap(&mut self) {
        self.rt_ready_bitmap = 0;
        for priority in 1..=RT_PRIORITY_MAX {
            if !self.rt_queues[priority].is_empty() {
                self.rt_ready_bitmap |= Self::rt_priority_bit(priority);
            }
        }
    }

    fn remove_ready_task(&mut self, task: &Arc<TaskControlBlock>) -> bool {
        if remove_task_from_queue(&mut self.normal_queue, task) {
            return true;
        }
        for priority in 1..=RT_PRIORITY_MAX {
            if remove_task_from_queue(&mut self.rt_queues[priority], task) {
                self.clear_rt_priority_if_empty(priority);
                return true;
            }
        }
        false
    }

    pub fn reprioritize_ready_task(&mut self, task: Arc<TaskControlBlock>) {
        if task.inner_exclusive_access().task_status != TaskStatus::Ready {
            return;
        }
        if self.remove_ready_task(&task) {
            self.enqueue(task, false);
        }
    }
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
    pub static ref TASK_MANAGER: UPIntrFreeCell<TaskManager> =
        unsafe { UPIntrFreeCell::new(TaskManager::new()) };
    pub static ref PID2PCB: UPIntrFreeCell<BTreeMap<usize, Arc<ProcessControlBlock>>> =
        unsafe { UPIntrFreeCell::new(BTreeMap::new()) };
}

pub fn add_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.exclusive_access().add(task);
}

fn wakeup_task_with_placement(task: Arc<TaskControlBlock>, front: bool) -> bool {
    let mut task_inner = task.inner_exclusive_access();
    if task_inner.task_status == TaskStatus::Blocked {
        task_inner.task_status = TaskStatus::Ready;
        drop(task_inner);
        if front {
            TASK_MANAGER.exclusive_access().add_front(task);
        } else {
            add_task(task);
        }
        perf::record_task_wakeup(front);
        true
    } else {
        false
    }
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

pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.exclusive_access().fetch()
}

pub(crate) fn remove_ready_tasks_of_process(process_id: usize) {
    TASK_MANAGER
        .exclusive_access()
        .remove_process_tasks(process_id);
}

pub(crate) fn reprioritize_ready_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER
        .exclusive_access()
        .reprioritize_ready_task(task);
}

pub fn pid2process(pid: usize) -> Option<Arc<ProcessControlBlock>> {
    let map = PID2PCB.exclusive_access();
    map.get(&pid).map(Arc::clone)
}

pub(crate) fn processes_snapshot() -> Vec<Arc<ProcessControlBlock>> {
    let map = PID2PCB.exclusive_access();
    map.values().cloned().collect()
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

pub fn insert_into_pid2process(pid: usize, process: Arc<ProcessControlBlock>) {
    PID2PCB.exclusive_access().insert(pid, process);
}

pub fn remove_from_pid2process(pid: usize) {
    let mut map = PID2PCB.exclusive_access();
    if map.remove(&pid).is_none() {
        panic!("cannot find pid {} in pid2task!", pid);
    }
}
