use crate::task::{SeccompSockFilter, SignalFlags, TaskControlBlock};
use crate::uapi::syscall_nr::{SYSCALL_EXIT, SYSCALL_READ, SYSCALL_RT_SIGRETURN, SYSCALL_WRITE};

fn filter_allows(filter: &[SeccompSockFilter], syscall_id: usize) -> bool {
    const BPF_LD_W_ABS: u16 = 0x20;
    const BPF_JMP_JEQ_K: u16 = 0x15;
    const BPF_RET_K: u16 = 0x06;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;

    let mut accumulator = 0u32;
    let mut pc = 0usize;
    while let Some(instruction) = filter.get(pc) {
        match instruction.code {
            BPF_LD_W_ABS => {
                accumulator = syscall_id as u32;
                pc += 1;
            }
            BPF_JMP_JEQ_K => {
                pc += 1 + if accumulator == instruction.k {
                    instruction.jt as usize
                } else {
                    instruction.jf as usize
                };
            }
            BPF_RET_K => return instruction.k & 0xffff_0000 == SECCOMP_RET_ALLOW,
            _ => return false,
        }
    }
    false
}

pub(super) fn signal_for_syscall(
    task: &TaskControlBlock,
    syscall_id: usize,
) -> Option<SignalFlags> {
    const SECCOMP_MODE_STRICT: u8 = 1;
    const SECCOMP_MODE_FILTER: u8 = 2;
    let inner = task.inner_exclusive_access();
    match inner.seccomp_mode {
        SECCOMP_MODE_STRICT => (!matches!(
            syscall_id,
            SYSCALL_READ | SYSCALL_WRITE | SYSCALL_EXIT | SYSCALL_RT_SIGRETURN
        ))
        .then_some(SignalFlags::SIGKILL),
        SECCOMP_MODE_FILTER => {
            if inner
                .seccomp_filter
                .as_deref()
                .is_some_and(|filter| filter_allows(filter, syscall_id))
            {
                None
            } else {
                Some(SignalFlags::SIGSYS)
            }
        }
        _ => None,
    }
}
