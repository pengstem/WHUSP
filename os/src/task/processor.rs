use super::__switch;
use super::fetch_task;
use super::{ProcessControlBlock, TaskContext, TaskControlBlock, TaskStatus};
use crate::arch::hart;
use crate::config::MAX_CPUS;
use crate::perf;
use crate::sync::{SpinNoIrqLock, SpinNoIrqLockGuard};
use crate::trap::TrapContext;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

pub struct Processor {
    current: Option<Arc<TaskControlBlock>>,
    current_process: Option<Arc<ProcessControlBlock>>,
    // Cached SATP/PGDL token for the running process. Keep it synchronized
    // with set_current() and refresh_current_user_token(); syscall user-copy
    // fast paths depend on this being the active address space.
    current_user_token: usize,
    pending_switch: Option<SwitchReason>,
    idle_task_cx: TaskContext,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SwitchReason {
    Yield,
    Block,
    Exit,
}

impl Processor {
    pub const fn new() -> Self {
        Self {
            current: None,
            current_process: None,
            current_user_token: 0,
            pending_switch: None,
            idle_task_cx: TaskContext::zero_init(),
        }
    }
    fn get_idle_task_cx_ptr(&mut self) -> *mut TaskContext {
        &mut self.idle_task_cx as *mut _
    }
    pub fn set_current(&mut self, task: Arc<TaskControlBlock>) {
        assert!(self.current.is_none(), "CPU already has a current task");
        assert!(
            self.pending_switch.is_none(),
            "CPU retained a completed switch"
        );
        let process = process_of_task(&task);
        let user_token = process.inner_exclusive_access().memory_set.token();
        self.current = Some(task);
        self.current_process = Some(process);
        self.current_user_token = user_token;
    }
    pub fn current(&self) -> Option<Arc<TaskControlBlock>> {
        self.current.as_ref().map(Arc::clone)
    }
    pub fn current_process(&self) -> Option<Arc<ProcessControlBlock>> {
        self.current_process.as_ref().map(Arc::clone)
    }
    pub fn current_user_token(&self) -> Option<usize> {
        self.current_process
            .as_ref()
            .map(|_| self.current_user_token)
    }
    pub fn refresh_current_user_token(&mut self) -> Option<usize> {
        // execve replaces the process MemorySet without scheduling a new task.
        // Refresh after the image switch before any later user-copy helper
        // reads the cached token through current_user_token().
        let process = self.current_process.as_ref()?;
        let token = process.inner_exclusive_access().memory_set.token();
        self.current_user_token = token;
        Some(token)
    }
}

#[repr(C, align(64))]
struct PerCpuProcessor {
    inner: SpinNoIrqLock<Processor>,
}

impl PerCpuProcessor {
    const fn new() -> Self {
        Self {
            inner: SpinNoIrqLock::new(Processor::new()),
        }
    }
}

static PROCESSORS: [PerCpuProcessor; MAX_CPUS] = [const { PerCpuProcessor::new() }; MAX_CPUS];
static PROCESSOR_IDLE: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

fn processor() -> SpinNoIrqLockGuard<'static, Processor> {
    PROCESSORS[crate::cpu::current_id()].inner.lock()
}

pub(crate) fn processor_slot_ptr(cpu: usize) -> usize {
    assert!(cpu < MAX_CPUS, "processor slot CPU exceeds MAX_CPUS");
    &PROCESSORS[cpu] as *const PerCpuProcessor as usize
}

pub(crate) fn current_processor_is_empty() -> bool {
    let processor = processor();
    processor.current.is_none()
        && processor.current_process.is_none()
        && processor.current_user_token == 0
        && processor.pending_switch.is_none()
}

pub(crate) fn processor_is_idle(cpu: usize) -> bool {
    PROCESSOR_IDLE[cpu].load(Ordering::Acquire)
}

pub(super) fn prepare_current_switch(
    reason: SwitchReason,
) -> (Arc<TaskControlBlock>, *mut TaskContext) {
    let cpu = crate::cpu::current_id();
    let mut processor = processor();
    assert!(
        processor.pending_switch.is_none(),
        "task prepared two context switches"
    );
    let task = processor
        .current
        .as_ref()
        .map(Arc::clone)
        .expect("context switch requires a current task");
    let task_cx_ptr = {
        let mut inner = task.inner_exclusive_access();
        assert_eq!(
            inner.task_status,
            TaskStatus::Running,
            "only a Running task may prepare a switch"
        );
        assert_eq!(
            inner.on_cpu,
            Some(cpu),
            "current task is owned by another CPU"
        );
        assert!(!inner.on_rq, "current task is also on a run queue");
        assert!(!inner.wake_pending, "running task retained a wakeup");
        if reason == SwitchReason::Block {
            inner.task_status = TaskStatus::Blocked;
            if inner.smp_sched_probe_active {
                super::smp_probe::record_block();
            }
        } else if reason == SwitchReason::Exit {
            inner.task_status = TaskStatus::Exited;
        }
        &mut inner.task_cx as *mut TaskContext
    };
    processor.pending_switch = Some(reason);
    (task, task_cx_ptr)
}

fn finish_current_switch() {
    let cpu = crate::cpu::current_id();
    let (task, process, reason) = {
        let mut processor = processor();
        let reason = processor
            .pending_switch
            .take()
            .expect("idle context resumed without a pending task switch");
        let task = processor
            .current
            .take()
            .expect("pending switch lost its current task");
        let process = processor
            .current_process
            .take()
            .expect("pending switch lost its current process");
        processor.current_user_token = 0;
        (task, process, reason)
    };

    if reason == SwitchReason::Block {
        // Stop runtime accounting while on_cpu still prevents a concurrent
        // waker from publishing the task to another CPU.
        super::charge_task_after_run(&task);
    }

    let mut enqueue = None;
    let probe;
    let cpu_probe;
    let wait_io_probe;
    {
        let mut inner = task.inner_exclusive_access();
        probe = inner.smp_sched_probe;
        cpu_probe = inner.smp_cpu_probe;
        wait_io_probe = inner.smp_wait_io_probe;
        assert_eq!(
            inner.on_cpu,
            Some(cpu),
            "switch completion observed the wrong CPU owner"
        );
        assert!(
            !inner.on_rq,
            "switching task was queued before switch completion"
        );
        inner.on_cpu = None;
        match reason {
            SwitchReason::Yield => {
                assert_eq!(inner.task_status, TaskStatus::Running);
                assert!(!inner.wake_pending);
                inner.task_status = TaskStatus::Ready;
                enqueue = Some(false);
            }
            SwitchReason::Block => {
                assert_eq!(inner.task_status, TaskStatus::Blocked);
                if inner.wake_pending {
                    let front = inner.wake_front;
                    inner.wake_pending = false;
                    inner.wake_front = false;
                    inner.task_status = TaskStatus::Ready;
                    enqueue = Some(front);
                }
            }
            SwitchReason::Exit => {
                assert_eq!(inner.task_status, TaskStatus::Exited);
                assert!(!inner.wake_pending);
            }
        }
    }

    if cpu_probe {
        super::smp_probe::record_cpu_probe_switch(
            task.sched_runtime_us(crate::timer::get_time_us()),
        );
    }

    process.release_scheduler_cpu(cpu);

    if probe && reason == SwitchReason::Exit {
        super::smp_probe::record_exit();
    }
    if cpu_probe && reason == SwitchReason::Exit {
        super::smp_probe::record_cpu_probe_exit();
    }
    if wait_io_probe && reason == SwitchReason::Exit {
        super::smp_probe::record_wait_io_exit();
    }

    match reason {
        SwitchReason::Yield => super::requeue_task_after_run(task),
        SwitchReason::Block => {
            if let Some(front) = enqueue {
                super::manager::enqueue_woken_task(task, front);
            }
        }
        SwitchReason::Exit => super::queue_exited_task(task),
    }
}

pub fn run_tasks() -> ! {
    crate::cpu::scheduler_publish_active(crate::cpu::current_id());
    loop {
        let mut processor = processor();
        if let Some(task) = fetch_task() {
            let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
            // fetch_task() atomically claims the task for this CPU before it
            // becomes visible as Processor::current.
            let next_task_cx_ptr = task
                .inner
                .exclusive_session(|task_inner| &task_inner.task_cx as *const TaskContext);
            task.mark_sched_run_start(crate::timer::get_time_us());
            processor.set_current(task);
            // release processor manually
            drop(processor);
            unsafe {
                __switch(idle_task_cx_ptr, next_task_cx_ptr);
            }
            finish_current_switch();
            super::reap_exited_tasks();
        } else {
            drop(processor);
            let cpu = crate::cpu::current_id();
            PROCESSOR_IDLE[cpu].store(true, Ordering::Release);
            // Pair idle publication with enqueue's pending-before-IPI order.
            // If work arrived just before publication, do not cross into WFI.
            if crate::cpu::take_scheduler_wake(cpu) {
                PROCESSOR_IDLE[cpu].store(false, Ordering::Release);
                continue;
            }
            #[cfg(target_arch = "loongarch64")]
            // CONTEXT: LA UART IRQ dispatch is not wired yet. Poll the console
            // while idle so stdin poll/select waiters can be woken by typed data.
            crate::fs::console_tty_drain_uart();
            hart::enable_interrupt_and_wait();
            PROCESSOR_IDLE[cpu].store(false, Ordering::Release);
        }
    }
}

pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    perf::record_task_current_call();
    processor().current()
}

pub fn current_process() -> Arc<ProcessControlBlock> {
    perf::record_task_current_process_call();
    processor()
        .current_process()
        .expect("current_process requires a running task")
}

pub fn current_user_token() -> usize {
    perf::record_task_current_user_token_call();
    processor()
        .current_user_token()
        .expect("current_user_token requires a running task")
}

pub fn refresh_current_user_token() {
    processor()
        .refresh_current_user_token()
        .expect("refresh_current_user_token requires a running task");
}

pub fn current_trap_cx() -> &'static mut TrapContext {
    perf::record_task_current_trap_cx_call();
    let trap_cx_ppn = processor()
        .current
        .as_ref()
        .map(|task| task.inner_exclusive_access().trap_cx_ppn)
        .expect("current_trap_cx requires a running task");
    trap_cx_ppn.get_mut()
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
pub fn trap_return_context_after_accounting_for_task(
    task: &TaskControlBlock,
    process: &ProcessControlBlock,
    now_us: usize,
) -> (usize, usize) {
    perf::record_task_current_trap_return_context_call();
    account_trap_return_for_task(task, process, now_us);
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
pub fn trap_return_context_after_accounting_for_task(
    task: &TaskControlBlock,
    process: &ProcessControlBlock,
    now_us: usize,
) -> (usize, usize) {
    perf::record_task_current_trap_return_context_call();
    account_trap_return_for_task(task, process, now_us);
    let trap_cx = task.inner_exclusive_access().get_trap_cx() as *mut TrapContext as usize;
    let user_token = process.inner_exclusive_access().memory_set.token();
    (trap_cx, user_token)
}

pub fn current_kstack_bounds() -> (usize, usize) {
    processor()
        .current
        .as_ref()
        .map_or_else(hart::boot_stack_bounds, |task| task.kstack.bounds())
}

pub fn schedule(switched_task_cx_ptr: *mut TaskContext) {
    let idle_task_cx_ptr = processor().get_idle_task_cx_ptr();
    unsafe {
        __switch(switched_task_cx_ptr, idle_task_cx_ptr);
    }
    super::mark_current_kernel_time_entry(crate::timer::get_time_us());
}
