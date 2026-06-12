use super::trap::handle_user_page_fault;
use crate::mm::{MmapFaultAccess, VirtAddr, page_table::PTEFlags};
use crate::syscall::user_ptr::{
    UserBufferAccess, read_user_value_with_fault, write_user_value_with_fault,
};
use crate::syscall::{LinuxSigInfo, errno::SysError, errno::SysResult};
use crate::task::{
    ProcessControlBlock, SI_TKILL, SIGKILL, SIGNAL_INFO_SLOTS, SIGRT_1, SIGRTMIN, SIGSTOP,
    SS_DISABLE, SignalAction, SignalFlags, SignalInfo, TaskControlBlock, current_add_signal,
    current_process, current_task, current_trap_cx, current_user_token, flags_to_linux_sigset,
    linux_sigset_to_flags, trap_cx_of_task,
};
use crate::trap::TrapContext;
use alloc::sync::Arc;
use core::mem::{offset_of, size_of};

// Guard word checked by rt_sigreturn so a corrupted or mismatched userspace
// signal frame is rejected before restoring TrapContext state.
const SIGNAL_FRAME_MAGIC: usize = 0x5753_4947_4652_414d;
const SIGNAL_STACK_ALIGN: usize = 16;
const SA_NODEFER: usize = 0x4000_0000;
const SA_ONSTACK: usize = 0x0800_0000;
const SA_RESTORER: usize = 0x0400_0000;
const SA_RESETHAND: usize = 0x8000_0000;
// `addi a7, zero, __NR_rt_sigreturn; ecall`, written into the user frame when
// no libc-provided restorer is available.
const RT_SIGRETURN_TRAMPOLINE: [u32; 2] = [0x08b0_0893, 0x0000_0073];
const LINUX_SIGSET_BYTES: usize = 128;

pub fn can_deliver_user_signal(signum: usize) -> bool {
    signum > 0
        && signum < SIGNAL_INFO_SLOTS
        && signum != SIGKILL as usize
        && signum != SIGSTOP as usize
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxStackT {
    sp: usize,
    flags: i32,
    pad: u32,
    size: usize,
}

impl LinuxStackT {
    fn for_task(task: &TaskControlBlock, current_sp: usize) -> Self {
        let stack = task.inner_exclusive_access().sigaltstack;
        Self {
            sp: stack.sp,
            flags: stack.flags_for_sp(current_sp),
            pad: 0,
            size: stack.size,
        }
    }
}

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct RiscvFpState {
    f: [u64; 64],
    fcsr: u32,
    reserved: [u32; 3],
}

impl RiscvFpState {
    fn new(saved_context: TrapContext) -> Self {
        let mut f = [0; 64];
        f[..32].copy_from_slice(&saved_context.f);
        Self {
            f,
            fcsr: saved_context.fcsr,
            reserved: [0; 3],
        }
    }
}

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct RiscvMContext {
    gregs: [usize; 32],
    fpregs: RiscvFpState,
}

impl RiscvMContext {
    fn new(interrupted_pc: usize, saved_context: TrapContext) -> Self {
        let mut gregs = [0; 32];
        gregs[0] = interrupted_pc;
        gregs[1..].copy_from_slice(&saved_context.x[1..]);
        Self {
            gregs,
            fpregs: RiscvFpState::new(saved_context),
        }
    }

    fn restore_trap_context(self, saved_context: TrapContext) -> TrapContext {
        let mut restored = saved_context;
        restored.sepc = self.gregs[0];
        restored.x[1..].copy_from_slice(&self.gregs[1..]);
        restored.f.copy_from_slice(&self.fpregs.f[..32]);
        restored.fcsr = self.fpregs.fcsr;
        restored
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RiscvUContext {
    flags: usize,
    link: usize,
    stack: LinuxStackT,
    sigmask: u64,
    sigmask_padding: [u8; LINUX_SIGSET_BYTES - size_of::<u64>()],
    mcontext: RiscvMContext,
}

impl RiscvUContext {
    fn restored_signal_mask(self) -> SignalFlags {
        let mut mask = linux_sigset_to_flags(self.sigmask);
        mask.remove(SignalFlags::SIGKILL);
        mask.remove(SignalFlags::SIGSTOP);
        mask
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RiscvSignalFrame {
    siginfo: LinuxSigInfo,
    ucontext: RiscvUContext,
    magic: usize,
    saved_context: TrapContext,
    trampoline: [u32; 2],
}

const _: () = {
    assert!(offset_of!(RiscvSignalFrame, siginfo) == 0);
    assert!(offset_of!(RiscvSignalFrame, ucontext) == 128);
    assert!(offset_of!(RiscvUContext, sigmask) == 40);
    assert!(offset_of!(RiscvUContext, mcontext) == 176);
    assert!(offset_of!(RiscvMContext, gregs) == 0);
    assert!(size_of::<LinuxSigInfo>() == 128);
};

struct PendingUserSignal {
    signum: u32,
    info: SignalInfo,
    action: SignalAction,
    old_mask: SignalFlags,
}

fn make_trampoline_page_executable(trampoline_ptr: usize) -> bool {
    let process = current_process();
    let vpn = VirtAddr::from(trampoline_ptr).floor();
    let mut inner = process.inner_exclusive_access();
    let Some(pte) = inner.memory_set.translate(vpn) else {
        return false;
    };
    // CONTEXT: Linux/RISC-V signal return uses a tiny user-space
    // rt_sigreturn trampoline. pthread stacks come from mmap(PROT_READ|WRITE),
    // so grant execute only to the page that holds this generated trampoline.
    if !inner
        .memory_set
        .remap_existing_page_flags(vpn, pte.flags() | PTEFlags::X)
    {
        return false;
    }
    crate::arch::mm::flush_tlb_page(trampoline_ptr);
    crate::arch::mm::publish_pte_barrier();
    crate::arch::mm::instruction_barrier();
    true
}

fn signal_user_fault(addr: usize, access: UserBufferAccess) -> bool {
    let access = match access {
        UserBufferAccess::Read => MmapFaultAccess::Read,
        UserBufferAccess::Write => MmapFaultAccess::Write,
    };
    handle_user_page_fault(addr, access)
}

fn remove_pending_signal_for_task(task: &TaskControlBlock, signum: usize, signal: SignalFlags) {
    let mut task_inner = task.inner_exclusive_access();
    if task_inner.pending_signals.contains(signal) {
        task_inner.clear_pending(signum as u32);
    }
}

fn take_pending_user_signal_for_task(
    task: &Arc<TaskControlBlock>,
    process: &Arc<ProcessControlBlock>,
) -> Option<PendingUserSignal> {
    let (signum, signal) = {
        let task_inner = task.inner_exclusive_access();
        let unmasked_bits = task_inner.pending_signals.bits() & !task_inner.signal_mask.bits();
        if unmasked_bits == 0 {
            return None;
        }
        let pending = SignalFlags::from_bits_retain(unmasked_bits);
        let mut selected = None;
        for signum in 1..SIGNAL_INFO_SLOTS {
            let signal = SignalFlags::from_signum(signum as u32)?;
            if !pending.contains(signal) {
                continue;
            }
            if !can_deliver_user_signal(signum) {
                continue;
            }
            selected = Some((signum, signal));
            break;
        }
        selected?
    };

    let action = {
        let mut process_inner = process.inner_exclusive_access();
        let action = process_inner.signal_actions[signum];
        if action.has_user_handler() && action.flags & SA_RESETHAND != 0 {
            process_inner.signal_actions[signum] = SignalAction::default();
        }
        action
    };
    if action.is_ignore() {
        remove_pending_signal_for_task(task, signum, signal);
        return None;
    }
    if !action.has_user_handler() {
        return None;
    }

    let mut task_inner = task.inner_exclusive_access();
    if !task_inner.pending_signals.contains(signal) || task_inner.signal_mask.contains(signal) {
        return None;
    }
    let info = task_inner
        .signal_infos
        .get(signum)
        .copied()
        .flatten()
        .unwrap_or_else(|| SignalInfo::user(signum as i32, 0));
    let old_mask = task_inner
        .sigsuspend_restore_mask
        .take()
        .unwrap_or(task_inner.signal_mask);
    task_inner.clear_pending(signum as u32);
    task_inner.signal_mask |= action.mask;
    if action.flags & SA_NODEFER == 0 {
        task_inner.signal_mask |= signal;
    }
    Some(PendingUserSignal {
        signum: signum as u32,
        info,
        action,
        old_mask,
    })
}

pub fn deliver_pending_signal(
    task: &Arc<TaskControlBlock>,
    process: &Arc<ProcessControlBlock>,
    interrupted_pc: usize,
    syscall_pc_if_interrupted: Option<usize>,
) -> bool {
    let Some(delivery) = take_pending_user_signal_for_task(task, process) else {
        return false;
    };
    let saved_context = *trap_cx_of_task(task);
    let interrupted_pc =
        interrupted_pc_for_delivery(interrupted_pc, syscall_pc_if_interrupted, &delivery);
    let user_sp = signal_frame_stack_top(delivery.action, saved_context.x[2]);
    let frame_sp = (user_sp - size_of::<RiscvSignalFrame>()) & !(SIGNAL_STACK_ALIGN - 1);
    let frame = RiscvSignalFrame {
        siginfo: LinuxSigInfo::from(delivery.info),
        ucontext: RiscvUContext {
            flags: 0,
            link: 0,
            stack: LinuxStackT::for_task(task, saved_context.x[2]),
            sigmask: flags_to_linux_sigset(delivery.old_mask),
            sigmask_padding: [0; LINUX_SIGSET_BYTES - size_of::<u64>()],
            mcontext: RiscvMContext::new(interrupted_pc, saved_context),
        },
        magic: SIGNAL_FRAME_MAGIC,
        saved_context,
        trampoline: RT_SIGRETURN_TRAMPOLINE,
    };
    let token = task.get_user_token();
    if write_user_value_with_fault(
        token,
        frame_sp as *mut RiscvSignalFrame,
        &frame,
        Some(signal_user_fault),
    )
    .is_err()
    {
        force_default_sigsegv();
        return false;
    }

    let siginfo_ptr = frame_sp + offset_of!(RiscvSignalFrame, siginfo);
    let ucontext_ptr = frame_sp + offset_of!(RiscvSignalFrame, ucontext);
    let trampoline_ptr = frame_sp + offset_of!(RiscvSignalFrame, trampoline);
    let restorer_ptr = if delivery.action.flags & SA_RESTORER != 0 && delivery.action.restorer != 0
    {
        delivery.action.restorer
    } else {
        // CONTEXT: This kernel has no RISC-V vDSO rt_sigreturn page yet, so
        // libc actions without SA_RESTORER still need a temporary stack
        // trampoline. Long-term this should become a fixed executable mapping.
        trampoline_ptr
    };
    if restorer_ptr == trampoline_ptr && !make_trampoline_page_executable(trampoline_ptr) {
        force_default_sigsegv();
        return false;
    }

    let trap_cx = trap_cx_of_task(task);
    trap_cx.x = saved_context.x;
    trap_cx.sepc = delivery.action.handler;
    trap_cx.set_sp(frame_sp);
    trap_cx.x[1] = restorer_ptr;
    trap_cx.x[10] = delivery.signum as usize;
    trap_cx.x[11] = siginfo_ptr;
    trap_cx.x[12] = ucontext_ptr;
    true
}

fn force_default_sigsegv() {
    let signum = SignalFlags::SIGSEGV.bits().trailing_zeros() as usize;
    current_process().inner_exclusive_access().signal_actions[signum] = SignalAction::default();
    if let Some(task) = current_task() {
        task.inner_exclusive_access()
            .signal_mask
            .remove(SignalFlags::SIGSEGV);
    }
    current_add_signal(SignalFlags::SIGSEGV);
}

fn interrupted_pc_for_delivery(
    interrupted_pc: usize,
    syscall_pc_if_interrupted: Option<usize>,
    delivery: &PendingUserSignal,
) -> usize {
    if is_cancellation_signal(delivery.signum as usize, delivery.info) {
        // CONTEXT: glibc and musl use internal real-time signals for pthread
        // cancellation. Their RISC-V handlers inspect whether the interrupted
        // PC lies inside the cancellable syscall ecall window.
        syscall_pc_if_interrupted.unwrap_or(interrupted_pc)
    } else {
        interrupted_pc
    }
}

fn is_cancellation_signal(signum: usize, info: SignalInfo) -> bool {
    matches!(signum, SIGRTMIN | SIGRT_1) && info.code == SI_TKILL
}

fn signal_frame_stack_top(action: SignalAction, current_sp: usize) -> usize {
    if action.flags & SA_ONSTACK == 0 {
        return current_sp;
    }
    let Some(task) = current_task() else {
        return current_sp;
    };
    let stack = task.inner_exclusive_access().sigaltstack;
    if !stack.is_enabled() || stack.flags & SS_DISABLE != 0 || stack.contains(current_sp) {
        current_sp
    } else {
        // UNFINISHED: Linux also detects altstack overflow and reports SIGSEGV
        // when the signal frame cannot fit on the configured alternate stack.
        stack.sp.saturating_add(stack.size)
    }
}

pub fn sys_rt_sigreturn() -> SysResult {
    let token = current_user_token();
    let signal_sp = current_trap_cx().x[2];
    let frame: RiscvSignalFrame = read_user_value_with_fault(
        token,
        signal_sp as *const RiscvSignalFrame,
        Some(signal_user_fault),
    )?;
    if frame.magic != SIGNAL_FRAME_MAGIC {
        return Err(SysError::EINVAL);
    }

    if let Some(task) = current_task() {
        task.inner_exclusive_access().signal_mask = frame.ucontext.restored_signal_mask();
    }
    // UNFINISHED: Linux also validates and restores vector extension records.
    // This first ABI-complete frame restores general registers and the saved
    // double-precision FPU payload carried by this kernel's TrapContext.
    let restored_context = frame
        .ucontext
        .mcontext
        .restore_trap_context(frame.saved_context);
    let return_value = restored_context.x[10] as isize;
    *current_trap_cx() = restored_context;
    Ok(return_value)
}
