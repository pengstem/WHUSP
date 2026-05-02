use crate::fs::{
    FileStat, MountId, WorkingDir, stat_at, stat_devfs_child, stat_devfs_misc_child,
    statfs_for_mount,
};
use crate::task::{current_process, current_user_token};

use super::super::errno::{SysError, SysResult};
use super::fd::get_file_by_fd;
use super::path::dirfd_base;
use super::uapi::{
    AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW, LinuxKstat, LinuxStatfs, LinuxStatx,
    STATX_RESERVED, VALID_FSTATAT_FLAGS, VALID_STATX_FLAGS,
};
use super::user_ptr::{PATH_MAX, read_user_c_string, write_user_value};

fn write_stat_result<T: From<FileStat> + Copy>(
    token: usize,
    buf: *mut T,
    stat: FileStat,
) -> SysResult {
    write_user_value(token, buf, &stat.into())?;
    Ok(0)
}

fn stat_by_dirfd(dirfd: isize) -> SysResult<FileStat> {
    if dirfd == AT_FDCWD {
        return Ok(stat_at(current_process().working_dir(), ".", true)?);
    }
    if dirfd < 0 {
        return Err(SysError::EBADF);
    }
    Ok(get_file_by_fd(dirfd as usize)?.stat())
}

pub(super) fn resolve_stat(
    dirfd: isize,
    path: &str,
    follow_final_symlink: bool,
) -> SysResult<FileStat> {
    if path.is_empty() {
        return stat_by_dirfd(dirfd);
    }
    let is_absolute = path.starts_with('/');
    if !is_absolute && dirfd != AT_FDCWD && dirfd >= 0 {
        let file = get_file_by_fd(dirfd as usize)?;
        if file.is_devfs_dir() {
            let stat = if file.is_devfs_misc_dir() {
                stat_devfs_misc_child(path)
            } else {
                stat_devfs_child(path)
            };
            return stat.ok_or(SysError::ENOENT);
        }
    }
    let base = if is_absolute {
        WorkingDir::root()
    } else {
        dirfd_base(dirfd)?
    };
    Ok(stat_at(base, path, follow_final_symlink)?)
}

pub fn sys_fstat(fd: usize, statbuf: *mut LinuxKstat) -> SysResult {
    if statbuf.is_null() {
        return Err(SysError::EFAULT);
    }
    let token = current_user_token();
    let file = get_file_by_fd(fd)?;
    write_stat_result(token, statbuf, file.stat())
}

pub fn sys_newfstatat(
    dirfd: isize,
    pathname: *const u8,
    statbuf: *mut LinuxKstat,
    flags: i32,
) -> SysResult {
    if statbuf.is_null() || pathname.is_null() {
        return Err(SysError::EFAULT);
    }
    if flags & !VALID_FSTATAT_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }

    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return Err(SysError::ENOENT);
    }
    let follow_final_symlink = flags & AT_SYMLINK_NOFOLLOW == 0;
    write_stat_result(
        token,
        statbuf,
        resolve_stat(dirfd, path.as_str(), follow_final_symlink)?,
    )
}

pub fn sys_statfs(pathname: *const u8, statfsbuf: *mut LinuxStatfs) -> SysResult {
    if statfsbuf.is_null() || pathname.is_null() {
        return Err(SysError::EFAULT);
    }
    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    if path.is_empty() {
        return Err(SysError::ENOENT);
    }
    let stat = resolve_stat(AT_FDCWD, path.as_str(), true)?;
    let fs_stat = statfs_for_mount(MountId(stat.dev as usize)).ok_or(SysError::ENOSYS)?;
    write_user_value(token, statfsbuf, &LinuxStatfs::from(fs_stat))?;
    Ok(0)
}

pub fn sys_statx(
    dirfd: isize,
    pathname: *const u8,
    flags: i32,
    mask: u32,
    statxbuf: *mut LinuxStatx,
) -> SysResult {
    if statxbuf.is_null() || pathname.is_null() {
        return Err(SysError::EFAULT);
    }
    if flags & !VALID_STATX_FLAGS != 0 || mask & STATX_RESERVED != 0 {
        return Err(SysError::EINVAL);
    }

    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return Err(SysError::ENOENT);
    }
    let follow_final_symlink = flags & AT_SYMLINK_NOFOLLOW == 0;
    write_stat_result(
        token,
        statxbuf,
        resolve_stat(dirfd, path.as_str(), follow_final_symlink)?,
    )
}
