use super::{ProcessControlBlock, ProcessProcSnapshot, TaskControlBlock, TaskStatus};
use crate::perf;
use crate::sync::UPIntrFreeCell;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use lazy_static::lazy_static;

lazy_static! {
    static ref PID2PCB: UPIntrFreeCell<BTreeMap<usize, Arc<ProcessControlBlock>>> =
        unsafe { UPIntrFreeCell::new(BTreeMap::new()) };
    static ref LINUX_TID2TASK: UPIntrFreeCell<BTreeMap<usize, Weak<TaskControlBlock>>> =
        unsafe { UPIntrFreeCell::new(BTreeMap::new()) };
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
