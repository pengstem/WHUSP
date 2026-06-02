mod context;

use crate::config::TRAMPOLINE;
use crate::mm::{MmapFaultAccess, MmapFaultResult};
use crate::syscall::{errno::SysError, syscall};
use crate::task::{
    SignalAction, SignalFlags, account_current_user_time_until, check_signals_of_current,
    current_add_signal, current_process, current_task, current_trap_cx,
    current_trap_return_context_after_accounting, exit_current_group_and_run_next,
    suspend_current_and_run_next,
};
use crate::timer::{check_timer, get_time_us, set_next_trigger};
use core::arch::{asm, global_asm};
use riscv::register::{
    mtvec::TrapMode,
    scause::{self, Exception, Interrupt, Trap},
    sie, sscratch, sstatus, stval, stvec,
};

global_asm!(include_str!("trap/trap.S"));

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

fn enable_supervisor_interrupt() {
    unsafe {
        sstatus::set_sie();
    }
}

fn disable_supervisor_interrupt() {
    unsafe {
        sstatus::clear_sie();
    }
}

#[unsafe(no_mangle)]
pub fn trap_handler() -> ! {
    set_kernel_trap_entry();
    account_current_user_time_until(get_time_us());
    let scause = scause::read();
    let stval = stval::read();
    let trap_pc = current_trap_cx().sepc;
    let mut interrupted_pc = trap_pc;
    let mut signal_delivery_attempted = false;
    // println!("into {:?}", scause.cause());
    match scause.cause() {
        Trap::Exception(Exception::UserEnvCall) => {
            let syscall_pc = trap_pc;
            let (syscall_nr, syscall_args, syscall_sp) = {
                let cx = current_trap_cx();
                // Snapshot the Linux RISC-V syscall ABI registers before
                // ptrace stops or syscall handlers can mutate TrapContext.
                (
                    cx.x[17],
                    [cx.x[10], cx.x[11], cx.x[12], cx.x[13], cx.x[14], cx.x[15]],
                    cx.x[2],
                )
            };
            crate::task::ptrace_syscall_enter_stop_current(
                syscall_nr,
                syscall_args,
                syscall_pc,
                syscall_sp,
            );
            // jump to next instruction anyway
            current_trap_cx().sepc += 4;

            enable_supervisor_interrupt();

            // get system call return value
            let result = syscall(syscall_nr, syscall_args);
            // cx is changed during sys_execve, so we have to call it again
            let cx = current_trap_cx();
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
            if crate::task::ptrace_syscall_exit_stop_current(
                result,
                syscall_exit_pc,
                syscall_exit_sp,
            ) {
                interrupted_pc = current_trap_cx().sepc;
            }
            if crate::task::ptrace_stop_current_if_needed() {
                interrupted_pc = current_trap_cx().sepc;
            }
            if crate::arch::signal::deliver_pending_signal(
                interrupted_pc,
                syscall_pc_if_interrupted,
            ) {
                trap_return();
            }
            signal_delivery_attempted = true;
        }
        Trap::Exception(Exception::StorePageFault) => {
            if !handle_user_page_fault(stval, MmapFaultAccess::Write) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::InstructionPageFault) => {
            if !handle_user_page_fault(stval, MmapFaultAccess::Execute) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::LoadPageFault) => {
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
            current_add_signal(SignalFlags::SIGILL);
        }
        Trap::Interrupt(Interrupt::SupervisorTimer) => {
            set_next_trigger();
            check_timer();
            suspend_current_and_run_next();
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
    if !signal_delivery_attempted && crate::task::ptrace_stop_current_if_needed() {
        interrupted_pc = current_trap_cx().sepc;
    }
    if !signal_delivery_attempted
        && crate::arch::signal::deliver_pending_signal(interrupted_pc, None)
    {
        trap_return();
    }
    if let Some((errno, _msg)) = check_signals_of_current() {
        exit_current_group_and_run_next(errno);
    }
    trap_return();
}

pub(crate) fn handle_user_page_fault(addr: usize, access: MmapFaultAccess) -> bool {
    if access == MmapFaultAccess::Write {
        let process = current_process();
        // Private COW pages are resolved before mmap faults so forked heap and
        // anonymous mappings preserve copy-on-write semantics.
        if process
            .inner_exclusive_access()
            .memory_set
            .resolve_cow_page_fault(addr)
        {
            return true;
        }
    }
    handle_mmap_page_fault(addr, access)
}

pub(crate) fn handle_mmap_page_fault(addr: usize, access: MmapFaultAccess) -> bool {
    let process = current_process();
    let fault = {
        let mut inner = process.inner_exclusive_access();
        inner.memory_set.prepare_mmap_page_fault(addr, access)
    };
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
            let Some(frame) = page.build_frame() else {
                return false;
            };
            let mut inner = process.inner_exclusive_access();
            inner.memory_set.install_mmap_fault_page(page, frame)
        }
        MmapFaultResult::PageCache(page) => {
            let Some(ppn) = page.resolve_ppn() else {
                return false;
            };
            let key = page.key();
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
    let now_us = get_time_us();
    let (trap_cx_user_va, user_satp) = current_trap_return_context_after_accounting(now_us);
    disable_supervisor_interrupt();
    set_user_trap_entry();
    unsafe extern "C" {
        unsafe fn __alltraps();
        unsafe fn __restore();
    }
    let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;
    //println!("before return");
    if crate::arch::mm::should_fence_i_on_trap_return() {
        crate::perf::record_riscv_return_fence_i_call();
        unsafe {
            asm!("fence.i");
        }
    }
    unsafe {
        asm!(
            "jr {restore_va}",
            restore_va = in(reg) restore_va,
            in("a0") trap_cx_user_va,
            in("a1") user_satp,
            options(noreturn)
        );
    }
}

#[unsafe(no_mangle)]
pub fn trap_from_kernel(_trap_cx: &TrapContext) {
    let scause = scause::read();
    let stval = stval::read();
    match scause.cause() {
        Trap::Interrupt(Interrupt::SupervisorExternal) => {
            crate::board::irq_handler();
        }
        Trap::Interrupt(Interrupt::SupervisorTimer) => {
            set_next_trigger();
            // CONTEXT: A kernel-mode timer interrupt can arrive while the
            // interrupted code holds non-IRQ-safe locks such as the global
            // heap allocator. `check_timer()` may drop timer events, queue
            // signals, and wake tasks, all of which can allocate/free memory;
            // only do that work from the idle loop, where no task kernel code
            // was interrupted and sleeping tasks still need timer wakeups.
            if current_task().is_none() {
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
