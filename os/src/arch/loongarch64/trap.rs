mod context;

use crate::config::TRAMPOLINE;
use crate::mm::{MmapFaultAccess, MmapFaultResult};
use crate::syscall::{syscall_is_exit, syscall_is_exit_group, syscall_with_current_task};
use crate::task::{
    SignalAction, SignalFlags, account_task_user_time_until, check_signals_of_task,
    current_add_signal, current_process, current_task,
    current_trap_return_context_after_accounting, exit_current_group_and_run_next, process_of_task,
    suspend_current_and_run_next, trap_cx_of_task,
};
use crate::timer::{check_timer, get_time_us, set_next_trigger};
use alloc::sync::Arc;
use core::arch::global_asm;
use loongArch64::register::{
    badv, ecfg,
    ecfg::LineBasedInterrupt,
    eentry,
    estat::{self, Exception, Interrupt, Trap},
    euen, stlbps, ticlr, tlbidx, tlbrehi, tlbrentry,
};
use loongArch64::register::{pwch, pwcl};

global_asm!(include_str!("trap/trap.S"));

pub fn init() {
    // CONTEXT: The LoongArch contest userland is built with the lp64d ABI.
    // Keep the FP unit enabled and save its state at user trap boundaries.
    euen::set_fpe(true);
    tlb_init();
    set_kernel_trap_entry();
}

fn set_kernel_trap_entry() {
    unsafe extern "C" {
        safe fn __trap_vector_base();
    }
    ecfg::set_vs(0);
    eentry::set_eentry(__trap_vector_base as usize);
}

pub fn enable_timer_interrupt() {
    ecfg::set_lie(ecfg::read().lie() | LineBasedInterrupt::TIMER);
}

pub fn enable_external_interrupt() {
    // CONTEXT: QEMU LoongArch virt routes external device interrupts through
    // EIOINTC. Different references number the CPU input by DTB cell or CSR
    // bit, so enable all hardware interrupt lines and let EIOINTC/PCH PIC
    // filter actual device vectors.
    let interrupts = LineBasedInterrupt::HWI0
        | LineBasedInterrupt::HWI1
        | LineBasedInterrupt::HWI2
        | LineBasedInterrupt::HWI3
        | LineBasedInterrupt::HWI4
        | LineBasedInterrupt::HWI5
        | LineBasedInterrupt::HWI6
        | LineBasedInterrupt::HWI7;
    ecfg::set_lie(ecfg::read().lie() | interrupts);
}

fn enable_supervisor_interrupt() {
    crate::arch::interrupt::enable_supervisor_interrupt();
}

fn disable_supervisor_interrupt() {
    crate::arch::interrupt::disable_supervisor_interrupt();
}

const PS_4K: usize = 0x0c;
const PAGE_SIZE_SHIFT: usize = 12;

fn tlb_init() {
    unsafe extern "C" {
        safe fn __tlb_refill();
    }
    // CONTEXT: These CSR fields describe the three-level 4 KiB page-table
    // layout produced by `PageTable`. The refill assembly depends on this
    // exact walker geometry before user traps can resolve TLB misses.
    tlbidx::set_ps(PS_4K);
    stlbps::set_ps(PS_4K);
    tlbrehi::set_ps(PS_4K);
    pwcl::set_pte_width(8);
    pwcl::set_ptbase(PAGE_SIZE_SHIFT);
    pwcl::set_ptwidth(PAGE_SIZE_SHIFT - 3);
    pwcl::set_dir1_base(PAGE_SIZE_SHIFT + PAGE_SIZE_SHIFT - 3);
    pwcl::set_dir1_width(PAGE_SIZE_SHIFT - 3);
    pwch::set_dir3_base(PAGE_SIZE_SHIFT + PAGE_SIZE_SHIFT - 3 + PAGE_SIZE_SHIFT - 3);
    pwch::set_dir3_width(PAGE_SIZE_SHIFT - 3);
    tlbrentry::set_tlbrentry(__tlb_refill as usize & 0x0000_ffff_ffff_ffff);
}

#[unsafe(no_mangle)]
pub fn trap_handler() -> ! {
    let mut task = current_task().expect("trap_handler requires a running task");
    let mut process = process_of_task(&task);
    account_task_user_time_until(&task, &process, get_time_us());
    let estat = estat::read();
    let badv = badv::read().vaddr();
    let is_syscall = matches!(estat.cause(), Trap::Exception(Exception::Syscall));
    let (trap_pc, syscall_entry) = {
        let cx = trap_cx_of_task(&task);
        let syscall_entry = if is_syscall {
            // Snapshot the contest LoongArch syscall ABI registers before
            // ptrace stops or syscall handlers can mutate TrapContext.
            Some((
                cx.x[11],
                [cx.x[4], cx.x[5], cx.x[6], cx.x[7], cx.x[8], cx.x[9]],
                cx.x[3],
            ))
        } else {
            None
        };
        (cx.era, syscall_entry)
    };
    let mut interrupted_pc = trap_pc;
    match estat.cause() {
        Trap::Exception(Exception::Syscall) => {
            let syscall_pc = trap_pc;
            let (syscall_nr, syscall_args, syscall_sp) =
                syscall_entry.expect("syscall entry snapshot must exist for Syscall");
            crate::task::ptrace_syscall_enter_stop_for_task(
                &process,
                syscall_nr,
                syscall_args,
                syscall_pc,
                syscall_sp,
            );
            trap_cx_of_task(&task).era += 4;
            enable_supervisor_interrupt();
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
            let cx = trap_cx_of_task(&task);
            interrupted_pc = cx.era;
            cx.x[4] = result as usize;
            let syscall_exit_pc = cx.era;
            let syscall_exit_sp = cx.x[3];
            if crate::task::ptrace_syscall_exit_stop_for_task(
                &process,
                result,
                syscall_exit_pc,
                syscall_exit_sp,
            ) {
                interrupted_pc = trap_cx_of_task(&task).era;
            }
        }
        Trap::Exception(Exception::StorePageFault)
        | Trap::Exception(Exception::PageModifyFault) => {
            if !handle_user_page_fault(badv, MmapFaultAccess::Write) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::FetchPageFault)
        | Trap::Exception(Exception::PageNonExecutableFault)
        | Trap::Exception(Exception::FetchInstructionAddressError) => {
            if !handle_user_page_fault(badv, MmapFaultAccess::Execute) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::LoadPageFault)
        | Trap::Exception(Exception::PageNonReadableFault)
        | Trap::Exception(Exception::MemoryAccessAddressError)
        | Trap::Exception(Exception::PagePrivilegeIllegal) => {
            if !handle_user_page_fault(badv, MmapFaultAccess::Read) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::InstructionNotExist)
        | Trap::Exception(Exception::InstructionPrivilegeIllegal) => {
            current_add_signal(SignalFlags::SIGILL);
        }
        Trap::Interrupt(Interrupt::Timer) => {
            ticlr::clear_timer_interrupt();
            set_next_trigger();
            check_timer();
            suspend_current_and_run_next();
        }
        Trap::Interrupt(
            Interrupt::HWI0
            | Interrupt::HWI1
            | Interrupt::HWI2
            | Interrupt::HWI3
            | Interrupt::HWI4
            | Interrupt::HWI5
            | Interrupt::HWI6
            | Interrupt::HWI7,
        ) => {
            crate::board::irq_handler();
        }
        other => {
            panic!(
                "Unsupported LoongArch trap {:?}, badv = {:#x}!",
                other, badv
            );
        }
    }
    if crate::task::ptrace_stop_task_if_needed(&task, &process) {
        interrupted_pc = trap_cx_of_task(&task).era;
    }
    if crate::arch::signal::deliver_pending_signal(&task, &process, interrupted_pc) {
        drop(process);
        drop(task);
        trap_return();
    }
    if let Some((errno, _msg)) = check_signals_of_task(&task, &process) {
        drop(process);
        drop(task);
        exit_current_group_and_run_next(errno);
        unreachable!("signal-forced exit returned");
    }
    drop(process);
    drop(task);
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
pub fn trap_return() -> ! {
    let now_us = get_time_us();
    let (trap_cx, user_token) = current_trap_return_context_after_accounting(now_us);
    let flush_tlb = crate::arch::mm::should_flush_tlb_on_return(user_token);
    if flush_tlb {
        crate::perf::record_la_return_invtlb_call();
    }
    disable_supervisor_interrupt();
    set_kernel_trap_entry();
    unsafe extern "C" {
        unsafe fn __restore(trap_cx: usize, user_token: usize, flush_tlb: usize) -> !;
    }
    unsafe { __restore(trap_cx, user_token, flush_tlb as usize) }
}

#[unsafe(no_mangle)]
pub fn trap_from_kernel(_trap_cx: &TrapContext) {
    let estat = estat::read();
    let badv = badv::read().vaddr();
    match estat.cause() {
        Trap::Interrupt(Interrupt::Timer) => {
            ticlr::clear_timer_interrupt();
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
        Trap::Interrupt(
            Interrupt::HWI0
            | Interrupt::HWI1
            | Interrupt::HWI2
            | Interrupt::HWI3
            | Interrupt::HWI4
            | Interrupt::HWI5
            | Interrupt::HWI6
            | Interrupt::HWI7,
        ) => {
            crate::board::irq_handler();
        }
        other => {
            panic!(
                "Unsupported LoongArch trap from kernel: {:?}, badv = {:#x}, trampoline={:#x}!",
                other, badv, TRAMPOLINE
            );
        }
    }
}

pub use context::TrapContext;
