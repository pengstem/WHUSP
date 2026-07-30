use super::uapi::LinuxTimeSpec;
use crate::uapi::errno::KResult;

pub fn sys_set_robust_list(head: usize, len: usize) -> KResult {
    crate::task::futex::sys_set_robust_list(head, len)
}

pub fn sys_get_robust_list(pid: isize, head_ptr: *mut usize, len_ptr: *mut usize) -> KResult {
    crate::task::futex::sys_get_robust_list(pid, head_ptr, len_ptr)
}

pub fn sys_futex(
    uaddr: *mut u32,
    futex_op: u32,
    val: u32,
    timeout: *const LinuxTimeSpec,
    uaddr2: *mut u32,
    val3: u32,
) -> KResult {
    crate::task::futex::sys_futex(uaddr, futex_op, val, timeout, uaddr2, val3)
}
