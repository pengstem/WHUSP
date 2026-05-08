mod context;

use crate::config::TRAMPOLINE;
use crate::mm::{MmapFaultAccess, MmapFaultResult};
use crate::syscall::{errno::SysError, syscall};
use crate::task::{
    SignalFlags, account_current_system_time_until, account_current_user_time_until,
    check_signals_of_current, current_add_signal, current_process, current_trap_cx,
    current_trap_cx_user_va, current_user_token, exit_current_group_and_run_next,
    mark_current_user_time_entry, suspend_current_and_run_next,
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
            let syscall_pc = current_trap_cx().sepc;
            // jump to next instruction anyway
            let mut cx = current_trap_cx();
            cx.sepc += 4;

            enable_supervisor_interrupt();

            // get system call return value
            let result = syscall(
                cx.x[17],
                [cx.x[10], cx.x[11], cx.x[12], cx.x[13], cx.x[14], cx.x[15]],
            );
            // cx is changed during sys_execve, so we have to call it again
            cx = current_trap_cx();
            // UNFINISHED: Full SA_RESTART is not modeled yet. Most interrupted
            // syscalls such as futex, nanosleep, clock_nanosleep, ppoll, and
            // pselect6 currently return EINTR after rt_sigreturn instead of
            // being automatically restarted; wait4/waitid only suppress EINTR
            // for restartable handlers.
            interrupted_pc = cx.sepc;
            cx.x[10] = result as usize;
            let syscall_pc_if_interrupted = if result == -(SysError::EINTR as isize) {
                Some(syscall_pc)
            } else {
                None
            };
            if crate::arch::signal::deliver_pending_signal(
                interrupted_pc,
                syscall_pc_if_interrupted,
            ) {
                trap_return();
            }
            signal_delivery_attempted = true;
        }
        Trap::Exception(Exception::StorePageFault) => {
            if !handle_mmap_page_fault(stval, MmapFaultAccess::Write) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::InstructionPageFault) => {
            if !handle_mmap_page_fault(stval, MmapFaultAccess::Execute) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::LoadPageFault) => {
            if !handle_mmap_page_fault(stval, MmapFaultAccess::Read) {
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
        MmapFaultResult::Page(page) => {
            // UNFINISHED: Linux reports SIGBUS for some file-backed mmap faults,
            // such as pages wholly beyond the backing object; this kernel still
            // collapses mmap fault failures into SIGSEGV.
            let Some(frame) = page.build_frame() else {
                return false;
            };
            let mut inner = process.inner_exclusive_access();
            inner.memory_set.install_mmap_fault_page(page, frame)
        }
        MmapFaultResult::PageCache(page) => {
            // UNFINISHED: Linux reports SIGBUS for some file-backed mmap faults,
            // such as pages wholly beyond the backing object; this kernel still
            // collapses mmap fault failures into SIGSEGV.
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

#[unsafe(no_mangle)]
/// set the new addr of __restore asm function in TRAMPOLINE page,
/// set the reg a0 = trap_cx_ptr, reg a1 = phy addr of usr page table,
/// finally, jump to new addr of __restore asm function
pub fn trap_return() -> ! {
    let now_us = get_time_us();
    account_current_system_time_until(now_us);
    mark_current_user_time_entry(now_us);
    disable_supervisor_interrupt();
    set_user_trap_entry();
    let trap_cx_user_va = current_trap_cx_user_va();
    let user_satp = current_user_token();
    unsafe extern "C" {
        unsafe fn __alltraps();
        unsafe fn __restore();
    }
    let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;
    //println!("before return");
    unsafe {
        asm!(
            "fence.i",
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
            check_timer();
            // do not schedule now
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
