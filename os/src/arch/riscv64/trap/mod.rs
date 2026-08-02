mod context;

use crate::arch::interrupt::{disable_supervisor_interrupt, enable_supervisor_interrupt};
use crate::config::TRAMPOLINE;
use crate::mm::{
    FaultOrigin, MmapFaultAccess, UserFaultFatal, UserFaultOutcome, record_fault_retry,
    record_fault_retry_chain, record_fault_retry_wait, resolve_user_page_fault,
};
use crate::syscall::{
    syscall_exit_with_current_task, syscall_is_exit, syscall_is_exit_group,
    syscall_with_current_task,
};
use crate::task::{
    SignalAction, SignalFlags, TaskControlBlock, check_signals_of_task, current_add_signal,
    current_process, current_task, exit_current_group_and_run_next, process_of_task,
    suspend_current_and_run_next, timer_tick_should_preempt, trap_cx_of_task,
    trap_return_context_after_accounting_for_task,
};
use crate::timer::{check_timer, set_next_trigger};
use crate::uapi::errno::Errno;
#[cfg(feature = "precise-cpu-accounting")]
use crate::{task::account_task_user_time_until, timer::get_time_us};
use alloc::sync::Arc;
use core::arch::{asm, global_asm};
use riscv::register::{
    mtvec::TrapMode,
    scause::{self, Exception, Interrupt, Trap},
    sie, sscratch, stval, stvec,
};

global_asm!(include_str!("trap.S"));

pub fn init() {
    set_kernel_trap_entry();
}

fn set_kernel_trap_entry() {
    unsafe extern "C" {
        unsafe fn __alltraps();
        unsafe fn __alltraps_k();
    }
    let __alltraps_k_va = __alltraps_k as usize - __alltraps as usize + TRAMPOLINE;
    unsafe {
        stvec::write(__alltraps_k_va, TrapMode::Direct);
        sscratch::write(trap_from_kernel as usize);
    }
}

fn set_user_trap_entry() {
    unsafe {
        stvec::write(TRAMPOLINE, TrapMode::Direct);
    }
}

pub fn enable_timer_interrupt() {
    unsafe {
        sie::set_stimer();
    }
}

#[unsafe(no_mangle)]
pub fn trap_handler() -> ! {
    crate::perf::record_rv_user_trap_entry();
    set_kernel_trap_entry();
    let mut task = current_task().expect("trap_handler requires a running task");
    let mut process = process_of_task(&task);
    #[cfg(feature = "precise-cpu-accounting")]
    account_task_user_time_until(&task, get_time_us());
    let scause = scause::read();
    let stval = stval::read();
    let is_user_ecall = matches!(scause.cause(), Trap::Exception(Exception::UserEnvCall));
    let (trap_pc, syscall_entry, user_fp_was_off) = {
        let cx = trap_cx_of_task(&task);
        let user_fp_was_off = cx.user_fp_is_off();
        if cx.user_fp_is_dirty() {
            crate::perf::record_rv_user_fp_save_call();
        }
        let syscall_entry = if is_user_ecall {
            // Snapshot the Linux RISC-V syscall ABI registers before ptrace
            // stops or syscall handlers can mutate TrapContext.
            Some((
                cx.x[17],
                [cx.x[10], cx.x[11], cx.x[12], cx.x[13], cx.x[14], cx.x[15]],
                cx.x[2],
            ))
        } else {
            None
        };
        (cx.sepc, syscall_entry, user_fp_was_off)
    };
    let mut interrupted_pc = trap_pc;
    let mut signal_delivery_attempted = false;
    // println!("into {:?}", scause.cause());
    match scause.cause() {
        Trap::Exception(Exception::UserEnvCall) => {
            let syscall_pc = trap_pc;
            let (syscall_nr, syscall_args, syscall_sp) =
                syscall_entry.expect("syscall entry snapshot must exist for UserEnvCall");
            #[cfg(not(feature = "ptrace"))]
            let _ = syscall_sp;
            #[cfg(feature = "ptrace")]
            crate::task::ptrace_syscall_enter_stop_for_task(
                &process,
                syscall_nr,
                syscall_args,
                syscall_pc,
                syscall_sp,
            );
            // jump to next instruction anyway
            trap_cx_of_task(&task).sepc += 4;

            enable_supervisor_interrupt();

            // Exit handlers tear down task/process state and may remove this
            // process from global lookup tables. Release the trap-local Arc
            // before entering them so cleanup and reap paths do not observe a
            // process kept alive only by this handler frame.
            if syscall_is_exit(syscall_nr) || syscall_is_exit_group(syscall_nr) {
                drop(process);
                syscall_exit_with_current_task(task, syscall_nr, syscall_args);
            }
            let outcome = syscall_with_current_task(task, process, syscall_nr, syscall_args);
            task = outcome.task;
            process = outcome.process;
            let result = outcome.result;
            // cx is changed during sys_execve, so we have to call it again
            let cx = trap_cx_of_task(&task);
            // UNFINISHED: Full SA_RESTART is not modeled yet. Most interrupted
            // syscalls such as futex, nanosleep, clock_nanosleep, ppoll, and
            // pselect6 currently return EINTR after rt_sigreturn instead of
            // being automatically restarted; wait4/waitid only suppress EINTR
            // for restartable handlers.
            interrupted_pc = cx.sepc;
            cx.x[10] = result as usize;
            #[cfg(feature = "ptrace")]
            let syscall_exit_pc = cx.sepc;
            #[cfg(feature = "ptrace")]
            let syscall_exit_sp = cx.x[2];
            let syscall_pc_if_interrupted = if result == -(Errno::EINTR as isize) {
                Some(syscall_pc)
            } else {
                None
            };
            #[cfg(feature = "ptrace")]
            if crate::task::ptrace_syscall_exit_stop_for_task(
                &process,
                result,
                syscall_exit_pc,
                syscall_exit_sp,
            ) {
                interrupted_pc = trap_cx_of_task(&task).sepc;
            }
            #[cfg(feature = "ptrace")]
            if crate::task::ptrace_stop_task_if_needed(&task, &process) {
                interrupted_pc = trap_cx_of_task(&task).sepc;
            }
            crate::task::stop_current_task_if_needed();
            if crate::arch::signal::deliver_pending_signal(
                &task,
                &process,
                interrupted_pc,
                syscall_pc_if_interrupted,
            ) {
                trap_return_for_task(task, process);
            }
            signal_delivery_attempted = true;
        }
        Trap::Exception(Exception::StorePageFault) => {
            enable_supervisor_interrupt();
            if !handle_user_page_fault(stval, MmapFaultAccess::Write) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::InstructionPageFault) => {
            enable_supervisor_interrupt();
            if !handle_user_page_fault(stval, MmapFaultAccess::Execute) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::LoadPageFault) => {
            enable_supervisor_interrupt();
            if !handle_user_page_fault(stval, MmapFaultAccess::Read) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::StoreFault)
        | Trap::Exception(Exception::InstructionFault)
        | Trap::Exception(Exception::LoadFault) => {
            /*
            println!(
                "[kernel] {:?} in application, bad addr = {:#x}, bad instruction = {:#x}, kernel killed it.",
                scause.cause(),
                stval,
                current_trap_cx().sepc,
            );
            */
            current_add_signal(SignalFlags::SIGSEGV);
        }
        Trap::Exception(Exception::IllegalInstruction) => {
            if user_fp_was_off {
                init_lazy_fp_for_task(&task);
                trap_return_for_task(task, process);
            }
            current_add_signal(SignalFlags::SIGILL);
        }
        Trap::Interrupt(Interrupt::SupervisorSoft) => {
            crate::arch::smp::clear_local_ipi();
            if crate::shutdown::stop_requested() {
                crate::shutdown::stop_current_cpu();
            }
            crate::cpu::handle_ipi();
            if timer_tick_should_preempt(&task) {
                suspend_current_and_run_next();
            }
        }
        Trap::Interrupt(Interrupt::SupervisorTimer) => {
            set_next_trigger();
            if crate::cpu::is_timer_expiry_owner() {
                check_timer();
            }
            if timer_tick_should_preempt(&task) {
                suspend_current_and_run_next();
            }
        }
        Trap::Interrupt(Interrupt::SupervisorExternal) => {
            crate::board::irq_handler();
        }
        _ => {
            panic!(
                "Unsupported trap {:?}, stval = {:#x}!",
                scause.cause(),
                stval
            );
        }
    }
    #[cfg(feature = "ptrace")]
    if !signal_delivery_attempted && crate::task::ptrace_stop_task_if_needed(&task, &process) {
        interrupted_pc = trap_cx_of_task(&task).sepc;
    }
    crate::task::stop_current_task_if_needed();
    if !signal_delivery_attempted
        && crate::arch::signal::deliver_pending_signal(&task, &process, interrupted_pc, None)
    {
        trap_return_for_task(task, process);
    }
    if let Some((errno, _msg)) = check_signals_of_task(&task, &process) {
        drop(process);
        drop(task);
        exit_current_group_and_run_next(errno);
    }
    trap_return_for_task(task, process);
}

fn init_lazy_fp_for_task(task: &Arc<TaskControlBlock>) {
    let cx = trap_cx_of_task(task);
    if !cx.user_fp_is_off() {
        return;
    }
    cx.mark_user_fp_active();
    crate::perf::record_rv_user_fp_lazy_init_trap();
}

pub(crate) fn handle_user_page_fault(addr: usize, access: MmapFaultAccess) -> bool {
    let process = current_process();
    match resolve_user_page_fault(&process, addr, access, FaultOrigin::Hardware) {
        UserFaultOutcome::Resolved => true,
        UserFaultOutcome::Retry(retry) => {
            record_fault_retry(FaultOrigin::Hardware, addr, access, &retry);
            record_fault_retry_chain(FaultOrigin::Hardware, 1);
            if let Some(waited_us) = retry.wait() {
                record_fault_retry_wait(FaultOrigin::Hardware, waited_us);
            }
            true
        }
        UserFaultOutcome::Fatal(UserFaultFatal::ForcedDefaultSegv) => {
            force_default_sigsegv_current();
            false
        }
        UserFaultOutcome::Fatal(UserFaultFatal::Bus) => {
            // CONTEXT: The access reached a mapped mmap VMA but violated its
            // backing-object rules. Queue SIGBUS and report the fault handled so
            // the outer page-fault path does not also add SIGSEGV.
            current_add_signal(SignalFlags::SIGBUS);
            true
        }
        UserFaultOutcome::Fatal(UserFaultFatal::Segv) => false,
    }
}

fn force_default_sigsegv_current() {
    let signum = SignalFlags::SIGSEGV.bits().trailing_zeros() as usize;
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    process_inner.signal_actions[signum] = SignalAction::default();
    process.publish_signal_action_masks_locked(&process_inner.signal_actions);
    drop(process_inner);
    if let Some(task) = current_task() {
        task.inner_exclusive_access()
            .signal_mask
            .remove(SignalFlags::SIGSEGV);
    }
    current_add_signal(SignalFlags::SIGSEGV);
}

#[unsafe(no_mangle)]
/// set the new addr of __restore asm function in TRAMPOLINE page,
/// set a0 to the user TrapContext and pass the return-time flush decision,
/// finally, jump to new addr of __restore asm function
pub fn trap_return() -> ! {
    let task = current_task().expect("trap_return requires a running task");
    let process = process_of_task(&task);
    trap_return_for_task(task, process)
}

fn trap_return_for_task(
    task: Arc<TaskControlBlock>,
    process: Arc<crate::task::ProcessControlBlock>,
) -> ! {
    crate::task::preempt_current_if_needed_on_user_return();
    let restore_fp = {
        let cx = trap_cx_of_task(&task);
        cx.kernel_tp = crate::cpu::current_ptr();
        cx.user_fp_is_dirty()
    };
    let (trap_cx_user_va, user_satp) = trap_return_context_after_accounting_for_task(&task);
    let flush_tlb = crate::arch::mm::should_flush_tlb_on_return(user_satp);
    if restore_fp {
        crate::perf::record_rv_user_fp_restore_call();
    }
    drop(process);
    drop(task);
    disable_supervisor_interrupt();
    set_user_trap_entry();
    unsafe extern "C" {
        unsafe fn __alltraps();
        unsafe fn __restore();
    }
    let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;
    //println!("before return");
    unsafe {
        asm!(
            "jr {restore_va}",
            restore_va = in(reg) restore_va,
            in("a0") trap_cx_user_va,
            in("a2") restore_fp as usize,
            in("a3") flush_tlb as usize,
            options(noreturn)
        );
    }
}

#[unsafe(no_mangle)]
pub fn trap_from_kernel(_trap_cx: &TrapContext) {
    let scause = scause::read();
    let stval = stval::read();
    match scause.cause() {
        Trap::Interrupt(Interrupt::SupervisorSoft) => {
            crate::arch::smp::clear_local_ipi();
            if crate::shutdown::stop_requested() {
                crate::shutdown::stop_current_cpu();
            }
            crate::cpu::handle_ipi();
        }
        Trap::Interrupt(Interrupt::SupervisorExternal) => {
            crate::board::irq_handler();
        }
        Trap::Interrupt(Interrupt::SupervisorTimer) => {
            set_next_trigger();
            if crate::cpu::is_parked_secondary() {
                return;
            }
            // CONTEXT: A kernel-mode timer interrupt can arrive while the
            // interrupted code holds non-IRQ-safe locks such as the global
            // heap allocator. `check_timer()` may drop timer events, queue
            // signals, and wake tasks, all of which can allocate/free memory;
            // only do that work from the idle loop, where no task kernel code
            // was interrupted and sleeping tasks still need timer wakeups.
            if crate::cpu::is_timer_expiry_owner() && current_task().is_none() {
                check_timer();
            }
        }
        _ => {
            panic!(
                "Unsupported trap from kernel: {:?}, stval = {:#x}!",
                scause.cause(),
                stval
            );
        }
    }
}

pub use context::TrapContext;
