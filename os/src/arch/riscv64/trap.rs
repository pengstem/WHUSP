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
    // println!("into {:?}", scause.cause());
    match scause.cause() {
        Trap::Exception(Exception::UserEnvCall) => {
            // jump to next instruction anyway
            let mut cx = current_trap_cx();
            cx.sepc += 4;

            enable_supervisor_interrupt();

            // get system call return value
            let syscall_id = cx.x[17];
            let result = syscall(
                syscall_id,
                [cx.x[10], cx.x[11], cx.x[12], cx.x[13], cx.x[14], cx.x[15]],
            );
            // cx is changed during sys_execve, so we have to call it again
            cx = current_trap_cx();
            if cx.sepc != trap_pc + 4 {
                interrupted_pc = cx.sepc;
            } else if result == -(SysError::EINTR as isize) {
                interrupted_pc = trap_pc;
            } else {
                interrupted_pc = cx.sepc;
            }
            cx.x[10] = result as usize;
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
    if crate::arch::signal::deliver_pending_signal(interrupted_pc) {
        trap_return();
    }
    // check signals
    if let Some((errno, msg)) = check_signals_of_current() {
        println!("[kernel] {}", msg);
        exit_current_group_and_run_next(errno);
    }
    trap_return();
}

fn handle_mmap_page_fault(addr: usize, access: MmapFaultAccess) -> bool {
    let process = current_process();
    let fault = {
        let inner = process.inner_exclusive_access();
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
