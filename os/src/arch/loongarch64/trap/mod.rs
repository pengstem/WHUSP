mod context;
mod unaligned;
mod unaligned_decode;

use crate::arch::interrupt::{disable_supervisor_interrupt, enable_supervisor_interrupt};
use crate::config::{MAX_CPUS, TRAMPOLINE};
use crate::mm::{
    FaultOrigin, MmapFaultAccess, UserFaultFatal, UserFaultOutcome, record_fault_retry,
    record_fault_retry_chain, record_fault_retry_wait, resolve_user_page_fault,
};
use crate::syscall::{
    syscall_exit_with_current_task, syscall_is_exit, syscall_is_exit_group,
    syscall_with_current_task,
};
use crate::task::{
    SignalAction, SignalFlags, TaskControlBlock, TaskControlBlockInner, check_signals_of_task,
    current_add_signal, current_process, current_task, exit_current_group_and_run_next,
    process_of_task, suspend_current_and_run_next, timer_tick_should_preempt, trap_cx_of_task,
    trap_return_context_after_accounting_for_task,
};
use crate::timer::{check_timer, set_next_trigger};
#[cfg(feature = "precise-cpu-accounting")]
use crate::{task::account_task_user_time_until, timer::get_time_us};
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::arch::global_asm;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use loongArch64::cpu::CPUCFG;
use loongArch64::register::{
    badv, ecfg,
    ecfg::LineBasedInterrupt,
    eentry,
    estat::{self, Exception, Interrupt, Trap},
    euen, stlbps, ticlr, tlbidx, tlbrehi, tlbrentry,
};
use loongArch64::register::{pwch, pwcl};

global_asm!(include_str!("trap.S"));

const FP_CAP: u8 = 1 << 0;
const LSX_CAP: u8 = 1 << 1;
const LASX_CAP: u8 = 1 << 2;
const NO_FP_OWNER: usize = 0;
static USER_FP_CAPS: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(0) }; MAX_CPUS];
static USER_FP_OWNERS: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(NO_FP_OWNER) }; MAX_CPUS];

pub fn init() {
    let extension_config = CPUCFG::read(2);
    let fp = extension_config.get_bit(0);
    let lsx = extension_config.get_bit(6);
    let lasx = extension_config.get_bit(7);
    assert!(!lsx || fp, "LSX CPUCFG support requires scalar FP support");
    assert!(!lasx || lsx, "LASX CPUCFG support requires LSX support");
    let mut caps = 0;
    if fp {
        caps |= FP_CAP;
    }
    if lsx {
        caps |= LSX_CAP;
    }
    if lasx {
        caps |= LASX_CAP;
    }
    USER_FP_CAPS[crate::cpu::current_id()].store(caps, Ordering::Relaxed);
    // Leave all user extended units disabled until the matching unavailable
    // exception proves that the current task actually uses them.
    disable_user_fp_units();
    tlb_init(extension_config.get_bit(24));
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

pub fn enable_ipi_interrupt() {
    ecfg::set_lie(ecfg::read().lie() | LineBasedInterrupt::IPI);
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

const PS_4K: usize = 0x0c;
const PAGE_SIZE_SHIFT: usize = 12;

fn tlb_init(hardware_page_table_walk: bool) {
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
    pwch::set_hptw_enabled(hardware_page_table_walk);
    tlbrentry::set_tlbrentry(__tlb_refill as usize & 0x0000_ffff_ffff_ffff);
}

#[unsafe(no_mangle)]
pub fn trap_handler() -> ! {
    crate::perf::record_la_user_trap_entry();
    let mut task = current_task().expect("trap_handler requires a running task");
    let mut process = process_of_task(&task);
    #[cfg(feature = "precise-cpu-accounting")]
    account_task_user_time_until(&task, get_time_us());
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
            #[cfg(feature = "ptrace")]
            let syscall_pc = trap_pc;
            let (syscall_nr, syscall_args, syscall_sp) =
                syscall_entry.expect("syscall entry snapshot must exist for Syscall");
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
            trap_cx_of_task(&task).era += 4;
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
            let cx = trap_cx_of_task(&task);
            interrupted_pc = cx.era;
            cx.x[4] = result as usize;
            #[cfg(feature = "ptrace")]
            let syscall_exit_pc = cx.era;
            #[cfg(feature = "ptrace")]
            let syscall_exit_sp = cx.x[3];
            #[cfg(feature = "ptrace")]
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
            enable_supervisor_interrupt();
            if !handle_user_page_fault(badv, MmapFaultAccess::Write) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::FetchPageFault)
        | Trap::Exception(Exception::PageNonExecutableFault)
        | Trap::Exception(Exception::FetchInstructionAddressError) => {
            enable_supervisor_interrupt();
            if !handle_user_page_fault(badv, MmapFaultAccess::Execute) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::LoadPageFault)
        | Trap::Exception(Exception::PageNonReadableFault)
        | Trap::Exception(Exception::MemoryAccessAddressError)
        | Trap::Exception(Exception::PagePrivilegeIllegal) => {
            enable_supervisor_interrupt();
            if !handle_user_page_fault(badv, MmapFaultAccess::Read) {
                current_add_signal(SignalFlags::SIGSEGV);
            }
        }
        Trap::Exception(Exception::InstructionNotExist)
        | Trap::Exception(Exception::InstructionPrivilegeIllegal) => {
            current_add_signal(SignalFlags::SIGILL);
        }
        Trap::Exception(Exception::AddressNotAligned) => {
            // The 2K1000 has no hardware unaligned-access support. Match the
            // bounded scalar integer subset handled by Linux for userspace.
            let registers = trap_cx_of_task(&task).x;
            enable_supervisor_interrupt();
            match unaligned::emulate_user_unaligned(trap_pc, badv, &registers) {
                unaligned::UserUnalignedOutcome::Emulated { register_write } => {
                    unaligned::finish_user_unaligned(
                        trap_cx_of_task(&task),
                        trap_pc,
                        register_write,
                    );
                }
                unaligned::UserUnalignedOutcome::Segv => {
                    current_add_signal(SignalFlags::SIGSEGV);
                }
                unaligned::UserUnalignedOutcome::Bus => {
                    current_add_signal(SignalFlags::SIGBUS);
                }
            }
        }
        Trap::Exception(Exception::FloatingPointUnavailable) => {
            if !activate_user_fp_for_task(&task, UserFpMode::Scalar) {
                current_add_signal(SignalFlags::SIGILL);
            }
        }
        Trap::Exception(Exception::LsxUnavailable) => {
            if !activate_user_fp_for_task(&task, UserFpMode::Lsx) {
                current_add_signal(SignalFlags::SIGILL);
            }
        }
        Trap::Exception(Exception::LasxUnavailable) => {
            if !activate_user_fp_for_task(&task, UserFpMode::Lasx) {
                current_add_signal(SignalFlags::SIGILL);
            }
        }
        Trap::Interrupt(Interrupt::IPI) => {
            crate::arch::smp::clear_local_ipi();
            if crate::shutdown::stop_requested() {
                crate::shutdown::stop_current_cpu();
            }
            crate::cpu::handle_ipi();
            if timer_tick_should_preempt(&task) {
                suspend_current_and_run_next();
            }
        }
        Trap::Interrupt(Interrupt::Timer) => {
            ticlr::clear_timer_interrupt();
            set_next_trigger();
            if crate::cpu::is_timer_expiry_owner() {
                crate::drivers::block::poll_completions();
                check_timer();
            }
            if timer_tick_should_preempt(&task) {
                suspend_current_and_run_next();
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
                "Unsupported LoongArch trap {:?}, badv = {:#x}!",
                other, badv
            );
        }
    }
    #[cfg(feature = "ptrace")]
    if crate::task::ptrace_stop_task_if_needed(&task, &process) {
        interrupted_pc = trap_cx_of_task(&task).era;
    }
    crate::task::stop_current_task_if_needed();
    if crate::arch::signal::deliver_pending_signal(&task, &process, interrupted_pc) {
        trap_return_for_task(task, process);
    }
    if let Some((errno, _msg)) = check_signals_of_task(&task, &process) {
        drop(process);
        drop(task);
        exit_current_group_and_run_next(errno);
    }
    trap_return_for_task(task, process);
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
    let (trap_cx, user_token) = trap_return_context_after_accounting_for_task(&task);
    let flush_tlb = crate::arch::mm::should_flush_tlb_on_return(user_token);
    if flush_tlb {
        crate::perf::record_la_return_invtlb_call();
    }
    disable_supervisor_interrupt();
    prepare_user_fp_return_for_task(&task);
    drop(process);
    drop(task);
    set_kernel_trap_entry();
    unsafe extern "C" {
        unsafe fn __restore(trap_cx: usize, user_token: usize, flush_tlb: usize) -> !;
    }
    unsafe { __restore(trap_cx, user_token, flush_tlb as usize) }
}

#[inline(always)]
fn user_fp_owner_key(task: &TaskControlBlock) -> usize {
    task as *const TaskControlBlock as usize
}

#[inline(always)]
fn required_fp_cap(mode: UserFpMode) -> u8 {
    match mode {
        UserFpMode::Scalar => FP_CAP,
        UserFpMode::Lsx => FP_CAP | LSX_CAP,
        UserFpMode::Lasx => FP_CAP | LSX_CAP | LASX_CAP,
    }
}

#[inline(always)]
fn disable_user_fp_units() {
    euen::set_asxe(false);
    euen::set_sxe(false);
    euen::set_fpe(false);
}

#[inline(always)]
fn enable_user_fp_mode(mode: UserFpMode) {
    euen::set_fpe(true);
    euen::set_sxe(mode >= UserFpMode::Lsx);
    euen::set_asxe(mode >= UserFpMode::Lasx);
}

fn save_live_user_fp_state(task: &TaskControlBlock, inner: &mut TaskControlBlockInner) -> bool {
    let cpu = crate::cpu::current_id();
    let owner = &USER_FP_OWNERS[cpu];
    if owner.load(Ordering::Relaxed) != user_fp_owner_key(task) {
        return false;
    }
    let state = inner
        .loongarch_fp_state
        .as_deref_mut()
        .expect("LoongArch FP owner lost its allocated state");
    let _mode = state
        .mode()
        .expect("LoongArch FP owner has an invalid state mode");
    unsafe extern "C" {
        unsafe fn __la_save_user_fp_state(state: *mut UserFpState);
    }
    unsafe { __la_save_user_fp_state(state) };
    crate::perf::record_la_user_fp_save();
    state.mark_saved();
    disable_user_fp_units();
    owner.store(NO_FP_OWNER, Ordering::Relaxed);
    true
}

/// Flush a live local owner before the task becomes migratable.
pub(crate) fn leave_user_fp_owner_before_switch(
    task: &TaskControlBlock,
    inner: &mut TaskControlBlockInner,
    preserve: bool,
) {
    if preserve {
        save_live_user_fp_state(task, inner);
    } else {
        let cpu = crate::cpu::current_id();
        let owner = &USER_FP_OWNERS[cpu];
        if owner.load(Ordering::Relaxed) == user_fp_owner_key(task) {
            disable_user_fp_units();
            owner.store(NO_FP_OWNER, Ordering::Relaxed);
        }
        inner.loongarch_fp_state = None;
        task.publish_loongarch_fp_state(false);
    }
}

/// Materialize the current task's live registers for fork, clone, or signals.
pub(crate) fn sync_user_fp_state_for_task(task: &TaskControlBlock) {
    let mut inner = task.inner_exclusive_access();
    save_live_user_fp_state(task, &mut inner);
}

pub(crate) fn snapshot_user_fp_state_for_task(task: &TaskControlBlock) -> Option<UserFpState> {
    if !task.has_loongarch_fp_state_fast() {
        return None;
    }
    sync_user_fp_state_for_task(task);
    task.inner_exclusive_access()
        .loongarch_fp_state
        .as_deref()
        .copied()
}

pub(crate) fn discard_user_fp_state_for_task(task: &TaskControlBlock) {
    let mut inner = task.inner_exclusive_access();
    leave_user_fp_owner_before_switch(task, &mut inner, false);
}

pub(crate) fn install_user_fp_state_for_task(
    task: &TaskControlBlock,
    mut state: Option<UserFpState>,
) -> bool {
    if state.as_ref().is_some_and(|state| !state.validate()) {
        return false;
    }
    let mut inner = task.inner_exclusive_access();
    leave_user_fp_owner_before_switch(task, &mut inner, false);
    if let Some(state) = state.as_mut() {
        state.mark_saved();
    }
    let active = state.is_some();
    inner.loongarch_fp_state = state.map(Box::new);
    task.publish_loongarch_fp_state(active);
    true
}

fn activate_user_fp_for_task(task: &TaskControlBlock, mode: UserFpMode) -> bool {
    let cpu = crate::cpu::current_id();
    let caps = USER_FP_CAPS[cpu].load(Ordering::Relaxed);
    if caps & required_fp_cap(mode) != required_fp_cap(mode) {
        return false;
    }

    let mut inner = task.inner_exclusive_access();
    // An LSX/LASX disabled trap can upgrade a scalar or LSX owner that is still
    // live on this CPU. Save the overlapping lower lanes before widening it.
    save_live_user_fp_state(task, &mut inner);
    let state = inner
        .loongarch_fp_state
        .get_or_insert_with(|| Box::new(UserFpState::new(mode)));
    state.upgrade(mode);
    task.publish_loongarch_fp_state(true);
    crate::perf::record_la_user_fp_lazy_trap(mode as usize);
    true
}

fn prepare_user_fp_return_for_task(task: &TaskControlBlock) {
    let cpu = crate::cpu::current_id();
    let owner = &USER_FP_OWNERS[cpu];
    let owner_key = user_fp_owner_key(task);
    let current_owner = owner.load(Ordering::Relaxed);
    if current_owner == owner_key {
        crate::perf::record_la_user_fp_owner_return_hit();
        return;
    }
    assert_eq!(
        current_owner, NO_FP_OWNER,
        "LoongArch CPU retained another task's FP owner"
    );
    if !task.has_loongarch_fp_state_fast() {
        return;
    }

    let mut inner = task.inner_exclusive_access();
    let Some(state) = inner.loongarch_fp_state.as_deref_mut() else {
        panic!("LoongArch FP fast state lost its allocation");
    };
    assert!(
        state.needs_restore(),
        "migratable LoongArch FP state was not materialized"
    );
    let mode = state
        .mode()
        .expect("LoongArch task has an invalid FP state mode");
    enable_user_fp_mode(mode);
    unsafe extern "C" {
        unsafe fn __la_restore_user_fp_state(state: *const UserFpState);
    }
    unsafe { __la_restore_user_fp_state(state) };
    crate::perf::record_la_user_fp_restore();
    state.mark_live();
    owner.store(owner_key, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub fn trap_from_kernel(trap_cx: &mut TrapContext) {
    crate::arch::hart::redirect_idle_interrupt(trap_cx);
    let estat = estat::read();
    let badv = badv::read().vaddr();
    match estat.cause() {
        Trap::Interrupt(Interrupt::IPI) => {
            crate::arch::smp::clear_local_ipi();
            if crate::shutdown::stop_requested() {
                crate::shutdown::stop_current_cpu();
            }
            crate::cpu::handle_ipi();
        }
        Trap::Interrupt(Interrupt::Timer) => {
            ticlr::clear_timer_interrupt();
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
                crate::drivers::block::poll_completions();
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
                "Unsupported LoongArch trap from kernel: {:?}, cpu={}, era={:#x}, badv={:#x}, ra={:#x}, sp={:#x}, trampoline={:#x}!",
                other,
                crate::cpu::current_id(),
                trap_cx.era,
                badv,
                trap_cx.x[1],
                trap_cx.x[3],
                TRAMPOLINE
            );
        }
    }
}

pub use context::{TrapContext, UserFpMode, UserFpState};
