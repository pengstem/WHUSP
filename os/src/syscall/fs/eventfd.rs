use crate::fs::{OpenFlags, make_eventfd};
use crate::task::{FdTableEntry, current_process};

use super::super::errno::{SysError, SysResult};

const EFD_SEMAPHORE: u32 = 0x1;
const EFD_NONBLOCK: u32 = OpenFlags::NONBLOCK.bits();
const EFD_CLOEXEC: u32 = OpenFlags::CLOEXEC.bits();
const EFD_VALID_FLAGS: u32 = EFD_SEMAPHORE | EFD_NONBLOCK | EFD_CLOEXEC;

pub fn sys_eventfd2(initval: u32, flags: u32) -> SysResult {
    if flags & !EFD_VALID_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }

    let mut open_flags = OpenFlags::RDWR;
    if flags & EFD_NONBLOCK != 0 {
        open_flags |= OpenFlags::NONBLOCK;
    }
    if flags & EFD_CLOEXEC != 0 {
        open_flags |= OpenFlags::CLOEXEC;
    }

    let file = make_eventfd(initval as u64, flags & EFD_SEMAPHORE != 0);
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let fd = inner.alloc_fd_from(0).ok_or(SysError::EMFILE)?;
    inner.fd_table[fd] = Some(FdTableEntry::from_file(file, open_flags));
    Ok(fd as isize)
}
