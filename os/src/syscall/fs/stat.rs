use crate::fs::{FileStat, WorkingDir, stat_at};
use crate::mm::translated_str;
use crate::task::{current_process, current_user_token};

use super::super::errno::{SysError, SysResult};
use super::fd::get_file_by_fd;
use super::path::dirfd_base;
use super::uapi::{AT_EMPTY_PATH, AT_FDCWD, LinuxKstat, VALID_FSTATAT_FLAGS};
use super::user_ptr::write_user_value;

fn write_stat_to_user(token: usize, statbuf: *mut LinuxKstat, stat: FileStat) -> SysResult {
    write_user_value(token, statbuf, &stat.into())?;
    Ok(0)
}

fn stat_by_dirfd(dirfd: isize) -> SysResult<FileStat> {
    if dirfd == AT_FDCWD {
        return stat_at(current_process().working_dir(), ".").ok_or(SysError::ENOENT);
    }
    if dirfd < 0 {
        return Err(SysError::EBADF);
    }
    Ok(get_file_by_fd(dirfd as usize)?.stat())
}

pub fn sys_fstat(fd: usize, statbuf: *mut LinuxKstat) -> SysResult {
    if statbuf.is_null() {
        return Err(SysError::EFAULT);
    }
    let token = current_user_token();
    let file = get_file_by_fd(fd)?;
    write_stat_to_user(token, statbuf, file.stat())
}

pub fn sys_fstatat(
    dirfd: isize,
    pathname: *const u8,
    statbuf: *mut LinuxKstat,
    flags: i32,
) -> SysResult {
    if statbuf.is_null() || pathname.is_null() {
        // CONTEXT: Linux 6.11 allows NULL + AT_EMPTY_PATH, but the current user-pointer helpers
        // assume a non-null C string. Keep the older behavior until pathname translation is widened.
        return Err(SysError::EFAULT);
    }
    if flags & !VALID_FSTATAT_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }

    let token = current_user_token();
    let path = translated_str(token, pathname);
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(SysError::ENOENT);
        }
        return write_stat_to_user(token, statbuf, stat_by_dirfd(dirfd)?);
    }

    // CONTEXT: `AT_NO_AUTOMOUNT` is a no-op on modern Linux, and this resolver has no automount
    // concept. Accept the bit for libc compatibility without changing lookup behavior.
    // CONTEXT: The current path resolver does not distinguish follow vs nofollow on the final path
    // component yet. Accept `AT_SYMLINK_NOFOLLOW` so libc and LTP can reach this syscall, but it
    // is not enforced until symlink-aware lookup lands.
    let base = if path.starts_with('/') {
        WorkingDir::root()
    } else {
        dirfd_base(dirfd)?
    };
    let stat = stat_at(base, path.as_str()).ok_or(SysError::ENOENT)?;
    write_stat_to_user(token, statbuf, stat)
}
