use super::id::RecycleAllocator;
use super::{FD_LIMIT, FdTableEntry, PidHandle, SignalFlags, TaskControlBlock};
use crate::fs::WorkingDir;
use crate::mm::MemorySet;
use crate::sync::{UPIntrFreeCell, UPIntrRefMut};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCpuTimesSnapshot {
    pub user_us: usize,
    pub system_us: usize,
    pub children_user_us: usize,
    pub children_system_us: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCpuTimes {
    // UNFINISHED: CPU accounting is process-wide and trap-boundary based;
    // exact per-thread aggregation, scheduler tick attribution, and
    // signal/job-control resource accounting are not modeled yet.
    user_us: usize,
    system_us: usize,
    children_user_us: usize,
    children_system_us: usize,
    last_user_enter_us: Option<usize>,
    last_kernel_enter_us: Option<usize>,
}

impl ProcessCpuTimes {
    pub fn mark_user_entry(&mut self, now_us: usize) {
        self.last_user_enter_us = Some(now_us);
        self.last_kernel_enter_us = None;
    }

    pub fn mark_kernel_entry(&mut self, now_us: usize) {
        self.last_kernel_enter_us = Some(now_us);
        self.last_user_enter_us = None;
    }

    pub fn account_user_until(&mut self, now_us: usize) {
        if let Some(start_us) = self.last_user_enter_us.take() {
            self.user_us = self.user_us.saturating_add(now_us.saturating_sub(start_us));
        }
        self.last_kernel_enter_us = Some(now_us);
    }

    pub fn account_system_until(&mut self, now_us: usize) {
        if let Some(start_us) = self.last_kernel_enter_us.take() {
            self.system_us = self
                .system_us
                .saturating_add(now_us.saturating_sub(start_us));
        }
        self.last_kernel_enter_us = Some(now_us);
    }

    pub fn add_waited_child(&mut self, child: ProcessCpuTimesSnapshot) {
        self.children_user_us = self
            .children_user_us
            .saturating_add(child.user_us)
            .saturating_add(child.children_user_us);
        self.children_system_us = self
            .children_system_us
            .saturating_add(child.system_us)
            .saturating_add(child.children_system_us);
    }

    pub fn snapshot(&self) -> ProcessCpuTimesSnapshot {
        ProcessCpuTimesSnapshot {
            user_us: self.user_us,
            system_us: self.system_us,
            children_user_us: self.children_user_us,
            children_system_us: self.children_system_us,
        }
    }
}

pub struct ProcessControlBlock {
    // immutable
    pub pid: PidHandle,
    // mutable
    pub(super) inner: UPIntrFreeCell<ProcessControlBlockInner>,
}

pub struct ProcessControlBlockInner {
    pub is_zombie: bool,
    pub memory_set: MemorySet,
    pub cwd: WorkingDir,
    pub cwd_path: String,
    pub parent: Option<Weak<ProcessControlBlock>>,
    pub children: Vec<Arc<ProcessControlBlock>>,
    pub exit_code: i32,
    pub fd_table: Vec<Option<FdTableEntry>>,
    pub signals: SignalFlags,
    pub cpu_times: ProcessCpuTimes,
    pub tasks: Vec<Option<Arc<TaskControlBlock>>>,
    pub task_res_allocator: RecycleAllocator,
}

impl ProcessControlBlockInner {
    #[allow(unused)]
    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }

    pub fn alloc_fd(&mut self) -> usize {
        self.alloc_fd_from(0).expect("fd table exhausted")
    }

    pub fn alloc_fd_from(&mut self, lower_bound: usize) -> Option<usize> {
        if lower_bound >= FD_LIMIT {
            return None;
        }
        if let Some(fd) =
            (lower_bound..self.fd_table.len().min(FD_LIMIT)).find(|fd| self.fd_table[*fd].is_none())
        {
            Some(fd)
        } else {
            let fd = self.fd_table.len().max(lower_bound);
            if fd >= FD_LIMIT {
                return None;
            }
            while self.fd_table.len() <= fd {
                self.fd_table.push(None);
            }
            Some(fd)
        }
    }

    pub fn alloc_tid(&mut self) -> usize {
        self.task_res_allocator.alloc()
    }

    pub fn dealloc_tid(&mut self, tid: usize) {
        self.task_res_allocator.dealloc(tid)
    }

    pub fn thread_count(&self) -> usize {
        self.tasks.len()
    }

    pub fn get_task(&self, tid: usize) -> Arc<TaskControlBlock> {
        self.tasks[tid].as_ref().unwrap().clone()
    }
}

impl ProcessControlBlock {
    pub fn inner_exclusive_access(&self) -> UPIntrRefMut<'_, ProcessControlBlockInner> {
        self.inner.exclusive_access()
    }

    pub fn working_dir(&self) -> WorkingDir {
        self.inner.exclusive_access().cwd
    }

    pub fn working_dir_path(&self) -> String {
        self.inner.exclusive_access().cwd_path.clone()
    }

    pub fn set_working_dir(&self, cwd: WorkingDir, cwd_path: String) {
        let mut inner = self.inner.exclusive_access();
        inner.cwd = cwd;
        inner.cwd_path = cwd_path;
    }

    pub fn getpid(&self) -> usize {
        self.pid.0
    }

    pub fn parent_process(&self) -> Option<Arc<Self>> {
        self.inner
            .exclusive_access()
            .parent
            .as_ref()
            .and_then(Weak::upgrade)
    }

    pub fn getppid(&self) -> usize {
        self.parent_process().map_or(0, |parent| parent.getpid())
    }

    pub fn mark_user_time_entry(&self, now_us: usize) {
        self.inner_exclusive_access()
            .cpu_times
            .mark_user_entry(now_us);
    }

    pub fn mark_kernel_time_entry(&self, now_us: usize) {
        self.inner_exclusive_access()
            .cpu_times
            .mark_kernel_entry(now_us);
    }

    pub fn account_user_time_until(&self, now_us: usize) {
        self.inner_exclusive_access()
            .cpu_times
            .account_user_until(now_us);
    }

    pub fn account_system_time_until(&self, now_us: usize) {
        self.inner_exclusive_access()
            .cpu_times
            .account_system_until(now_us);
    }

    pub fn cpu_times_snapshot(&self) -> ProcessCpuTimesSnapshot {
        self.inner_exclusive_access().cpu_times.snapshot()
    }
}
