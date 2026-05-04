use super::trap::handle_mmap_page_fault;
use crate::mm::{MmapFaultAccess, VirtAddr, page_table::PTEFlags};
use crate::syscall::user_ptr::{
    UserBufferAccess, read_user_value_with_fault, write_user_value_with_fault,
};
use crate::syscall::{LinuxSigInfo, errno::SysError, errno::SysResult};
use crate::task::{
    SIGCHLD, SIGNAL_INFO_SLOTS, SignalAction, SignalFlags, SignalInfo, current_add_signal,
    current_process, current_task, current_trap_cx, current_user_token, flags_to_linux_sigset,
    linux_sigset_to_flags,
};
use crate::trap::TrapContext;
use core::mem::{offset_of, size_of};

const SIGNAL_FRAME_MAGIC: usize = 0x5753_4947_4652_414d;
const SIGNAL_STACK_ALIGN: usize = 16;
const SA_NODEFER: usize = 0x4000_0000;
const SIGINT: usize = 2;
const SIGALRM: usize = 14;
const SIGCANCEL: usize = 33;
const RT_SIGRETURN_TRAMPOLINE: [u32; 2] = [0x08b0_0893, 0x0000_0073];

pub fn can_deliver_user_signal(signum: usize) -> bool {
    matches!(signum, SIGINT | SIGALRM | SIGCANCEL) || signum == SIGCHLD as usize
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxUContextCompat {
    // CONTEXT: musl/riscv64's pthread cancel handler reads uc_sigmask at
    // ucontext+40 and the interrupted PC at ucontext+176.
    prefix_until_sigmask: [u8; 40],
    sigmask: u64,
    prefix_until_pc: [u8; 128],
    pc: usize,
}

impl LinuxUContextCompat {
    fn new(interrupted_pc: usize, old_mask: SignalFlags) -> Self {
        Self {
            prefix_until_sigmask: [0; 40],
            sigmask: flags_to_linux_sigset(old_mask),
            prefix_until_pc: [0; 128],
            pc: interrupted_pc,
        }
    }

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
    magic: usize,
    trampoline: [u32; 2],
    saved_context: TrapContext,
    siginfo: LinuxSigInfo,
    ucontext: LinuxUContextCompat,
}

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
    true
}

fn signal_mmap_fault(addr: usize, access: UserBufferAccess) -> bool {
    let access = match access {
        UserBufferAccess::Read => MmapFaultAccess::Read,
        UserBufferAccess::Write => MmapFaultAccess::Write,
    };
    handle_mmap_page_fault(addr, access)
}

fn remove_pending_signal(signum: usize, signal: SignalFlags) {
    let Some(task) = current_task() else {
        return;
    };
    let mut task_inner = task.inner_exclusive_access();
    if task_inner.pending_signals.contains(signal) {
        task_inner.clear_pending(signum as u32);
    }
}

fn take_pending_user_signal() -> Option<PendingUserSignal> {
    let task = current_task()?;
    let process = current_process();
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
                // UNFINISHED: Full Linux signal delivery must support every
                // user-installed handler. This stage deliberately limits signal
                // frames to libc-test sigreturn's SIGINT, musl's pthread
                // cancellation signal, ITIMER_REAL's SIGALRM, and BusyBox
                // ash's SIGCHLD wait wakeup while the generic signal ABI is
                // still being validated.
                continue;
            }
            selected = Some((signum, signal));
            break;
        }
        selected?
    };

    let action = process.inner_exclusive_access().signal_actions[signum];
    if action.is_ignore() {
        remove_pending_signal(signum, signal);
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

pub fn deliver_pending_signal(interrupted_pc: usize) -> bool {
    let Some(delivery) = take_pending_user_signal() else {
        return false;
    };
    let saved_context = *current_trap_cx();
    let user_sp = saved_context.x[2];
    let frame_sp = (user_sp - size_of::<RiscvSignalFrame>()) & !(SIGNAL_STACK_ALIGN - 1);
    let frame = RiscvSignalFrame {
        magic: SIGNAL_FRAME_MAGIC,
        trampoline: RT_SIGRETURN_TRAMPOLINE,
        saved_context,
        siginfo: LinuxSigInfo::from(delivery.info),
        ucontext: LinuxUContextCompat::new(interrupted_pc, delivery.old_mask),
    };
    let token = current_user_token();
    if write_user_value_with_fault(
        token,
        frame_sp as *mut RiscvSignalFrame,
        &frame,
        Some(signal_mmap_fault),
    )
    .is_err()
    {
        current_add_signal(SignalFlags::SIGSEGV);
        return false;
    }

    let siginfo_ptr = frame_sp + offset_of!(RiscvSignalFrame, siginfo);
    let ucontext_ptr = frame_sp + offset_of!(RiscvSignalFrame, ucontext);
    let trampoline_ptr = frame_sp + offset_of!(RiscvSignalFrame, trampoline);
    if !make_trampoline_page_executable(trampoline_ptr) {
        current_add_signal(SignalFlags::SIGSEGV);
        return false;
    }

    let trap_cx = current_trap_cx();
    trap_cx.x = saved_context.x;
    trap_cx.sepc = delivery.action.handler;
    trap_cx.set_sp(frame_sp);
    trap_cx.x[1] = trampoline_ptr;
    trap_cx.x[10] = delivery.signum as usize;
    trap_cx.x[11] = siginfo_ptr;
    trap_cx.x[12] = ucontext_ptr;
    true
}

pub fn sys_rt_sigreturn() -> SysResult {
    let token = current_user_token();
    let signal_sp = current_trap_cx().x[2];
    let frame: RiscvSignalFrame = read_user_value_with_fault(
        token,
        signal_sp as *const RiscvSignalFrame,
        Some(signal_mmap_fault),
    )?;
    if frame.magic != SIGNAL_FRAME_MAGIC {
        return Err(SysError::EINVAL);
    }

    if let Some(task) = current_task() {
        task.inner_exclusive_access().signal_mask = frame.ucontext.restored_signal_mask();
    }
    let mut restored_context = frame.saved_context;
    restored_context.sepc = frame.ucontext.pc;
    let return_value = restored_context.x[10] as isize;
    *current_trap_cx() = restored_context;
    Ok(return_value)
}
