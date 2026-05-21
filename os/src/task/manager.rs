use super::{ProcessControlBlock, ProcessProcSnapshot, TaskControlBlock, TaskStatus};
use crate::sync::UPIntrFreeCell;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;

pub struct TaskManager {
    ready_queue: VecDeque<Arc<TaskControlBlock>>,
}

/// A simple FIFO scheduler.
impl TaskManager {
    pub fn new() -> Self {
        Self {
            ready_queue: VecDeque::new(),
        }
    }
    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        self.ready_queue.push_back(task);
    }
    pub fn add_front(&mut self, task: Arc<TaskControlBlock>) {
        self.ready_queue.push_front(task);
    }
    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        // CONTEXT: Linux-visible SCHED_FIFO/RR metadata is enough for
        // cyclictest only if awakened RT tasks can run ahead of normal load.
        // UNFINISHED: This is not a full Linux RT scheduler; equal-priority RT
        // tasks and all normal tasks still keep this queue's FIFO order.
        let mut best_idx = None;
        let mut best_rt_priority = 0;
        let mut idx = 0;
        while idx < self.ready_queue.len() {
            let task = &self.ready_queue[idx];
            if task.inner_exclusive_access().task_status == TaskStatus::Exited {
                self.ready_queue.remove(idx);
                continue;
            }
            let rt_priority = task.realtime_priority();
            if best_idx.is_none() || rt_priority > best_rt_priority {
                best_idx = Some(idx);
                best_rt_priority = rt_priority;
            }
            idx += 1;
        }
        best_idx.and_then(|idx| self.ready_queue.remove(idx))
    }
    pub fn remove_process_tasks(&mut self, process_id: usize) {
        self.ready_queue.retain(|task| {
            task.process
                .upgrade()
                .is_none_or(|process| process.getpid() != process_id)
        });
    }
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
