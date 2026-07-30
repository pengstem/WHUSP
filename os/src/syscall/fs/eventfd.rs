use crate::fs::{OpenFlags, make_eventfd};

use super::fd::install_file_fd;
use crate::uapi::errno::{Errno, KResult};

const EFD_SEMAPHORE: u32 = 0x1;
const EFD_NONBLOCK: u32 = OpenFlags::NONBLOCK.bits();
const EFD_CLOEXEC: u32 = OpenFlags::CLOEXEC.bits();
const EFD_VALID_FLAGS: u32 = EFD_SEMAPHORE | EFD_NONBLOCK | EFD_CLOEXEC;

pub fn sys_eventfd2(initval: u32, flags: u32) -> KResult {
    if flags & !EFD_VALID_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }

    let mut open_flags = OpenFlags::RDWR;
    if flags & EFD_NONBLOCK != 0 {
        open_flags |= OpenFlags::NONBLOCK;
    }
    if flags & EFD_CLOEXEC != 0 {
        open_flags |= OpenFlags::CLOEXEC;
    }

    let file = make_eventfd(initval as u64, flags & EFD_SEMAPHORE != 0);
    install_file_fd(file, open_flags, None)
}
