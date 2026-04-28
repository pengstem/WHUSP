mod context;

use crate::config::TRAMPOLINE;
use crate::mm::{MmapFaultAccess, MmapFaultResult};
use crate::syscall::syscall;
use crate::task::{
    SignalFlags, account_current_system_time_until, account_current_user_time_until,
    check_signals_of_current, current_add_signal, current_process, current_trap_cx,
    current_user_token, exit_current_and_run_next, mark_current_user_time_entry,
    suspend_current_and_run_next,
};
use crate::timer::{check_timer, get_time_us, set_next_trigger};
use core::arch::global_asm;
use loongArch64::register::{
    badv, ecfg,
    ecfg::LineBasedInterrupt,
    eentry,
    estat::{self, Exception, Interrupt, Trap},
    stlbps, ticlr, tlbidx, tlbrehi, tlbrentry,
};
use loongArch64::register::{pwch, pwcl};

global_asm!(include_str!("trap/trap.S"));

pub fn init() {
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
    let interrupts = LineBasedInterrupt::TIMER;
    ecfg::set_lie(interrupts);
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
    account_current_user_time_until(get_time_us());
    let estat = estat::read();
    let badv = badv::read().vaddr();
    match estat.cause() {
        Trap::Exception(Exception::Syscall) => {
            let mut cx = current_trap_cx();
            cx.era += 4;
            enable_supervisor_interrupt();
            let result = syscall(
                cx.x[11],
                [cx.x[4], cx.x[5], cx.x[6], cx.x[7], cx.x[8], cx.x[9]],
            );
            cx = current_trap_cx();
            cx.x[4] = result as usize;
        }
        Trap::Exception(Exception::StorePageFault)
        | Trap::Exception(Exception::PageModifyFault) => {
            if !handle_mmap_page_fault(badv, MmapFaultAccess::Write) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::FetchPageFault)
        | Trap::Exception(Exception::PageNonExecutableFault)
        | Trap::Exception(Exception::FetchInstructionAddressError) => {
            if !handle_mmap_page_fault(badv, MmapFaultAccess::Execute) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::LoadPageFault)
        | Trap::Exception(Exception::PageNonReadableFault)
        | Trap::Exception(Exception::MemoryAccessAddressError)
        | Trap::Exception(Exception::PagePrivilegeIllegal) => {
            if !handle_mmap_page_fault(badv, MmapFaultAccess::Read) {
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
        other => {
            panic!(
                "Unsupported LoongArch trap {:?}, badv = {:#x}!",
                other, badv
            );
        }
    }
    if let Some((errno, msg)) = check_signals_of_current() {
        println!("[kernel] {}", msg);
        exit_current_and_run_next(errno);
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
pub fn trap_return() -> ! {
    let now_us = get_time_us();
    account_current_system_time_until(now_us);
    mark_current_user_time_entry(now_us);
    disable_supervisor_interrupt();
    set_kernel_trap_entry();
    let trap_cx = current_trap_cx() as *mut TrapContext as usize;
    let user_token = current_user_token();
    unsafe extern "C" {
        unsafe fn __restore(trap_cx: usize, user_token: usize) -> !;
    }
    unsafe { __restore(trap_cx, user_token) }
}

#[unsafe(no_mangle)]
pub fn trap_from_kernel(_trap_cx: &TrapContext) {
    let estat = estat::read();
    let badv = badv::read().vaddr();
    match estat.cause() {
        Trap::Interrupt(Interrupt::Timer) => {
            ticlr::clear_timer_interrupt();
            set_next_trigger();
            check_timer();
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
