use crate::syscall::errno::{SysError, SysResult};

pub fn can_deliver_user_signal(_signum: usize) -> bool {
    false
}

pub fn deliver_pending_signal(_interrupted_pc: usize) -> bool {
    false
}

pub fn sys_rt_sigreturn() -> SysResult {
    // UNFINISHED: LoongArch signal-frame construction/restoration has not been
    // validated yet; keep this boundary explicit while RISC-V pthread cancel is
    // brought up first.
    Err(SysError::ENOSYS)
}
