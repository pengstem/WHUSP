use crate::syscall::LinuxSigInfo;
use crate::syscall::user_ptr::{
    copy_to_user, read_user_value, write_user_value, write_user_value_in_memory_set,
};
use crate::task::{
    CAP_SYS_PTRACE, Credentials, ProcessControlBlock, SIGTRAP, TaskControlBlock, current_process,
    current_user_token, pid2process, ptrace_attach_process, ptrace_kill_process,
    ptrace_resume_process, ptrace_traceme_current, ptrace_validate_tracee,
};
use crate::uapi::errno::{Errno, KResult};
use alloc::sync::Arc;
use core::mem::{size_of, size_of_val};

const PTRACE_TRACEME: usize = 0;
const PTRACE_PEEKTEXT: usize = 1;
const PTRACE_PEEKDATA: usize = 2;
const PTRACE_PEEKUSER: usize = 3;
const PTRACE_POKETEXT: usize = 4;
const PTRACE_POKEDATA: usize = 5;
const PTRACE_CONT: usize = 7;
const PTRACE_KILL: usize = 8;
const PTRACE_GETREGS: usize = 12;
const PTRACE_ATTACH: usize = 16;
const PTRACE_DETACH: usize = 17;
const PTRACE_SYSCALL: usize = 24;
const PTRACE_SETOPTIONS: usize = 0x4200;
const PTRACE_GETSIGINFO: usize = 0x4202;
const PTRACE_GETREGSET: usize = 0x4204;
const PTRACE_GET_SYSCALL_INFO: usize = 0x420e;

const NT_PRSTATUS: usize = 1;
const PTRACE_USER_AREA_SIZE: usize = 0x1000;
const PTRACE_O_MASK: usize = 0x0030_00ff;
const PTRACE_SYSCALL_INFO_NONE: u8 = 0;
const PTRACE_SYSCALL_INFO_ENTRY: u8 = 1;
const PTRACE_SYSCALL_INFO_EXIT: u8 = 2;
const AUDIT_ARCH_64BIT: u32 = 0x8000_0000;
const AUDIT_ARCH_LE: u32 = 0x4000_0000;
#[cfg(target_arch = "riscv64")]
const AUDIT_ARCH_RISCV64: u32 = AUDIT_ARCH_64BIT | AUDIT_ARCH_LE | 243;
#[cfg(target_arch = "loongarch64")]
const AUDIT_ARCH_LOONGARCH64: u32 = AUDIT_ARCH_64BIT | AUDIT_ARCH_LE | 258;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct UserIovec {
    base: usize,
    len: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PtraceSyscallInfo {
    op: u8,
    _pad: [u8; 3],
    arch: u32,
    instruction_pointer: u64,
    stack_pointer: u64,
    data: [u64; 7],
}

const MAX_PRSTATUS_WORDS: usize = 64;

fn tracee_from_pid(pid: isize) -> KResult<Arc<ProcessControlBlock>> {
    if pid <= 0 {
        return Err(Errno::ESRCH);
    }
    pid2process(pid as usize).ok_or(Errno::ESRCH)
}

fn has_sys_ptrace_equivalent(credentials: &Credentials) -> bool {
    credentials.euid == 0
        && credentials
            .capabilities
            .has_effective(CAP_SYS_PTRACE)
            .unwrap_or(false)
}

fn can_attach(caller: &Credentials, target: &Credentials, target_dumpable: bool) -> bool {
    if has_sys_ptrace_equivalent(caller) {
        return true;
    }
    target_dumpable
        && caller.euid == target.ruid
        && caller.euid == target.euid
        && caller.euid == target.suid
}

fn check_attach_permission(tracee: &Arc<ProcessControlBlock>) -> KResult<()> {
    let caller = current_process();
    let caller_credentials = caller.credentials();
    let (target_credentials, target_dumpable) = {
        let inner = tracee.inner_exclusive_access();
        (inner.credentials.clone(), inner.dumpable)
    };
    if can_attach(&caller_credentials, &target_credentials, target_dumpable) {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}

fn read_tracee_word(tracee: &Arc<ProcessControlBlock>, addr: usize) -> KResult {
    let token = tracee.inner_exclusive_access().memory_set.token();
    read_user_value::<usize>(token, addr as *const usize).map(|value| value as isize)
}

fn write_tracee_word(tracee: &Arc<ProcessControlBlock>, addr: usize, data: usize) -> KResult {
    let mut inner = tracee.inner_exclusive_access();
    write_user_value_in_memory_set(&mut inner.memory_set, addr as *mut usize, &data)?;
    Ok(0)
}

fn validate_user_area_offset(addr: usize) -> KResult<()> {
    if addr % size_of::<usize>() != 0 || addr >= PTRACE_USER_AREA_SIZE {
        return Err(Errno::EIO);
    }
    Ok(())
}

#[cfg(target_arch = "riscv64")]
fn collect_prstatus_words(task: &Arc<TaskControlBlock>) -> ([usize; MAX_PRSTATUS_WORDS], usize) {
    let task_inner = task.inner_exclusive_access();
    let cx = task_inner.get_trap_cx();
    let mut words = [0usize; MAX_PRSTATUS_WORDS];
    words[0] = cx.sepc;
    words[1..32].copy_from_slice(&cx.x[1..32]);
    (words, 32)
}

#[cfg(target_arch = "loongarch64")]
fn collect_prstatus_words(task: &Arc<TaskControlBlock>) -> ([usize; MAX_PRSTATUS_WORDS], usize) {
    let task_inner = task.inner_exclusive_access();
    let cx = task_inner.get_trap_cx();
    let mut words = [0usize; MAX_PRSTATUS_WORDS];
    words[..32].copy_from_slice(&cx.x);
    words[32] = cx.era;
    // words[33] is csr_badvaddr; this kernel does not keep a per-task copy.
    // words[34..44] are the Linux reserved slots.
    (words, 44)
}

fn prstatus_bytes(words: &[usize]) -> &[u8] {
    unsafe { core::slice::from_raw_parts(words.as_ptr().cast::<u8>(), size_of_val(words)) }
}

#[cfg(target_arch = "riscv64")]
fn audit_arch() -> u32 {
    AUDIT_ARCH_RISCV64
}

#[cfg(target_arch = "loongarch64")]
fn audit_arch() -> u32 {
    AUDIT_ARCH_LOONGARCH64
}

fn ptrace_getregs(task: &Arc<TaskControlBlock>, data: usize) -> KResult {
    let (words, word_len) = collect_prstatus_words(task);
    let bytes = prstatus_bytes(&words[..word_len]);
    copy_to_user(current_user_token(), data as *mut u8, bytes)?;
    Ok(0)
}

fn ptrace_getregset(task: &Arc<TaskControlBlock>, addr: usize, data: usize) -> KResult {
    if addr != NT_PRSTATUS {
        return Err(Errno::EIO);
    }
    let token = current_user_token();
    let mut iov = read_user_value::<UserIovec>(token, data as *const UserIovec)?;
    let (words, word_len) = collect_prstatus_words(task);
    let bytes = prstatus_bytes(&words[..word_len]);
    let copy_len = iov.len.min(bytes.len());
    if copy_len != 0 {
        copy_to_user(token, iov.base as *mut u8, &bytes[..copy_len])?;
    }
    iov.len = copy_len;
    write_user_value(token, data as *mut UserIovec, &iov)?;
    Ok(0)
}

fn ptrace_get_syscall_info(tracee: &Arc<ProcessControlBlock>, size: usize, data: usize) -> KResult {
    let stop = tracee.inner_exclusive_access().ptrace.syscall_stop;
    let mut info = PtraceSyscallInfo {
        op: PTRACE_SYSCALL_INFO_NONE,
        arch: audit_arch(),
        ..PtraceSyscallInfo::default()
    };
    if let Some(stop) = stop {
        info.op = stop.op;
        info.instruction_pointer = stop.instruction_pointer as u64;
        info.stack_pointer = stop.stack_pointer as u64;
        match stop.op {
            PTRACE_SYSCALL_INFO_ENTRY => {
                info.data[0] = stop.nr as u64;
                for (dst, arg) in info.data[1..].iter_mut().zip(stop.args.iter()) {
                    *dst = *arg as u64;
                }
            }
            PTRACE_SYSCALL_INFO_EXIT => {
                info.data[0] = stop.rval as u64;
                info.data[1] = stop.is_error as u64;
            }
            _ => {}
        }
    }

    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&info as *const PtraceSyscallInfo).cast::<u8>(),
            size_of::<PtraceSyscallInfo>(),
        )
    };
    let copy_len = size.min(bytes.len());
    if copy_len != 0 {
        copy_to_user(current_user_token(), data as *mut u8, &bytes[..copy_len])?;
    }
    Ok(bytes.len() as isize)
}

fn ptrace_getsiginfo(tracee: &Arc<ProcessControlBlock>, data: *mut LinuxSigInfo) -> KResult {
    if data.is_null() {
        return Err(Errno::EFAULT);
    }
    let stop_signal = tracee
        .inner_exclusive_access()
        .ptrace
        .stop_signal
        .unwrap_or(SIGTRAP);
    let info = LinuxSigInfo {
        si_signo: if stop_signal == (SIGTRAP | 0x80) {
            SIGTRAP as i32
        } else {
            stop_signal as i32
        },
        si_code: if stop_signal == (SIGTRAP | 0x80) {
            stop_signal as i32
        } else {
            0
        },
        ..LinuxSigInfo::default()
    };
    write_user_value(current_user_token(), data, &info)?;
    Ok(0)
}

pub fn sys_ptrace(request: usize, pid: isize, addr: usize, data: usize) -> KResult {
    if request == PTRACE_TRACEME {
        return ptrace_traceme_current();
    }

    let tracer_pid = current_process().getpid();
    let tracee = tracee_from_pid(pid)?;

    match request {
        PTRACE_ATTACH => {
            check_attach_permission(&tracee)?;
            ptrace_attach_process(&tracee, tracer_pid)
        }
        PTRACE_CONT => ptrace_resume_process(&tracee, tracer_pid, data as u32, false, false),
        PTRACE_DETACH => ptrace_resume_process(&tracee, tracer_pid, data as u32, true, false),
        PTRACE_KILL => ptrace_kill_process(&tracee, tracer_pid),
        PTRACE_PEEKTEXT | PTRACE_PEEKDATA => {
            ptrace_validate_tracee(&tracee, tracer_pid, true)?;
            read_tracee_word(&tracee, addr)
        }
        PTRACE_POKETEXT | PTRACE_POKEDATA => {
            ptrace_validate_tracee(&tracee, tracer_pid, true)?;
            write_tracee_word(&tracee, addr, data)
        }
        PTRACE_PEEKUSER => {
            ptrace_validate_tracee(&tracee, tracer_pid, true)?;
            validate_user_area_offset(addr)?;
            Ok(0)
        }
        PTRACE_GETREGS => {
            let task = ptrace_validate_tracee(&tracee, tracer_pid, true)?;
            ptrace_getregs(&task, data)
        }
        PTRACE_GETREGSET => {
            let task = ptrace_validate_tracee(&tracee, tracer_pid, true)?;
            ptrace_getregset(&task, addr, data)
        }
        PTRACE_GETSIGINFO => {
            ptrace_validate_tracee(&tracee, tracer_pid, true)?;
            ptrace_getsiginfo(&tracee, data as *mut LinuxSigInfo)
        }
        PTRACE_SETOPTIONS => {
            ptrace_validate_tracee(&tracee, tracer_pid, true)?;
            if data & !PTRACE_O_MASK != 0 {
                Err(Errno::EINVAL)
            } else {
                tracee.inner_exclusive_access().ptrace.options = data;
                Ok(0)
            }
        }
        PTRACE_SYSCALL => ptrace_resume_process(&tracee, tracer_pid, data as u32, false, true),
        PTRACE_GET_SYSCALL_INFO => {
            ptrace_validate_tracee(&tracee, tracer_pid, true)?;
            ptrace_get_syscall_info(&tracee, addr, data)
        }
        _ => Err(Errno::EIO),
    }
}
