mod context;

use crate::arch::interrupt::{disable_supervisor_interrupt, enable_supervisor_interrupt};
use crate::config::TRAMPOLINE;
use crate::mm::{MmapFaultAccess, MmapFaultResult};
use crate::perf;
use crate::syscall::{
    errno::SysError, syscall_is_exit, syscall_is_exit_group, syscall_with_current_task,
};
use crate::task::{
    ProcessControlBlock, SignalAction, SignalFlags, TaskControlBlock, account_task_user_time_until,
    check_signals_of_task, current_add_signal, current_process, current_task,
    exit_current_group_and_run_next, process_of_task, suspend_current_and_run_next,
    timer_tick_should_preempt, trap_cx_of_task, trap_return_context_after_accounting_for_task,
};
use crate::timer::{check_timer, get_time_us, set_next_trigger};
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
    set_kernel_trap_entry();
    let mut task = current_task().expect("trap_handler requires a running task");
    let mut process = process_of_task(&task);
    account_task_user_time_until(&task, &process, get_time_us());
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
            if syscall_is_exit(syscall_nr) {
                drop(process);
                let _ = syscall_with_current_task(task, syscall_nr, syscall_args);
                unreachable!("exit syscall returned");
            }
            let result = if syscall_is_exit_group(syscall_nr) {
                drop(process);
                let result = syscall_with_current_task(task, syscall_nr, syscall_args);
                task = current_task().expect("seccomp-blocked exit_group requires a running task");
                process = process_of_task(&task);
                result
            } else {
                syscall_with_current_task(Arc::clone(&task), syscall_nr, syscall_args)
            };
            // cx is changed during sys_execve, so we have to call it again
            let cx = trap_cx_of_task(&task);
            // UNFINISHED: Full SA_RESTART is not modeled yet. Most interrupted
            // syscalls such as futex, nanosleep, clock_nanosleep, ppoll, and
            // pselect6 currently return EINTR after rt_sigreturn instead of
            // being automatically restarted; wait4/waitid only suppress EINTR
            // for restartable handlers.
            interrupted_pc = cx.sepc;
            cx.x[10] = result as usize;
            let syscall_exit_pc = cx.sepc;
            let syscall_exit_sp = cx.x[2];
            let syscall_pc_if_interrupted = if result == -(SysError::EINTR as isize) {
                Some(syscall_pc)
            } else {
                None
            };
            if crate::task::ptrace_syscall_exit_stop_for_task(
                &process,
                result,
                syscall_exit_pc,
                syscall_exit_sp,
            ) {
                interrupted_pc = trap_cx_of_task(&task).sepc;
            }
            if crate::task::ptrace_stop_task_if_needed(&task, &process) {
                interrupted_pc = trap_cx_of_task(&task).sepc;
            }
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
            crate::cpu::handle_ipi();
        }
        Trap::Interrupt(Interrupt::SupervisorTimer) => {
            set_next_trigger();
            if crate::cpu::current_id() == 0 {
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
    if !signal_delivery_attempted && crate::task::ptrace_stop_task_if_needed(&task, &process) {
        interrupted_pc = trap_cx_of_task(&task).sepc;
    }
    if !signal_delivery_attempted
        && crate::arch::signal::deliver_pending_signal(&task, &process, interrupted_pc, None)
    {
        trap_return_for_task(task, process);
    }
    if let Some((errno, _msg)) = check_signals_of_task(&task, &process) {
        drop(process);
        drop(task);
        exit_current_group_and_run_next(errno);
        unreachable!("signal-forced exit returned");
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
    let _profile_scope = perf::time_scope(perf::ProfilePoint::PageFault);
    let process = current_process();
    let fault = if access == MmapFaultAccess::Write {
        let mut inner = process.inner_exclusive_access();
        {
            let _cow_scope = perf::time_scope(perf::ProfilePoint::PageFaultCow);
            // Private COW pages are resolved before mmap faults so forked heap
            // and anonymous mappings preserve copy-on-write semantics.
            if inner.memory_set.resolve_cow_page_fault(addr) {
                return true;
            }
        }
        let _prepare_scope = perf::time_scope(perf::ProfilePoint::PageFaultMmapPrepare);
        inner.memory_set.prepare_mmap_page_fault(addr, access)
    } else {
        prepare_mmap_page_fault(&process, addr, access)
    };
    if handle_prepared_mmap_page_fault(&process, fault) {
        return true;
    }
    let _lazy_scope = perf::time_scope(perf::ProfilePoint::PageFaultLazyFramed);
    process
        .inner_exclusive_access()
        .memory_set
        .resolve_lazy_framed_page_fault(addr, access)
}

fn prepare_mmap_page_fault(
    process: &Arc<ProcessControlBlock>,
    addr: usize,
    access: MmapFaultAccess,
) -> Option<MmapFaultResult> {
    let _prepare_scope = perf::time_scope(perf::ProfilePoint::PageFaultMmapPrepare);
    let mut inner = process.inner_exclusive_access();
    inner.memory_set.prepare_mmap_page_fault(addr, access)
}

fn handle_prepared_mmap_page_fault(
    process: &Arc<ProcessControlBlock>,
    fault: Option<MmapFaultResult>,
) -> bool {
    let Some(fault) = fault else {
        return false;
    };
    match fault {
        MmapFaultResult::Handled => true,
        MmapFaultResult::FatalSigsegv => {
            force_default_sigsegv_current();
            false
        }
        MmapFaultResult::FatalSigbus => {
            // CONTEXT: The access reached a mapped mmap VMA but violated its
            // backing-object rules. Queue SIGBUS and report the fault handled so
            // the outer page-fault path does not also add SIGSEGV.
            current_add_signal(SignalFlags::SIGBUS);
            true
        }
        MmapFaultResult::Page(page) => {
            let frame = {
                let _build_scope = perf::time_scope(perf::ProfilePoint::PageFaultMmapBuildFrame);
                page.build_frame()
            };
            let Some(frame) = frame else {
                return false;
            };
            let _install_scope = perf::time_scope(perf::ProfilePoint::PageFaultMmapInstallFrame);
            let mut inner = process.inner_exclusive_access();
            inner.memory_set.install_mmap_fault_page(page, frame)
        }
        MmapFaultResult::PageCache(page) => {
            let ppn = {
                let _resolve_scope =
                    perf::time_scope(perf::ProfilePoint::PageFaultMmapResolvePageCache);
                page.resolve_ppn()
            };
            let Some(ppn) = ppn else {
                return false;
            };
            let key = page.key();
            let _install_scope =
                perf::time_scope(perf::ProfilePoint::PageFaultMmapInstallPageCache);
            let mut inner = process.inner_exclusive_access();
            let installed = inner
                .memory_set
                .install_mmap_page_cache_fault_page(page, ppn);
            if !installed {
                crate::mm::page_cache::PAGE_CACHE
                    .exclusive_access()
                    .dec_ref(key);
            }
            installed
        }
    }
}

fn force_default_sigsegv_current() {
    let signum = SignalFlags::SIGSEGV.bits().trailing_zeros() as usize;
    current_process().inner_exclusive_access().signal_actions[signum] = SignalAction::default();
    if let Some(task) = current_task() {
        task.inner_exclusive_access()
            .signal_mask
            .remove(SignalFlags::SIGSEGV);
    }
    current_add_signal(SignalFlags::SIGSEGV);
}

#[unsafe(no_mangle)]
/// set the new addr of __restore asm function in TRAMPOLINE page,
/// set the reg a0 = trap_cx_ptr, reg a1 = phy addr of usr page table,
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
    let now_us = get_time_us();
    let restore_fp = {
        let cx = trap_cx_of_task(&task);
        cx.kernel_tp = crate::cpu::current_ptr();
        cx.kernel_entry_flush =
            crate::arch::mm::should_flush_tlb_on_kernel_entry(cx.kernel_satp) as usize;
        cx.user_fp_is_dirty()
    };
    let (trap_cx_user_va, user_satp) =
        trap_return_context_after_accounting_for_task(&task, &process, now_us);
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
            in("a1") user_satp,
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
            if crate::cpu::current_id() == 0 && current_task().is_none() {
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
