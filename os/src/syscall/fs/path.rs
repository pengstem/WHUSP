use crate::fs::{
    File, OpenFlags, WorkingDir, lookup_dir_at, mkdir_at, normalize_path, open_devfs_child,
    open_file_at, rename_at, rmdir_at, unlink_file_at,
};
use crate::mm::UserBuffer;
use crate::task::{FdTableEntry, current_process, current_user_token};
use alloc::sync::Arc;

use super::super::errno::{SysError, SysResult};
use super::fd::get_file_by_fd;
use super::uapi::{
    AT_FDCWD, AT_REMOVEDIR, RENAME_EXCHANGE, RENAME_NOREPLACE, RENAME_WHITEOUT, VALID_RENAME_FLAGS,
};
use super::user_ptr::{
    PATH_MAX, UserBufferAccess, read_user_c_string, translated_byte_buffer_checked,
};

pub(super) fn dirfd_base(dirfd: isize) -> SysResult<WorkingDir> {
    if dirfd == AT_FDCWD {
        return Ok(current_process().working_dir());
    }
    if dirfd < 0 {
        return Err(SysError::EBADF);
    }
    let file = get_file_by_fd(dirfd as usize)?;
    file.working_dir().ok_or(SysError::ENOTDIR)
}

fn path_base(dirfd: isize, path: &str) -> SysResult<WorkingDir> {
    if path.starts_with('/') {
        Ok(WorkingDir::root())
    } else {
        dirfd_base(dirfd)
    }
}

fn copy_c_string_to_user(ptr: *mut u8, buf_len: usize, string: &str) -> SysResult {
    let total_len = string.len() + 1;
    if buf_len < total_len {
        return Err(SysError::ERANGE);
    }
    let token = current_user_token();
    let mut written = 0usize;
    let buffers = translated_byte_buffer_checked(
        token,
        ptr.cast_const(),
        total_len,
        UserBufferAccess::Write,
    )?;
    for byte_ref in UserBuffer::new(buffers) {
        unsafe {
            *byte_ref = if written < string.len() {
                string.as_bytes()[written]
            } else {
                0
            };
        }
        written += 1;
    }
    Ok(ptr as isize)
}

fn install_open_file(file: Arc<dyn File + Send + Sync>, flags: OpenFlags) -> SysResult {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let fd = inner.alloc_fd();
    inner.fd_table[fd] = Some(FdTableEntry::from_file(file, flags));
    Ok(fd as isize)
}

fn open_devfs_child_from_dirfd(
    dirfd: isize,
    path: &str,
    flags: OpenFlags,
) -> SysResult<Option<Arc<dyn File + Send + Sync>>> {
    if path.starts_with('/') || dirfd == AT_FDCWD || dirfd < 0 {
        return Ok(None);
    }
    let file = get_file_by_fd(dirfd as usize)?;
    if !file.is_devfs_dir() {
        return Ok(None);
    }
    open_devfs_child(path, flags)?
        .map(Some)
        .ok_or(SysError::ENOENT)
}

pub fn sys_openat(dirfd: isize, path: *const u8, flags: u32, _mode: u32) -> SysResult {
    let token = current_user_token();
    let path = read_user_c_string(token, path, PATH_MAX)?;
    let Some(flags) = OpenFlags::from_bits(flags) else {
        return Err(SysError::EINVAL);
    };
    if flags.bits() & 0b11 == 0b11 {
        return Err(SysError::EINVAL);
    }
    if let Some(file) = open_devfs_child_from_dirfd(dirfd, path.as_str(), flags)? {
        return install_open_file(file, flags);
    }
    let base = path_base(dirfd, path.as_str())?;
    let file = open_file_at(base, path.as_str(), flags)?;
    install_open_file(file, flags)
}

pub fn sys_chdir(path: *const u8) -> SysResult {
    let process = current_process();
    let token = current_user_token();
    let path = read_user_c_string(token, path, PATH_MAX)?;
    let cwd = process.working_dir();
    let next_cwd = lookup_dir_at(cwd, path.as_str())?;
    let Some(next_path) = normalize_path(&process.working_dir_path(), path.as_str()) else {
        return Err(SysError::ENOENT);
    };
    process.set_working_dir(next_cwd, next_path);
    Ok(0)
}

pub fn sys_getcwd(buf: *mut u8, size: usize) -> SysResult {
    let process = current_process();
    let cwd_path = process.working_dir_path();
    copy_c_string_to_user(buf, size, cwd_path.as_str())
}

pub fn sys_mkdirat(dirfd: isize, path: *const u8, mode: u32) -> SysResult {
    let token = current_user_token();
    let path = read_user_c_string(token, path, PATH_MAX)?;
    let base = path_base(dirfd, path.as_str())?;
    mkdir_at(base, path.as_str(), mode)?;
    Ok(0)
}

pub fn sys_unlinkat(dirfd: isize, path: *const u8, flags: u32) -> SysResult {
    if flags & !AT_REMOVEDIR != 0 {
        return Err(SysError::EINVAL);
    }
    let token = current_user_token();
    let path = read_user_c_string(token, path, PATH_MAX)?;
    let base = path_base(dirfd, path.as_str())?;
    if flags & AT_REMOVEDIR != 0 {
        rmdir_at(base, path.as_str())?;
    } else {
        unlink_file_at(base, path.as_str())?;
    }
    Ok(0)
}

pub fn sys_renameat2(
    olddirfd: isize,
    oldpath: *const u8,
    newdirfd: isize,
    newpath: *const u8,
    flags: u32,
) -> SysResult {
    if flags & !VALID_RENAME_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    if flags & RENAME_EXCHANGE != 0 {
        // UNFINISHED: Linux RENAME_EXCHANGE atomically swaps two existing pathnames.
        // The current EXT4/VFS wrapper only supports one-way rename.
        return Err(SysError::EINVAL);
    }
    if flags & RENAME_WHITEOUT != 0 {
        // UNFINISHED: Linux RENAME_WHITEOUT creates an overlay/union whiteout
        // device while renaming. This kernel has no overlay filesystem support.
        return Err(SysError::EINVAL);
    }

    let token = current_user_token();
    let oldpath = read_user_c_string(token, oldpath, PATH_MAX)?;
    let newpath = read_user_c_string(token, newpath, PATH_MAX)?;
    let old_base = path_base(olddirfd, oldpath.as_str())?;
    let new_base = path_base(newdirfd, newpath.as_str())?;
    rename_at(
        old_base,
        oldpath.as_str(),
        new_base,
        newpath.as_str(),
        flags & RENAME_NOREPLACE != 0,
    )?;
    Ok(0)
}

pub fn sys_getdents64(fd: usize, buf: *mut u8, len: usize) -> SysResult {
    if len == 0 {
        return Err(SysError::EINVAL);
    }
    let token = current_user_token();
    let buffers =
        translated_byte_buffer_checked(token, buf.cast_const(), len, UserBufferAccess::Write)?;
    let process = current_process();
    let inner = process.inner_exclusive_access();
    let Some(file) = inner
        .fd_table
        .get(fd)
        .and_then(|entry| entry.as_ref())
        .map(|entry| entry.file())
    else {
        return Err(SysError::EBADF);
    };
    drop(inner);
    Ok(file.read_dirent64(UserBuffer::new(buffers))?)
}
