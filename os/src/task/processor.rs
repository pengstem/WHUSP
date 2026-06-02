use super::__switch;
use super::{ProcessControlBlock, TaskContext, TaskControlBlock};
use super::{TaskStatus, fetch_task};
use crate::arch::hart;
use crate::perf;
use crate::sync::UPIntrFreeCell;
use crate::trap::TrapContext;
use alloc::sync::Arc;
use lazy_static::*;

pub struct Processor {
    current: Option<Arc<TaskControlBlock>>,
    idle_task_cx: TaskContext,
}

impl Processor {
    pub fn new() -> Self {
        Self {
            current: None,
            idle_task_cx: TaskContext::zero_init(),
        }
    }
    fn get_idle_task_cx_ptr(&mut self) -> *mut TaskContext {
        &mut self.idle_task_cx as *mut _
    }
    pub fn take_current(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.current.take()
    }
    pub fn current(&self) -> Option<Arc<TaskControlBlock>> {
        self.current.as_ref().map(Arc::clone)
    }
}

lazy_static! {
    pub static ref PROCESSOR: UPIntrFreeCell<Processor> =
        unsafe { UPIntrFreeCell::new(Processor::new()) };
}

pub fn run_tasks() {
    loop {
        let mut processor = PROCESSOR.exclusive_access();
        if let Some(task) = fetch_task() {
            let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
            // access coming task TCB exclusively
            let next_task_cx_ptr = task.inner.exclusive_session(|task_inner| {
                task_inner.task_status = TaskStatus::Running;
                &task_inner.task_cx as *const TaskContext
            });
            processor.current = Some(task);
            // release processor manually
            drop(processor);
            unsafe {
                __switch(idle_task_cx_ptr, next_task_cx_ptr);
            }
            super::reap_exited_tasks();
        } else {
            drop(processor);
            #[cfg(target_arch = "loongarch64")]
            // CONTEXT: LA UART IRQ dispatch is not wired yet. Poll the console
            // while idle so stdin poll/select waiters can be woken by typed data.
            crate::fs::console_tty_drain_uart();
            hart::enable_interrupt_and_wait();
        }
    }
}

pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    PROCESSOR.exclusive_access().take_current()
}

pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    perf::record_task_current_call();
    PROCESSOR.exclusive_access().current()
}

pub fn current_process() -> Arc<ProcessControlBlock> {
    perf::record_task_current_process_call();
    let task = current_task().expect("current_process requires a running task");
    process_of_task(&task)
}

pub fn current_user_token() -> usize {
    perf::record_task_current_user_token_call();
    let task = current_task().expect("current_user_token requires a running task");
    task.get_user_token()
}

pub fn current_trap_cx() -> &'static mut TrapContext {
    perf::record_task_current_trap_cx_call();
    let task = current_task().expect("current_trap_cx requires a running task");
    trap_cx_of_task(&task)
}

pub fn process_of_task(task: &TaskControlBlock) -> Arc<ProcessControlBlock> {
    task.process
        .upgrade()
        .expect("current task process must outlive the task")
}

pub fn trap_cx_of_task(task: &TaskControlBlock) -> &'static mut TrapContext {
    task.inner_exclusive_access().get_trap_cx()
}

fn account_trap_return_for_task(
    task: &TaskControlBlock,
    process: &ProcessControlBlock,
    now_us: usize,
) {
    task.account_system_time_until(now_us);
    process.account_system_time_until(now_us);
    task.mark_user_time_entry(now_us);
    process.mark_user_time_entry(now_us);
}

#[cfg(target_arch = "riscv64")]
pub fn current_trap_return_context_after_accounting(now_us: usize) -> (usize, usize) {
    perf::record_task_current_trap_return_context_call();
    let task = current_task().expect("current_trap_return_context requires a running task");
    let process = task
        .process
        .upgrade()
        .expect("current task process must outlive the task");
    account_trap_return_for_task(&task, &process, now_us);
    let trap_cx_user_va = task
        .inner_exclusive_access()
        .res
        .as_ref()
        .expect("current user task must own TaskUserRes")
        .trap_cx_user_va();
    let user_token = process.inner_exclusive_access().memory_set.token();
    (trap_cx_user_va, user_token)
}

#[cfg(target_arch = "loongarch64")]
pub fn current_trap_return_context_after_accounting(now_us: usize) -> (usize, usize) {
    perf::record_task_current_trap_return_context_call();
    let task = current_task().expect("current_trap_return_context requires a running task");
    let process = task
        .process
        .upgrade()
        .expect("current task process must outlive the task");
    account_trap_return_for_task(&task, &process, now_us);
    let trap_cx = task.inner_exclusive_access().get_trap_cx() as *mut TrapContext as usize;
    let user_token = process.inner_exclusive_access().memory_set.token();
    (trap_cx, user_token)
}

pub fn current_kstack_top() -> usize {
    if let Some(task) = current_task() {
        task.kstack.get_top()
    } else {
        hart::boot_stack_top()
    }
}

pub fn schedule(switched_task_cx_ptr: *mut TaskContext) {
    let idle_task_cx_ptr =
        PROCESSOR.exclusive_session(|processor| processor.get_idle_task_cx_ptr());
    unsafe {
        __switch(switched_task_cx_ptr, idle_task_cx_ptr);
    }
    super::mark_current_kernel_time_entry(crate::timer::get_time_us());
}
